//! End to end: a simulated day in control mode, through the real gate, device queues, native
//! drivers and wire protocols, against the protocol simulators (Solis Modbus, Tuya 3.4 cable,
//! SmartEVSE on a local MQTT broker), compared write by write with what HA would have written.
//!
//! - **The decisions** come from the closed-loop house model and the engine on a virtual clock
//!   (the oracle run; the engine's decisions are the ones the replay matches against HA's
//!   recorder, `docs/REPLAY.md`). The control run feeds the runtime the same Worlds and must
//!   make the same commands, tick by tick.
//! - **The writes HA would make** for those commands come from [`HaModel`], a line-by-line
//!   transliteration of the HA side: `solis_modbus` (`select.py` companion writes from the
//!   register cache, `number.py` skip-if-equal and `round(value / multiplier)`, `switch.py`
//!   read-modify-write of one bit), `tuya_local` via `script.laadkabel_start_laden` (select,
//!   3 s, number; `mode: restart`) and `script.laadkabel_stop_laden`, and the automations'
//!   `mqtt.publish` / the SmartEVSE mode select (QoS 0, not retained).
//! - **The writes made** are what the simulators received. With the default `rc_keepalive_s = 0`
//!   they are HA's, write for write. The model's `keepalive` reproduces the opt-in mode-only
//!   keepalive (`rc_keepalive_s > 0`), which the driver tests cover.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use chrono::{Duration, TimeZone};
use energie_core::command::Command;
use energie_core::engine::Engine;
use energie_core::memory::Memory;
use energie_core::settings::Settings;
use energie_core::time::{TZ, Ts};
use energie_core::world::World;
use energie_drivers::sim::mqtt::{Broker, SmartEvseSim};
use energie_drivers::sim::solis::SolisSim;
use energie_drivers::sim::tuya::TuyaSim;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde_json::{Map, Value, json};

use super::{Control, ControlConfig, Verdict};
use crate::config::{Config, Mode, SimConfig};
use crate::drivers::Drivers;
use crate::notify::LogNotifier;
use crate::plug::{Actuator, Device, DriverFactory, LogActuator, Router, Section, Sources, WorldSource};
use crate::runtime::{Parts, Runtime};
use crate::sim::House;

const KEY: &str = "0123456789abcdef";
const PREFIX: &str = "SmartEVSE/1234";

/// HA's side of every command: the integrations' write semantics over HA's register cache.
#[derive(Default)]
struct HaModel {
    regs: BTreeMap<u16, u16>,
    modbus: Vec<(u16, u16)>,
    tuya: Vec<Map<String, Value>>,
    mqtt: Vec<(String, String)>,
    /// `script.laadkabel_start_laden` waiting in its `delay: 3 s`.
    cable_pending: Option<(Ts, u8)>,
    refused: usize,
    /// energie's RC select instead of HA's: a held mode (with a valid timeout) only re-writes
    /// 43135 when it has not written it yet (the day is far shorter than a keepalive period in
    /// wall time); `None` held writes nothing.
    keepalive: bool,
    rc_written: bool,
}

impl HaModel {
    fn write(&mut self, r: u16, v: u16) {
        self.modbus.push((r, v));
        self.regs.insert(r, v);
        self.rc_written |= r == 43135;
    }

    /// `SolisNumberEntity.set_native_value`: `native_value == value` skips; else
    /// `round(value / multiplier)` (Python: ties to even). HA refuses values outside min/max.
    fn number(&mut self, r: u16, value: u32, mult: u32, max: u32, min: u32) {
        if value < min || value > max {
            self.refused += 1;
            return;
        }
        if self.regs.get(&r).map(|raw| u32::from(*raw) * mult) == Some(value) {
            return;
        }
        let raw = (f64::from(value) / f64::from(mult)).round_ties_even() as u16;
        self.write(r, raw);
    }

