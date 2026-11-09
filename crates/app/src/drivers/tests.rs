//! The driver factory against the protocol simulators of `energie-drivers`. Nothing here talks
//! to a real device: every address is a simulator on `127.0.0.1`.

use std::path::Path;

use chrono::Duration as ChronoDuration;
use energie_core::world::{Omnik as OmnikReading, P1};
use energie_drivers::sim::homewizard::HomeWizardSim;
use energie_drivers::sim::mqtt::{Broker, SmartEvseSim};
use energie_drivers::sim::omnik::OmnikSim;
use energie_drivers::sim::prices::PriceSim;
use energie_drivers::sim::solis::SolisSim;
use energie_drivers::sim::tuya::TuyaSim;
use serde_json::json;

use super::*;
use crate::config::ActuatorKind;
use crate::ha::ws::tests::{fake_ha, recv};
use crate::plug::tests::t0;
use crate::plug::{Device, Section};

/// The instance id a legacy `driver` selection of this section gets (`[devices.<id>]`).
fn sid(s: Section) -> &'static str {
    match s {
        Section::P1 => "homewizard",
        Section::Solis => "solis",
        Section::Omnik => "omnik",
        Section::Evse => "smartevse",
        Section::Cable => "cable",
        Section::Prices => "prices",
        _ => "none",
    }
}

/// The instance id that carries out a device's commands under a legacy selection.
fn did(d: Device) -> &'static str {
    match d {
        Device::Evse => "smartevse",
        Device::Cable => "cable",
        Device::Solis => "solis",
        Device::Prices => "prices",
        Device::Airco => "none",
    }
}

const HW_TOKEN: &str = "made-up-hw-token";
const ENTSOE_TOKEN: &str = "made-up-entsoe-token";
const TUYA_KEY: &[u8; 16] = b"0123456789abcdef";
const MQTT_USER: &str = "energie";
const MQTT_PASSWORD: &str = "made-up-password";

fn write(dir: &Path, name: &str, text: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p.display().to_string()
}

/// One simulator per device, and the secret files that reach them.
struct Rig {
    dir: tempfile::TempDir,
    hw: HomeWizardSim,
    solis: SolisSim,
    omnik: OmnikSim,
    _broker: Broker,
    evse: SmartEvseSim,
    tuya: TuyaSim,
    prices: PriceSim,
}

impl Rig {
    async fn start() -> Rig {
        let broker = Broker::start(Some((MQTT_USER, MQTT_PASSWORD))).await;
        let evse = SmartEvseSim::start(broker.addr, "SmartEVSE/1234", Some((MQTT_USER, MQTT_PASSWORD))).await;
        let prices = PriceSim::start(ENTSOE_TOKEN).await;
        let today = wall_now().date_naive();
        prices.set_day(today, vec![100.0; 100]);
        prices.set_day(today + ChronoDuration::days(1), vec![50.0; 100]);
        let hw = HomeWizardSim::start(HW_TOKEN).await;
        hw.set_measurement(json!({"power_w": 1234.0, "current_l1_a": 1.5, "energy_import_kwh": 100.0}));
        let solis = SolisSim::start().await;
        solis.set_pv(2345);
        let omnik = OmnikSim::start().await;
        omnik.set_power(Some(489));
        Rig {
            dir: tempfile::tempdir().unwrap(),
            hw,
            solis,
            omnik,
            _broker: broker,
            evse,
            tuya: TuyaSim::start(TUYA_KEY).await,
            prices,
        }
    }

    /// `[devices.*]` for every simulator.
    fn devices(&self, read_only: bool) -> String {
        let d = self.dir.path();
        let hw = self.hw.config(HW_TOKEN);
        format!(
            "[devices.homewizard]\naddress = \"{hw_addr}\"\nserial = \"aabbccddeeff\"\ntoken_file = \"{hw_token}\"\nca_file = \"{ca}\"\npoll_ms = 50\ngrace_polls = 2\n\
             [devices.solis]\naddress = \"{solis}\"\npoll_ms = 100\nread_only = {read_only}\n\
             [devices.omnik]\nbase_url = \"{omnik}/\"\npoll_ms = 50\n\
             [devices.smartevse]\nhost = \"127.0.0.1\"\nport = {mqtt_port}\nusername = \"{MQTT_USER}\"\npassword_file = \"{mqtt_pw}\"\npoll_ms = 50\nread_only = {read_only}\n\
             [devices.cable]\naddress = \"{tuya}\"\ndevice_id = \"made-up-device\"\nlocal_key_file = \"{tuya_key}\"\npoll_ms = 50\nread_only = {read_only}\n\
             [devices.prices]\nentsoe_token_file = \"{entsoe}\"\nenergy_charts_url = \"{ec}\"\nentsoe_url = \"{eu}\"\npoll_ms = 50\nread_only = {read_only}\n",
            hw_addr = hw.addr,
            hw_token = write(d, "hw-token", &format!("{HW_TOKEN}\n")),
            ca = write(d, "hw-ca.pem", &hw.ca_pem),
            solis = self.solis.addr,
            omnik = self.omnik.base_url(),
            mqtt_port = self._broker.addr.port(),
            mqtt_pw = write(d, "mqtt-password", MQTT_PASSWORD),
            tuya = self.tuya.addr,
            tuya_key = write(d, "tuya-key", std::str::from_utf8(TUYA_KEY).unwrap()),
            entsoe = write(d, "entsoe-token", ENTSOE_TOKEN),
            ec = self.prices.energy_charts_url(),
            eu = self.prices.entsoe_url(),
        )
    }
}

/// Every section that has a driver read from it, every device written through it.
const ALL_DRIVERS: &str = "[sources]\ndefault = \"sim\"\np1 = \"driver\"\nsolis = \"driver\"\nomnik = \"driver\"\nevse = \"driver\"\ncable = \"driver\"\nprices = \"driver\"\n\
     [actuators]\ndefault = \"log\"\nsolis = \"driver\"\nevse = \"driver\"\ncable = \"driver\"\nprices = \"driver\"\n";

/// Wait (at most 15 s: a Solis poll alone takes about 2 s of frame gaps) until `f` holds.
async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let wait = async {
        while !f() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), wait)
        .await
        .unwrap_or_else(|_| panic!("not reached: {what}"));
}

/// Change one key of a legacy `[devices.<name>]` table after parsing.
fn set(cfg: &mut Config, device: &str, key: &str, v: toml::Value) {
    cfg.devices.get_mut(device).unwrap().insert(key.into(), v);
}

fn health(src: &Arc<dyn WorldSource>) -> DriverHealth {
    src.health().expect("a driver source reports health")
}

/// `[devices]` parses with the documented defaults: read only, 3 grace polls, the per-device
/// poll periods; only the tables a selection uses become instances.
#[test]
fn devices_config_defaults() {
    let c = Config::parse(
        "mode = \"simulate\"\n[sources]\np1 = \"driver\"\nsolis = \"driver\"\nomnik = \"driver\"\nevse = \"driver\"\ncable = \"driver\"\nprices = \"driver\"\n\
         [devices.homewizard]\naddress = \"192.168.1.30\"\nserial = \"s\"\ntoken_file = \"t\"\n\
         [devices.solis]\naddress = \"192.168.1.20:502\"\n[devices.omnik]\nbase_url = \"http://192.168.1.40\"\n\
         [devices.smartevse]\nhost = \"192.168.1.10\"\n[devices.cable]\naddress = \"192.168.1.60\"\ndevice_id = \"d\"\nlocal_key_file = \"k\"\n\
         [devices.prices]\n",
    )
    .unwrap();
    let plan = c.plan().unwrap();
    let d = |id: &str| plan.devices.iter().find(|i| i.id == id).unwrap();
    let hw = d("homewizard");
    assert_eq!((hw.poll_ms, hw.grace_polls, hw.table.get("ca_file")), (1000, 3, None));
    assert_eq!((d("solis").poll_ms, d("solis").read_only, d("solis").kind.as_str()), (15_000, true, "solis_modbus"));
    assert_eq!(d("omnik").poll_ms, 30_000);
    assert_eq!(
        (d("smartevse").poll_ms, d("smartevse").read_only, d("smartevse").kind.as_str()),
        (1000, true, "smartevse_mqtt")
    );
    assert_eq!((d("cable").poll_ms, d("cable").read_only, d("cable").kind.as_str()), (1000, true, "tuya_cable"));
    assert_eq!(
        (d("prices").poll_ms, d("prices").read_only, d("prices").kind.as_str()),
        (10_000, true, "energy_charts")
    );
    for s in Section::ALL.into_iter().filter(|s| sid(*s) != "none") {
        assert_eq!(d(sid(s)).reads, [s.slot()], "{s:?}");
    }
    assert!(Config::parse("mode = \"simulate\"\n").unwrap().devices.is_empty());
    let unused = Config::parse("mode = \"simulate\"\n[devices.omnik]\nbase_url = \"http://x\"\n").unwrap();
    assert!(unused.plan().unwrap().devices.is_empty(), "an unused table is checked but not built");
}

/// Addresses are IPs with an optional port; host names are refused.
#[test]
fn addresses_take_a_default_port() {
    let socket = energie_drivers::plugin::socket;
    assert_eq!(socket("192.168.1.20", 502).unwrap(), "192.168.1.20:502".parse().unwrap());
    assert_eq!(socket("192.168.1.20:1502", 502).unwrap(), "192.168.1.20:1502".parse().unwrap());
    let e = Config::parse("mode = \"simulate\"\n[devices.solis]\naddress = \"omvormer.lan\"\n").unwrap_err().0;
    assert!(e.contains("devices.solis: config: address"), "{e}");
}