    /// `SolisBinaryEntity.set_register_bit`: from the cache, written only when it changes.
    fn bit(&mut self, r: u16, bit: u8, on: bool) {
        let cur = self.regs[&r];
        let new = if on { cur | 1 << bit } else { cur & !(1 << bit) };
        if new != cur {
            self.write(r, new);
        }
    }

    fn tuya(&mut self, dps: Value) {
        self.tuya.push(dps.as_object().unwrap().clone());
    }

    /// The script's delayed `number.set_value` once its 3 s are over.
    fn flush(&mut self, t: Ts) {
        if let Some((at, a)) = self.cable_pending
            && at <= t
        {
            self.cable_pending = None;
            self.tuya(json!({"114": a}));
        }
    }

    fn command(&mut self, t: Ts, cmd: &Command) {
        use energie_core::world::RcMode;
        self.flush(t);
        match *cmd {
            Command::SolisRcMode(m) => {
                let on = match m {
                    RcMode::None => 0,
                    RcMode::ForceCharge => 1,
                    RcMode::ForceDischarge => 2,
                };
                let held = self.regs.get(&43135) == Some(&on);
                if self.keepalive && held && self.regs.get(&43282).is_some_and(|t| (1..=30).contains(t)) {
                    if m != RcMode::None && !self.rc_written {
                        self.write(43135, on);
                    }
                    return;
                }
                self.write(43135, on);
                for c in [43136, 43129, 43282] {
                    if let Some(v) = self.regs.get(&c).copied() {
                        self.write(c, v);
                    }
                }
            }
            Command::SolisRcChargeW(w) => self.number(43136, w, 10, 655_350, 0),
            Command::SolisRcDischargeW(w) => self.number(43129, w, 10, 655_350, 0),
            Command::SolisRcTimeoutMin(m) => self.number(43282, m, 1, 30, 1),
            Command::SolisBackflowW(w) => self.number(43074, w, 100, 20_000, 0),
            Command::SolisAllowGridCharge(on) => self.bit(43110, 5, on),
            Command::SolisFeedInLimit(on) => self.bit(43073, 4, on),
            Command::EvseCurrentDeciA(d) => self.mqtt.push((format!("{PREFIX}/Set/CurrentOverride"), d.to_string())),
            Command::EvseMode(m) => {
                self.mqtt.push((format!("{PREFIX}/Set/Mode"), energie_drivers::smartevse::mode_name(m).into()))
            }
            Command::CableStart { amps } => {
                self.tuya(json!({"124": "OpenCharging"}));
                // `mode: restart`: a running start is replaced; its current is never set.
                self.cable_pending = (6..=16).contains(&amps).then(|| (t + Duration::seconds(3), amps));
                if !(6..=16).contains(&amps) {
                    self.refused += 1;
                }
            }
            Command::CableStop => self.tuya(json!({"124": "CloseCharging"})),
            Command::CableSetCurrent(a) if (6..=16).contains(&a) => self.tuya(json!({"114": a})),
            Command::CableSetCurrent(_) => self.refused += 1,
            _ => {}
        }
    }

    /// The device writes so far, counting a start's pending current (the drivers in this
    /// harness wait 1 ms instead of 3 s).
    fn counts(&self) -> (usize, usize, usize) {
        (self.modbus.len(), self.tuya.len() + usize::from(self.cable_pending.is_some()), self.mqtt.len())
    }
}