/// Driver selections are checked against `[devices]` when the config is loaded, with a message
/// that says what to add or change.
#[test]
fn driver_selections_need_their_device() {
    let err = |t: &str| Config::parse(&format!("mode = \"control\"\n[sources]\ndefault = \"sim\"\n{t}")).unwrap_err().0;
    assert!(err("p1 = \"driver\"\n").contains("sources.p1 = \"driver\" needs [devices.homewizard]"));
    assert!(err("ev3 = \"driver\"\n").contains("sources.ev3: there is no native driver"));
    assert!(err("[actuators]\nairco = \"driver\"\n").contains("actuators.airco: there is no native driver"));
    assert!(err("[actuators]\nsolis = \"driver\"\n").contains("actuators.solis = \"driver\" needs [devices.solis]"));
    let ro = err("[actuators]\nevse = \"driver\"\n[devices.smartevse]\nhost = \"h\"\n");
    assert!(ro.contains("devices.smartevse.read_only is true"), "{ro}");
    assert!(err("[devices.omnik]\nbase_url = \"http://x\"\npoll_ms = 0\n").contains("devices.omnik.poll_ms"));
    assert!(
        err("[devices.homewizard]\naddress = \"meter\"\nserial = \"s\"\ntoken_file = \"t\"\n").contains("homewizard")
    );
    assert!(err("[devices.solis]\naddress = \"x\"\n").contains("devices.solis: config: address"));
    assert!(err("[devices.cable]\naddress = \"x\"\ndevice_id = \"d\"\nlocal_key_file = \"k\"\n").contains("cable"));
    assert!(err("[devices.solis]\naddress = \"127.0.0.1\"\nreadonly = false\n").contains("readonly"));
    assert!(err("[devices.wasmachine]\n").contains("wasmachine"));
    // Writable and selected: accepted.
    Config::parse("mode = \"control\"\n[sources]\ndefault = \"sim\"\n[actuators]\nevse = \"driver\"\n[devices.smartevse]\nhost = \"h\"\nread_only = false\n")
        .unwrap();
}

/// LOGIC: a failed read keeps the last good reading for `grace_polls` failures in a row, then the
/// section goes HA-`unavailable` (every field `None`) until a read succeeds again.
#[test]
fn read_errors_keep_the_reading_through_the_grace_period_then_go_unavailable() {
    let unavailable = || Reading::P1(P1::default());
    let good = |w| Ok(Reading::P1(P1 { power_w: Some(w), ..P1::default() }));
    let mut st = State::new(2, false);
    assert!(!st.health.available, "nothing read yet");
    st.on_read("t", good(100.0), unavailable(), t0());
    assert!(st.health.available);
    assert_eq!(st.health.last_ok.as_deref(), Some("2026-11-07T13:46:00+01:00"));
    for n in 1..=2 {
        st.on_read("t", Err(DriverError::Timeout), unavailable(), t0());
        assert_eq!(st.world.p1.power_w, Some(100.0), "failure {n} is within the grace period");
        assert!(st.health.available);
    }
    st.on_read("t", Err(DriverError::Status(500)), unavailable(), t0());
    assert_eq!(st.world.p1.power_w, None, "third failure: unavailable");
    assert!(!st.health.available);
    st.on_read("t", Err(DriverError::Timeout), unavailable(), t0());
    assert_eq!(st.world.p1.power_w, None);
    assert_eq!(
        (st.health.read_errors, st.health.consecutive_errors, st.health.last_error.as_deref()),
        (4, 4, Some("timeout"))
    );
    st.on_read("t", good(200.0), unavailable(), t0());
    assert_eq!(st.world.p1.power_w, Some(200.0));
    assert_eq!((st.health.available, st.health.consecutive_errors, st.health.reads_ok), (true, 0, 2));

    let mut strict = State::new(0, false);
    strict.on_read("t", good(1.0), unavailable(), t0());
    strict.on_read("t", Err(DriverError::Timeout), unavailable(), t0());
    assert_eq!(strict.world.p1.power_w, None, "grace 0: unavailable at the first failure");
}

/// A command the device took counts as applied; a device error or a command for another device
/// counts as an error.
#[test]
fn command_outcomes_are_counted() {
    let mut st = State::new(3, true);
    st.on_apply("t", &Job::Command(Command::CableStop), Ok(Applied::Done));
    st.on_apply("t", &Job::Command(Command::CableStart { amps: 6 }), Ok(Applied::Scheduled));
    st.on_apply("t", &Job::Command(Command::CableStop), Ok(Applied::NotMine));
    assert_eq!(st.health.last_command_error.as_deref(), Some("not a command for this device"));
    st.on_apply("t", &Job::Command(Command::CableStop), Err(DriverError::NotConnected));
    let h = &st.health;
    assert_eq!((h.writable, h.commands_applied, h.command_errors), (true, 2, 2));
    assert_eq!(h.last_command_error.as_deref(), Some("not connected"));
}

/// Built from the config against all six simulators: every source serves its simulator's
/// values, and a command for each writable device reaches its simulator.
#[tokio::test]
async fn factory_reads_and_writes_every_simulator() {
    let rig = Rig::start().await;
    let cfg = Config::parse(&format!("mode = \"control\"\n{ALL_DRIVERS}{}", rig.devices(false))).unwrap();
    let drivers = Drivers::new(&cfg).unwrap();
    assert_eq!(drivers.actuator_count(), 4);
    assert!(drivers.source("ev3").is_none());
    assert!(drivers.actuator("airco").is_none());
    assert_eq!(drivers.devices().len(), 6);
    let src = |s| drivers.source(sid(s)).unwrap();
    until("step 1", || {
        Section::ALL
            .iter()
            .filter_map(|s| drivers.source(sid(*s)))
            .all(|s| s.health().is_some_and(|h| h.available && h.last_ok.is_some()))
    })
    .await;
    let now = t0();
    assert_eq!(src(Section::P1).read(now).p1.power_w, Some(1234.0));
    assert_eq!(src(Section::P1).read(now).p1.phase_a[0], Some(1.5));
    assert_eq!(src(Section::Solis).read(now).solis.pv_power_w, Some(2345.0));
    assert_eq!(src(Section::Omnik).read(now).omnik, OmnikReading { power_w: Some(489.0) });
    assert_eq!(src(Section::Evse).read(now).evse.plug.as_deref(), Some("Connected"));
    assert_eq!(src(Section::Cable).read(now).cable.status.as_deref(), Some("plugged_in"));
    assert!(src(Section::Prices).read(now).prices.afname_now.is_some());
    assert_eq!(src(Section::P1).name(), "driver:homewizard");
    assert!(health(&src(Section::Solis)).writable);
    assert!(!health(&src(Section::P1)).writable);
    let act = |d| drivers.actuator(did(d)).unwrap();
    assert_eq!(act(Device::Solis).name(), "driver:solis");

    act(Device::Evse).submit(now, &Command::EvseCurrentDeciA(100)).unwrap();
    until("step 2", || {
        rig.evse.received().iter().any(|(t, p)| t == "SmartEVSE/1234/Set/CurrentOverride" && p == "100")
    })
    .await;
    act(Device::Solis).submit(now, &Command::SolisRcChargeW(1230)).unwrap();
    until("step 3", || rig.solis.writes().contains(&(43136, 123))).await;
    act(Device::Cable).submit(now, &Command::CableSetCurrent(10)).unwrap();
    until("step 4", || rig.tuya.sets().iter().any(|(_, dps)| dps.get("114") == Some(&json!(10)))).await;
    let hits = rig.prices.hits("energy-charts");
    act(Device::Prices).submit(now, &Command::RefreshPrices).unwrap();
    until("step 5", || rig.prices.hits("energy-charts") > hits).await;
    until("step 6", || act(Device::Solis).health().unwrap().commands_applied == 1).await;
    // A command for another device is refused by the driver and counted.
    act(Device::Solis).submit(now, &Command::CableStop).unwrap();
    until("step 7", || act(Device::Solis).health().unwrap().command_errors == 1).await;
}

/// End to end: the runtime assembles its World from driver sources over the simulators, shows
/// driver health in `/api/state`, and routes a command through the driver to the simulator.
#[tokio::test]
async fn runtime_reads_through_drivers_and_actuates_the_simulator() {
    let rig = Rig::start().await;
    let state = tempfile::tempdir().unwrap();
    let cfg = Config::parse(&format!(
        "mode = \"control\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 50\n{}{}",
        state.path().display(),
        // Only the price feed is written: the engine's own Solis/EVSE commands go to the log, so
        // the routed command below is not queued behind them.
        ALL_DRIVERS.split("[actuators]").next().unwrap(),
        rig.devices(false)
    ))
    .unwrap();
    let mut cfg = cfg;
    cfg.actuators.per_device.insert(Device::Prices, ActuatorKind::Driver);
    let drivers = Drivers::new(&cfg).unwrap();
    let r = crate::app::start(cfg, &drivers).await.unwrap();
    let mut snap = r.rt.lock().unwrap().snapshot_rx();
    until("step 8", || {
        let s = snap.borrow_and_update().clone();
        s.world.p1.power_w == Some(1234.0)
            && s.world.solis.pv_power_w == Some(2345.0)
            && s.world.omnik.power_w == Some(489.0)
            && s.world.evse.plug.is_some()
            && s.world.cable.status.is_some()
            && s.world.prices.afname_now.is_some()
    })
    .await;
    {
        let rt = r.rt.lock().unwrap();
        assert_eq!(rt.sources.describe()[&Section::P1], "driver:homewizard");
        assert_eq!(rt.sources.describe()[&Section::Buzz], "sim");
        assert_eq!(rt.router.describe()[&Device::Prices], "driver:prices");
        assert_eq!(rt.router.describe()[&Device::Solis], "log");
    }
    let hits = rig.prices.hits("energy-charts");
    r.rt.lock().unwrap().router.route(t0(), &Command::RefreshPrices).unwrap();
    until("step 9", || rig.prices.hits("energy-charts") > hits).await;
    until("step 9b", || snap.borrow().drivers["driver:prices"].commands_applied >= 1).await;
    let snap = snap.borrow().clone();
    assert!(snap.drivers["driver:prices"].writable);
    assert!(!snap.drivers["driver:solis"].writable);
    assert!(snap.drivers["driver:homewizard"].available);
    let body = serde_json::to_string(&*snap).unwrap();
    assert!(body.contains("\"drivers\":{\"driver:cable\""), "{body}");
    r.stop().await;
}