/// The HA model itself, on the commands the day does not send: the feed-in switch and the
/// export limit, values HA refuses, a start that replaces a pending one.
#[test]
fn ha_model_by_hand() {
    let t0 = TZ.with_ymd_and_hms(2026, 6, 21, 12, 0, 0).unwrap();
    let mut ha = HaModel::default();
    ha.regs.extend([(43073, 16), (43074, 55)]);
    for c in [
        Command::SolisFeedInLimit(true),
        Command::SolisFeedInLimit(false),
        Command::SolisBackflowW(5500),
        Command::SolisBackflowW(0),
        Command::SolisBackflowW(20_100),
        Command::SolisRcTimeoutMin(31),
        Command::CableSetCurrent(17),
        Command::CableStart { amps: 20 },
        Command::RefreshPrices,
    ] {
        ha.command(t0, &c);
    }
    assert_eq!(ha.modbus, vec![(43073, 0), (43074, 0)]);
    assert_eq!(ha.refused, 4);
    ha.command(t0, &Command::CableStart { amps: 8 });
    ha.command(t0 + Duration::seconds(1), &Command::CableStart { amps: 10 });
    ha.flush(t0 + Duration::seconds(4));
    let open = json!({"124": "OpenCharging"}).as_object().unwrap().clone();
    assert_eq!(ha.tuya, vec![open.clone(), open.clone(), open, json!({"114": 10}).as_object().unwrap().clone()]);
}

/// The recorded World of the oracle run, served to the runtime.
struct Recorded(Arc<Mutex<World>>);

impl WorldSource for Recorded {
    fn name(&self) -> &str {
        "recorded"
    }
    fn read(&self, _: Ts) -> World {
        self.0.lock().unwrap().clone()
    }
}

struct Frame {
    t: Ts,
    world: World,
    commands: Vec<Command>,
}

fn is_device(c: &Command) -> bool {
    Device::of(c).is_some()
}

/// The oracle: house model and engine closed loop. A June day; the Buzz plugs in at 09:30, the
/// EV3 at 11:00, both aircos run from 17:30 to 19:30 on top of the evening peak.
fn oracle(start: Ts, step: Duration, hours: i64) -> (Vec<Frame>, Settings, Memory) {
    let cfg =
        SimConfig { buzz_plugged: false, ev3_plugged: false, battery_soc: 35.0, cloudiness: 0.2, ..Default::default() };
    let (settings, memory) = (Settings::default(), Memory::default());
    let mut house = House::new(cfg, start);
    let mut engine = Engine::new(settings.clone(), memory.clone());
    let mut t = start;
    let mut frames = Vec::new();
    while t < start + Duration::hours(hours) {
        t += step;
        let local = t - start;
        house.buzz_plugged = local >= Duration::minutes(9 * 60 + 30);
        house.ev3_plugged = local >= Duration::hours(11);
        let airco = local >= Duration::minutes(17 * 60 + 30) && local < Duration::minutes(19 * 60 + 30);
        if airco {
            house.airco = [true, true];
        }
        house.step(t);
        let world = house.world();
        let out = engine.tick(t, &world);
        for c in &out.commands {
            house.apply(t, c);
        }
        frames.push(Frame { t, world, commands: out.commands });
    }
    (frames, settings, memory)
}

/// Whether `pred` holds within 5 s.
async fn eventually(mut pred: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
    while !pred() && std::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    pred()
}

/// The control run's config: the three controlled devices native, against the simulators.
fn config(solis: &SolisSim, tuya: &TuyaSim, broker: &Broker, dir: &std::path::Path) -> Config {
    let key = dir.join("tuya.key");
    std::fs::write(&key, KEY).unwrap();
    let text = format!(
        "mode = \"control\"\nstate_dir = {dir:?}\n\
         [[device]]\nid = \"solis\"\nkind = \"solis_modbus\"\nrole = \"battery_inverter\"\ncontrol = \"battery_inverter\"\nread_only = false\n\
         address = \"{}\"\nframe_gap_ms = 0\nsettings_every = 1\npoll_ms = 500\n\
         [[device]]\nid = \"cable\"\nkind = \"tuya_cable\"\nrole = \"charger:ev3\"\ncontrol = \"charger:ev3\"\nread_only = false\n\
         address = \"{}\"\ndevice_id = \"bf00000000000000000000\"\nlocal_key_file = {key:?}\nstart_delay_ms = 1\npoll_ms = 500\n\
         [[device]]\nid = \"evse\"\nkind = \"smartevse_mqtt\"\nrole = \"charger:buzz\"\ncontrol = \"charger:buzz\"\nread_only = false\n\
         host = \"127.0.0.1\"\nport = {}\npoll_ms = 500\n\
         [control]\nalarms = false\n",
        solis.addr,
        tuya.addr,
        broker.addr.port()
    );
    toml::from_str(&text).unwrap()
}