/// End to end on the wire: the meter answering 500 keeps the last reading for the grace polls,
/// then the P1 section goes unavailable; it comes back with the first good answer.
#[tokio::test]
async fn failing_meter_goes_unavailable_after_the_grace_period_and_recovers() {
    let rig = Rig::start().await;
    let cfg = Config::parse(&format!(
        "mode = \"control\"\n[sources]\ndefault = \"sim\"\np1 = \"driver\"\n{}",
        rig.devices(true)
    ))
    .unwrap();
    let drivers = Drivers::new(&cfg).unwrap();
    assert_eq!(drivers.actuator_count(), 0);
    let p1 = drivers.source(sid(Section::P1)).unwrap();
    until("step 10", || p1.read(t0()).p1.power_w == Some(1234.0)).await;
    rig.hw.set_status(500);
    until("step 11", || health(&p1).consecutive_errors >= 1).await;
    until("step 12", || p1.read(t0()).p1.power_w.is_none()).await;
    let h = health(&p1);
    assert!(h.consecutive_errors > 2, "grace_polls = 2: unavailable only from the third failure");
    assert!(!h.available);
    assert_eq!(h.last_error.as_deref(), Some("http status 500"));
    assert_eq!(p1.read(t0()).p1, P1::default());
    rig.hw.set_status(200);
    until("step 13", || p1.read(t0()).p1.power_w == Some(1234.0)).await;
    assert!(health(&p1).available);
}

/// SAFETY: in shadow mode no actuator is built, whatever `[actuators]` and `read_only` say; the
/// runtime routes everything to the log actuator and the charger receives nothing, while the
/// driver still feeds the World.
#[tokio::test]
async fn shadow_mode_never_constructs_driver_actuators() {
    let rig = Rig::start().await;
    let (url, _) = fake_ha(json!([]), |_, mut ws| async move { while recv(&mut ws).await.is_some() {} }).await;
    let state = tempfile::tempdir().unwrap();
    let token = write(state.path(), "ha-token", "made-up-ha-token");
    let cfg = Config::parse(&format!(
        "mode = \"shadow\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 50\n[ha]\nurl = \"{url}\"\ntoken_file = \"{token}\"\nstart_wait_s = 5\n\
         [sources]\ndefault = \"ha\"\nevse = \"driver\"\nsolis = \"driver\"\n[actuators]\ndefault = \"driver\"\n{}",
        state.path().display(),
        rig.devices(false)
    ))
    .unwrap();
    let drivers = Drivers::new(&cfg).unwrap();
    assert_eq!(drivers.actuator_count(), 0);
    for d in Device::ALL {
        assert!(drivers.actuator(did(d)).is_none(), "{d:?}");
    }
    assert!(!health(&drivers.source(sid(Section::Solis)).unwrap()).writable);
    let r = crate::app::start(cfg, &drivers).await.unwrap();
    assert!(r.rt.lock().unwrap().router.describe().values().all(|a| a == "log"));
    until("step 14", || r.rt.lock().unwrap().tick >= 5).await;
    until("step 15", || r.rt.lock().unwrap().snapshot_rx().borrow().world.evse.plug.is_some()).await;
    r.stop().await;
    assert!(rig.evse.received().is_empty(), "nothing was published to the charger");
    assert!(rig.solis.writes().is_empty(), "no register was written");
}

/// `read_only = true` makes the factory refuse the actuator even when the actuator is selected
/// (the config check refuses that combination too); the device is still read.
#[tokio::test]
async fn read_only_devices_get_no_actuator() {
    let rig = Rig::start().await;
    let state = tempfile::tempdir().unwrap();
    let mut cfg = Config::parse(&format!(
        "mode = \"control\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n{ALL_DRIVERS}{}",
        state.path().display(),
        rig.devices(false)
    ))
    .unwrap();
    set(&mut cfg, "solis", "read_only", true.into());
    set(&mut cfg, "smartevse", "read_only", true.into());
    let drivers = Drivers::new(&cfg).unwrap();
    assert!(drivers.actuator(did(Device::Solis)).is_none());
    assert!(drivers.actuator(did(Device::Evse)).is_none());
    assert!(drivers.actuator(did(Device::Cable)).is_some());
    let solis = drivers.source(sid(Section::Solis)).unwrap();
    until("step 16", || solis.read(t0()).solis.pv_power_w == Some(2345.0)).await;
    // The runtime cannot start with a selected actuator the factory refused.
    let e = crate::app::start(cfg, &drivers).await.err().unwrap();
    assert!(e.0.contains("no driver available for actuator Evse"), "{}", e.0);
    assert!(rig.solis.writes().is_empty());
}

/// `submit` never blocks: a full queue (the device task is stuck on a stalled inverter) and a
/// stopped driver are errors, counted in the health.
#[tokio::test]
async fn submit_refuses_when_the_queue_is_full_or_the_driver_stopped() {
    let rig = Rig::start().await;
    rig.solis.set_stall(true);
    let cfg = Config::parse(&format!(
        "mode = \"control\"\n[sources]\ndefault = \"sim\"\n[actuators]\nsolis = \"driver\"\n{}",
        rig.devices(false)
    ))
    .unwrap();
    let drivers = Drivers::new(&cfg).unwrap();
    assert!(drivers.source(sid(Section::Solis)).is_none(), "the source is not selected");
    let act = drivers.actuator(did(Device::Solis)).unwrap();
    let results: Vec<_> = (0..QUEUE + 4).map(|_| act.submit(t0(), &Command::SolisRcTimeoutMin(5))).collect();
    let refused: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(refused.len() >= 4);
    assert_eq!(refused[0].0, "driver:solis: command queue full");
    drop(drivers);
    until("step 17", || {
        act.submit(t0(), &Command::SolisRcTimeoutMin(5)).is_err_and(|e| e.0.ends_with("driver task stopped"))
    })
    .await;
    let h = act.health().unwrap();
    assert!(h.command_errors >= 5);
    assert_eq!(h.last_command_error.as_deref(), Some("driver task stopped"));
    assert!(rig.solis.writes().is_empty());
}

/// Building a driver fails on a missing secret file, a bad CA or local key, or a missing device
/// entry; the message names the file or the device.
#[tokio::test]
async fn construction_errors_name_the_culprit() {
    let rig = Rig::start().await;
    let devices = rig.devices(false);
    let d = rig.dir.path().display().to_string();
    let err = |sources: &str, edit: &dyn Fn(&mut Config)| {
        let mut cfg =
            Config::parse(&format!("mode = \"control\"\n[sources]\ndefault = \"sim\"\n{sources}{devices}")).unwrap();
        edit(&mut cfg);
        Drivers::new(&cfg).err().expect(sources).0
    };
    assert!(
        err("p1 = \"driver\"\n", &|c| set(c, "homewizard", "token_file", "/nonexistent/secret".into()))
            .contains("/nonexistent/secret")
    );
    write(
        rig.dir.path(),
        "bad-ca.pem",
        "-----BEGIN CERTIFICATE-----\nZ2VlbiBjZXJ0aWZpY2FhdA==\n-----END CERTIFICATE-----\n",
    );
    assert!(
        err("p1 = \"driver\"\n", &|c| set(c, "homewizard", "ca_file", format!("{d}/bad-ca.pem").into()))
            .contains("devices.homewizard")
    );
    assert!(
        err("evse = \"driver\"\n", &|c| set(c, "smartevse", "password_file", "/nonexistent/pw".into()))
            .contains("/nonexistent/pw")
    );
    write(rig.dir.path(), "short-key", "te-kort");
    assert!(
        err("cable = \"driver\"\n", &|c| set(c, "cable", "local_key_file", format!("{d}/short-key").into()))
            .contains("devices.cable: config: tuya local key")
    );
    assert!(
        err("prices = \"driver\"\n", &|c| set(c, "prices", "entsoe_token_file", "/nonexistent/e".into()))
            .contains("/nonexistent/e")
    );
    assert!(
        err("omnik = \"driver\"\n", &|c| drop(c.devices.remove("omnik")))
            .contains("sources.omnik = \"driver\" needs [devices.omnik]")
    );
}

/// Without any `driver` selection nothing is built and no task runs (no tokio runtime needed).
#[test]
fn nothing_selected_builds_nothing() {
    let cfg = Config::parse("mode = \"simulate\"\n[devices.omnik]\nbase_url = \"http://127.0.0.1:1\"\n").unwrap();
    let drivers = Drivers::new(&cfg).unwrap();
    assert_eq!((drivers.actuator_count(), drivers.tasks.len()), (0, 0));
    assert!(drivers.source(sid(Section::Omnik)).is_none());
}

/// Drivers build with their defaults: prices on the production endpoints, the meter on
/// HomeWizard's own CA. Built only, never polled: no task runs, nothing is fetched.
#[tokio::test]
async fn drivers_build_with_their_defaults() {
    let first = |c: &Config| c.plan().unwrap().devices.remove(0);
    let cfg = Config::parse("mode = \"simulate\"\n[sources]\nprices = \"driver\"\n[devices.prices]\n").unwrap();
    assert!(build(&first(&cfg), Path::new("/var/lib/energie")).is_ok());
    let dir = tempfile::tempdir().unwrap();
    let token = write(dir.path(), "t", HW_TOKEN);
    let hw = Config::parse(&format!(
        "mode = \"simulate\"\n[sources]\np1 = \"driver\"\n[devices.homewizard]\naddress = \"127.0.0.1:1\"\nserial = \"s\"\ntoken_file = \"{token}\"\n"
    ))
    .unwrap();
    assert_eq!(
        build(&first(&hw), Path::new("/var/lib/energie")).unwrap().name(),
        "homewizard",
        "the built-in HomeWizard CA"
    );
    let evse = Config::parse(
        "mode = \"simulate\"\n[sources]\nevse = \"driver\"\n[devices.smartevse]\nhost = \"127.0.0.1\"\nport = 1\nusername = \"u\"\n",
    )
    .unwrap();
    assert_eq!(build(&first(&evse), Path::new("/var/lib/energie")).unwrap().name(), "smartevse");
    // A kind the registry has no driver for (a bridge) cannot be built natively.
    let ha = Instance { kind: "ha".into(), label: "device x".into(), ..first(&evse) };
    assert_eq!(build(&ha, Path::new("/var/lib/energie")).err().unwrap().0, "device x: kind ha has no driver");
}