/// Publishes on `Set/#` that a new subscriber gets: retained ones. The check publishes one
/// retained probe of its own first, to show it would see one.
async fn retained_set_topics(broker: &Broker) -> Vec<String> {
    let opts = MqttOptions::new("retain-check", "127.0.0.1", broker.addr.port());
    let (client, mut ev) = AsyncClient::new(opts, 10);
    client.publish(format!("{PREFIX}/Set/Probe"), QoS::AtLeastOnce, true, "x").await.unwrap();
    client.subscribe(format!("{PREFIX}/Set/#"), QoS::AtMostOnce).await.unwrap();
    let mut got = Vec::new();
    let _ = tokio::time::timeout(StdDuration::from_millis(400), async {
        loop {
            if let Ok(Event::Incoming(Packet::Publish(p))) = ev.poll().await {
                got.push(p.topic);
            }
        }
    })
    .await;
    got
}

/// A day of decisions carried out by the native drivers: every tick's commands equal the
/// oracle's, nothing is blocked by the gate, and the Modbus registers, Tuya DP sets and MQTT
/// publishes the simulators received are, in order, the ones HA would have written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_day_in_control_mode_writes_what_ha_would() {
    let start = TZ.with_ymd_and_hms(2026, 6, 21, 0, 0, 0).unwrap();
    let (frames, settings, memory) = oracle(start, Duration::seconds(5), 24);

    let solis = SolisSim::start().await;
    let tuya = TuyaSim::start(energie_drivers::tuya::protocol::parse_key(KEY).as_ref().unwrap()).await;
    let broker = Broker::start(None).await;
    let evse = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&solis, &tuya, &broker, dir.path());
    let drivers = Drivers::new(&cfg).unwrap();

    let world = Arc::new(Mutex::new(World::default()));
    let per_section =
        Section::ALL.iter().map(|s| (*s, Arc::new(Recorded(world.clone())) as Arc<dyn WorldSource>)).collect();
    let mut per_device: BTreeMap<Device, Arc<dyn Actuator>> = BTreeMap::new();
    for (d, id) in [(Device::Solis, "solis"), (Device::Cable, "cable"), (Device::Evse, "evse")] {
        per_device.insert(d, drivers.actuator(id).unwrap());
    }
    per_device.insert(Device::Airco, Arc::new(LogActuator));
    per_device.insert(Device::Prices, Arc::new(LogActuator));
    assert!(super::wait_ready(&Router { per_device: per_device.clone() }, StdDuration::from_secs(10)).await.is_empty());
    let (mut rt, _rx) = Runtime::new(Parts {
        mode: Mode::Control,
        engine: Engine::new(settings, memory),
        sources: Sources { per_section },
        router: Router { per_device },
        notifier: Arc::new(LogNotifier),
        ha: None,
        settings_source: crate::config::SettingsSource::Local,
        mqtt: None,
        lockstep: false,
        shadow: None,
        persister: None,
        devices: drivers.devices(),
        publish: Vec::new(),
        history: None,
        control: Some(Control::new(ControlConfig { alarms: false, ..cfg.control.clone() }, dir.path())),
    });

    // HA's cache starts where the inverter is; `ha` is what energie writes, `as_ha` what HA would.
    let mut ha = HaModel::default();
    for r in [43073, 43074, 43110, 43129, 43135, 43136, 43282] {
        ha.regs.insert(r, solis.holding(r).unwrap());
    }
    let mut as_ha = HaModel { regs: ha.regs.clone(), ..HaModel::default() };
    let mut kinds: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_minute: BTreeMap<(Device, i64), u32> = BTreeMap::new();
    for f in &frames {
        *world.lock().unwrap() = f.world.clone();
        rt.step(f.t);
        let snap = rt.snapshot_rx().borrow().clone();
        let mine: Vec<Command> =
            snap.commands.iter().rev().filter(|c| c.at == f.t.to_rfc3339()).map(|c| c.command.clone()).collect();
        let want: Vec<Command> = f.commands.iter().filter(|c| is_device(c)).cloned().collect();
        assert_eq!(mine, want, "decisions at {}", f.t);
        assert!(snap.commands.iter().filter(|c| c.at == f.t.to_rfc3339()).all(|c| c.error.is_none()), "at {}", f.t);
        for c in &want {
            ha.command(f.t, c);
            as_ha.command(f.t, c);
            let name = format!("{c:?}");
            *kinds.entry(name.split(['(', ' ']).next().unwrap().to_string()).or_default() += 1;
            *per_minute.entry((Device::of(c).unwrap(), f.t.timestamp() / 60)).or_default() += 1;
        }
        if want.is_empty() {
            continue;
        }
        let (m, t, q) = ha.counts();
        let settled = eventually(|| {
            rt.control_unreported() == 0
                && solis.writes().len() == m
                && tuya.sets().len() == t
                && evse.received().len() == q
        });
        assert!(settled.await, "the drivers at {}", f.t);
    }
    ha.flush(start + Duration::days(2));

    let modbus = solis.writes();
    let sets: Vec<Map<String, Value>> = tuya.sets().into_iter().map(|(_, s)| s).collect();
    assert_eq!(modbus, ha.modbus, "Modbus writes");
    assert_eq!(sets, ha.tuya, "Tuya DP sets");
    assert_eq!(evse.received(), ha.mqtt, "MQTT publishes");
    let retained = retained_set_topics(&broker).await;
    assert_eq!(retained, [format!("{PREFIX}/Set/Probe")], "only the probe is retained");

    // The day exercised every controlled command kind, and the gate never had to step in.
    for k in [
        "SolisRcMode",
        "SolisRcChargeW",
        "SolisRcDischargeW",
        "SolisRcTimeoutMin",
        "SolisAllowGridCharge",
        "EvseCurrentDeciA",
        "EvseMode",
        "CableStart",
        "CableSetCurrent",
    ] {
        assert!(kinds.get(k).is_some_and(|n| *n > 0), "{k} never sent: {kinds:?}");
    }
    let c = rt.control.as_ref().unwrap();
    assert_eq!(c.stats.blocked, 0);
    let busiest = per_minute.values().max().copied().unwrap_or(0);
    assert!(busiest < ControlConfig::default().max_commands_per_min, "{busiest} commands in one minute");
    assert!(dir.path().join("control-audit.jsonl").exists());
    assert_eq!(
        c.stats.sent + c.stats.errors + c.pending() as u64,
        kinds.values().sum::<usize>() as u64,
        "every command audited"
    );
    assert_eq!(c.stats.errors as usize, ha.refused, "the driver refuses exactly what HA refuses");
    assert!(c.view().recent.iter().all(|e| e.result != Verdict::Error || e.detail.is_some()));
    assert_eq!(modbus.len(), as_ha.modbus.len(), "the default keeps HA's RC selects");
    let per_register = |w: &[(u16, u16)]| {
        w.iter().fold(BTreeMap::<u16, usize>::new(), |mut m, (r, _)| {
            *m.entry(*r).or_default() += 1;
            m
        })
    };
    eprintln!("Modbus writes per register: {:?}, HA {:?}", per_register(&modbus), per_register(&as_ha.modbus));
    eprintln!(
        "control day: {} ticks, {} Modbus writes (HA {}), {} DP sets, {} publishes, busiest minute {busiest}, kinds {kinds:?}, {:?}",
        frames.len(),
        modbus.len(),
        as_ha.modbus.len(),
        sets.len(),
        ha.mqtt.len(),
        c.stats
    );
}