/// End to end with `[[device]]` tables: two PV instances (an `http_json` endpoint and a
/// `static` stand-in) are summed into the PV section, a stand-in car fills `car:ev3`, the
/// model fills the rest; each instance shows its own entities and health.
#[tokio::test]
async fn device_tables_bind_roles_and_expose_entities() {
    use energie_drivers::entity::Value as V;
    let endpoint = energie_drivers::sim::http::serve(
        axum::Router::new()
            .route("/status", axum::routing::get(|| async { axum::Json(json!({"meters": [{"power": 0.25}]})) })),
    )
    .await;
    let state = tempfile::tempdir().unwrap();
    let cfg = Config::parse(&format!(
        "mode = \"simulate\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 50\n\
         [[device]]\nid = \"garage\"\nkind = \"http_json\"\nrole = \"pv\"\nurl = \"http://{}/status\"\npointer = \"/meters/0/power\"\nscale = 1000.0\npoll_ms = 50\ngrace_polls = 0\n\
         [[device]]\nid = \"tuin\"\nkind = \"static\"\nrole = \"pv\"\nvalues = {{ power_w = 100.0 }}\npoll_ms = 50\n\
         [[device]]\nid = \"ev3\"\nkind = \"static\"\nrole = \"car:ev3\"\nvalues = {{ plugged = false, soc = 61.0 }}\nextra = {{ bron = \"stand-in\" }}\npoll_ms = 50\n\
         [[publish]]\nmatch = \"world_pv_*\"\nmin_interval_s = 10\n",
        state.path().display(),
        endpoint.addr
    ))
    .unwrap();
    assert!(cfg.warnings().is_empty(), "no legacy tables");
    let drivers = Drivers::new(&cfg).unwrap();
    assert_eq!((drivers.devices().len(), drivers.actuator_count()), (3, 0));
    let kinds: Vec<_> = drivers.devices().iter().map(|d| (d.id().to_string(), d.kind().to_string())).collect();
    assert_eq!(kinds[0], ("ev3".to_string(), "static".to_string()));
    let r = crate::app::start(cfg, &drivers).await.unwrap();
    let mut snap = r.rt.lock().unwrap().snapshot_rx();
    until("pv sum", || {
        let s = snap.borrow_and_update().clone();
        s.world.omnik.power_w == Some(350.0) && s.world.ev3.soc == Some(61.0)
    })
    .await;
    let s = snap.borrow().clone();
    assert_eq!(s.sources[&Section::Omnik], "driver:garage+driver:tuin");
    assert_eq!(s.sources[&Section::Ev3], "driver:ev3");
    assert_eq!(s.sources[&Section::P1], "sim");
    assert!(s.drivers["driver:tuin"].available);
    let get = |id: &str| s.entities.iter().find(|e| e.id == id).unwrap_or_else(|| panic!("{id}")).clone();
    assert_eq!(get("world_pv_power_w").value, V::Number(350.0));
    assert_eq!(get("world_pv_power_w").publish.min_interval_s, Some(10.0));
    assert_eq!(get("garage_power_w").value, V::Number(250.0));
    assert_eq!(get("garage_raw").value, V::Number(0.25));
    assert_eq!(get("tuin_power_w").value, V::Number(100.0));
    assert_eq!(get("ev3_soc").value, V::Number(61.0));
    assert_eq!(get("ev3_bron").value, V::Text("stand-in".into()));
    assert_eq!(get("world_car_ev3_plugged").value, V::Bool(false));
    assert!(get("world_charger_buzz_override_a").control.is_some(), "the model carries out the charger's commands");
    // The endpoint going away: the instance goes unavailable, its extras go null, and the sum
    // is the other inverter alone.
    drop(endpoint);
    until("garage down", || {
        let s = snap.borrow_and_update().clone();
        s.world.omnik.power_w == Some(100.0) && s.entities.iter().any(|e| e.id == "garage_raw" && e.value == V::Null)
    })
    .await;
    r.stop().await;
}

/// A writable instance (`read_only = false`, not shadow) takes entity settings through its
/// queue even without a `control` binding; the driver's `set` decides (a `static` stand-in
/// sets nothing, which counts as a command error). A read-only one refuses at once.
#[tokio::test]
async fn device_entities_are_set_through_the_queue() {
    let cfg = |mode: &str, ro: bool| {
        let bridge = if mode == "shadow" { "ha" } else { "sim" };
        Config::parse(&format!(
            "mode = \"{mode}\"\n[ha]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"/nonexistent/t\"\n\
             [[device]]\nid = \"bridge\"\nkind = \"{bridge}\"\nrole = \"*\"\n[[device]]\nid = \"ev3\"\nkind = \"static\"\nrole = \"car:ev3\"\nread_only = {ro}\npoll_ms = 50\n"
        ))
        .unwrap()
    };
    let drivers = Drivers::new(&cfg("simulate", false)).unwrap();
    assert_eq!(drivers.actuator_count(), 0, "no role commands");
    let dev = drivers.devices().remove(0);
    assert!(dev.settable());
    dev.set("ac_limit", &Value::Number(80.0)).unwrap();
    until("set reached the driver", || dev.health().command_errors == 1).await;
    assert_eq!(dev.health().last_command_error.as_deref(), Some("not a command for this device"));
    for (mode, ro) in [("simulate", true), ("shadow", false)] {
        let drivers = Drivers::new(&cfg(mode, ro)).unwrap();
        let dev = drivers.devices().remove(0);
        assert!(!dev.settable(), "{mode} {ro}");
        assert_eq!(dev.set("ac_limit", &Value::Number(80.0)).unwrap_err().0, "driver:ev3: read only");
    }
}

/// End to end with a `forecast_solar` device against a mock of the API: its power forecast
/// becomes the dashboard's PV forecast series, its entities are recorded through an
/// `extra_series` glob, and once the API is gone the instance's forecast is dropped (the store
/// keeps the last one).
#[tokio::test]
async fn the_solar_forecast_reaches_the_history() {
    use energie_drivers::entity::Value as V;
    let body = include_str!("../../../cloud/fixtures/forecast_solar_estimate.json");
    let api = energie_drivers::sim::http::serve(
        axum::Router::new()
            .fallback(move || async move { ([(axum::http::header::CONTENT_TYPE, "application/json")], body) }),
    )
    .await;
    let state = tempfile::tempdir().unwrap();
    let cfg = Config::parse(&format!(
        "mode = \"simulate\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 50\n\
         [history]\nextra_series = [\"forecast_*_kwh\", \"forecast_power_now_w\"]\n\
         [[device]]\nid = \"forecast\"\nkind = \"forecast_solar\"\nrole = \"solar_forecast\"\nbase_url = \"http://{}\"\npoll_interval_s = 0\npoll_ms = 50\ngrace_polls = 0\n",
        state.path().display(),
        api.addr
    ))
    .unwrap();
    let drivers = Drivers::new(&cfg).unwrap();
    let r = crate::app::start(cfg, &drivers).await.unwrap();
    let h = r.history.clone().unwrap();
    let day = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp();
    let want = [(crate::history::series::PV_FORECAST.to_string(), crate::history::tier::Pick::Mean)];
    let fc = || h.lock().unwrap().query(&want, day("2026-11-07T10:00:00Z"), day("2026-11-07T11:00:00Z"), 900).unwrap();
    until("forecast in the store", || fc().series[0].v.iter().all(Option::is_some)).await;
    assert_eq!(fc().series[0].v, vec![Some(1240.0); 4], "the 10:00 point holds until 11:00");
    until("entity series", || h.lock().unwrap().index_of("forecast_energy_today_kwh").is_some()).await;
    assert!(h.lock().unwrap().index_of("forecast_power_now_w").is_some());
    let dev = drivers.devices().remove(0);
    assert_eq!(dev.forecasts()[0].0, "power_w");
    let snap = r.rt.lock().unwrap().snapshot_rx().borrow().clone();
    assert!(snap.entities.iter().any(|e| e.id == "forecast_energy_tomorrow_kwh" && matches!(e.value, V::Number(_))));
    drop(api);
    until("forecast dropped", || dev.forecasts().is_empty()).await;
    assert!(fc().series[0].v.iter().all(Option::is_some));
    r.stop().await;
}

/// Control-mode jobs: a dry run is planned by the driver and writes nothing; a real one is
/// executed and reported with its writes; one whose device was switched off after it was
/// queued is dropped by the device task.
#[tokio::test]
async fn tracked_jobs_plan_execute_or_drop() {
    use std::sync::atomic::AtomicBool;
    let rig = Rig::start().await;
    let cfg = Config::parse(&format!(
        "mode = \"control\"\n[sources]\ndefault = \"sim\"\n[actuators]\nsolis = \"driver\"\n{}",
        rig.devices(false)
    ))
    .unwrap();
    let drivers = Drivers::new(&cfg).unwrap();
    let act = drivers.actuator(did(Device::Solis)).unwrap();
    let ticket = |on: bool| Ticket::new(Arc::new(AtomicBool::new(on)));
    let cmd = Command::SolisRcTimeoutMin(12);
    let (dry, off, real) = (ticket(true), ticket(false), ticket(true));
    act.submit_tracked(t0(), &cmd, true, dry.clone()).unwrap();
    act.submit_tracked(t0(), &cmd, false, off.clone()).unwrap();
    act.submit_tracked(t0(), &cmd, false, real.clone()).unwrap();
    until("three reports", || dry.done() && off.done() && real.done()).await;
    let report = |t: &Ticket| format!("{t:?}");
    assert!(report(&off).contains("done: true"));
    assert_eq!(rig.solis.writes(), vec![(43282, 12)], "only the real one wrote");
    assert_eq!(act.health().unwrap().commands_applied, 1);
}
