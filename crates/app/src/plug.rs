//! The plug-in points: what a source of readings and an actuator must implement, and how the
//! runtime picks one per World section and per device.
//!
//! The HA bridge (`ha`), the house model (`sim`) and the native drivers (`drivers`, over
//! `energie-drivers`) all implement the same two traits:
//!
//! - [`WorldSource`]: called once per tick, returns the latest readings as a `World`. Only the
//!   sections the source is selected for are copied into the tick's world ([`assemble`]), so a
//!   driver only has to fill the sections it knows.
//! - [`Actuator`]: receives the commands for the devices it is selected for. `submit` must not
//!   block: a driver queues the command for its own I/O task.
//!
//! A [`DriverFactory`] hands the runtime the native device instances by id; each of them is
//! also a [`DeviceView`] (its own readings, entities and health). Which instance fills which
//! section and carries out which device's commands is the config's role binding
//! ([`crate::plan`]): sections and devices map to role slots with [`Section::slot`] and
//! [`Device::slot`]. Several PV instances are summed by [`PvSum`].

use std::collections::BTreeMap;
use std::sync::Arc;

use energie_core::command::Command;
use energie_core::time::Ts;
use energie_core::world::World;
use energie_drivers::entity::{Entity, Value};
use energie_drivers::plugin::Slot;
use serde::{Deserialize, Serialize};

use crate::control::Ticket;
use crate::error::{Error, Result};
use energie_drivers::{Applied, Outcome, Write};

/// One section of the [`World`], each fed by one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Section {
    P1,
    Solis,
    Omnik,
    Evse,
    Cable,
    Ev3,
    Buzz,
    Prices,
    Airco,
    Vriezer,
    Fluvius,
}

impl Section {
    pub const ALL: [Section; 11] = [
        Section::P1,
        Section::Solis,
        Section::Omnik,
        Section::Evse,
        Section::Cable,
        Section::Ev3,
        Section::Buzz,
        Section::Prices,
        Section::Airco,
        Section::Vriezer,
        Section::Fluvius,
    ];

    /// Copy this section from `src` into `dst`.
    pub fn copy(self, dst: &mut World, src: &World) {
        match self {
            Section::P1 => dst.p1 = src.p1.clone(),
            Section::Solis => dst.solis = src.solis.clone(),
            Section::Omnik => dst.omnik = src.omnik.clone(),
            Section::Evse => dst.evse = src.evse.clone(),
            Section::Cable => dst.cable = src.cable.clone(),
            Section::Ev3 => dst.ev3 = src.ev3.clone(),
            Section::Buzz => dst.buzz = src.buzz.clone(),
            Section::Prices => dst.prices = src.prices.clone(),
            Section::Airco => dst.airco = src.airco.clone(),
            Section::Vriezer => dst.vriezer = src.vriezer.clone(),
            Section::Fluvius => dst.fluvius = src.fluvius.clone(),
        }
    }

    /// The role slot this section holds.
    pub fn slot(self) -> Slot {
        match self {
            Section::P1 => Slot::GridMeter,
            Section::Solis => Slot::BatteryInverter,
            Section::Omnik => Slot::Pv,
            Section::Evse => Slot::ChargerBuzz,
            Section::Cable => Slot::ChargerEv3,
            Section::Ev3 => Slot::CarEv3,
            Section::Buzz => Slot::CarBuzz,
            Section::Prices => Slot::PriceFeed,
            Section::Airco => Slot::LoadAirco,
            Section::Vriezer => Slot::LoadVriezer,
            Section::Fluvius => Slot::CapacityPeak,
        }
    }

    /// The section a slot fills; `None` for a slot the World has no field for (solar forecast).
    pub fn of_slot(slot: Slot) -> Option<Section> {
        Section::ALL.into_iter().find(|s| s.slot() == slot)
    }
}

/// A device that commands are routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Device {
    /// The SmartEVSE (Buzz).
    Evse,
    /// The Kolanky Tuya cable (EV3).
    Cable,
    /// The Solis hybrid inverter.
    Solis,
    /// The two LG airco units.
    Airco,
    /// The day-ahead price fetcher.
    Prices,
}

impl Device {
    pub const ALL: [Device; 5] = [Device::Evse, Device::Cable, Device::Solis, Device::Airco, Device::Prices];

    /// The device a command is for; `None` for notifications (they go to the notifier).
    pub fn of(cmd: &Command) -> Option<Device> {
        use Command::*;
        match cmd {
            EvseCurrentDeciA(_) | EvseMode(_) => Some(Device::Evse),
            CableStart { .. } | CableStop | CableSetCurrent(_) | CableReconnect => Some(Device::Cable),
            SolisRcMode(_)
            | SolisRcChargeW(_)
            | SolisRcDischargeW(_)
            | SolisRcTimeoutMin(_)
            | SolisAllowGridCharge(_)
            | SolisFeedInLimit(_)
            | SolisBackflowW(_)
            | SolisReconnect => Some(Device::Solis),
            Airco { .. } => Some(Device::Airco),
            RefreshPrices => Some(Device::Prices),
            Notify { .. } | Persistent { .. } => None,
        }
    }

    /// The role slot whose controller carries out this device's commands.
    pub fn slot(self) -> Slot {
        match self {
            Device::Evse => Slot::ChargerBuzz,
            Device::Cable => Slot::ChargerEv3,
            Device::Solis => Slot::BatteryInverter,
            Device::Airco => Slot::LoadAirco,
            Device::Prices => Slot::PriceFeed,
        }
    }

    /// The device whose commands a slot takes; `None` for a slot that takes none.
    pub fn of_slot(slot: Slot) -> Option<Device> {
        Device::ALL.into_iter().find(|d| d.slot() == slot)
    }

    /// Its name in the config (`[control] devices`) and the entities (`control_<name>`).
    pub fn name(self) -> &'static str {
        match self {
            Device::Evse => "evse",
            Device::Cable => "cable",
            Device::Solis => "solis",
            Device::Airco => "airco",
            Device::Prices => "prices",
        }
    }
}

/// How a native driver is doing, for the dashboard and `/api/state`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DriverHealth {
    /// The cache holds a reading (false before the first read and after the grace period).
    pub available: bool,
    /// Wall-clock time of the last successful read (RFC 3339).
    pub last_ok: Option<String>,
    pub reads_ok: u64,
    pub read_errors: u64,
    /// Failed reads since the last good one.
    pub consecutive_errors: u32,
    pub last_error: Option<String>,
    /// An actuator was built for this device.
    pub writable: bool,
    pub commands_applied: u64,
    /// Commands that failed on the device or could not be queued.
    pub command_errors: u64,
    pub last_command_error: Option<String>,
    /// The sign-in state, for a driver that logs in with a password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<energie_drivers::AuthStatus>,
}

/// A source of device readings.
pub trait WorldSource: Send + Sync {
    /// A short name for logs and the dashboard.
    fn name(&self) -> &str;
    /// The latest readings at `now`. Sections this source does not know stay at their default.
    fn read(&self, now: Ts) -> World;
    /// Driver health, for native drivers.
    fn health(&self) -> Option<DriverHealth> {
        None
    }
}

/// Carries out commands for one or more devices.
pub trait Actuator: Send + Sync {
    fn name(&self) -> &str;
    /// Queue `cmd`. Must not block the tick; an `Err` is logged and shown on the dashboard.
    fn submit(&self, now: Ts, cmd: &Command) -> Result<()>;
    /// Queue `cmd` for control mode and report its outcome on `ticket`: with `dry`, only what
    /// it would write. The default (bridges, the log actuator) submits it and reports no wire
    /// writes, or for a dry run names the command and submits nothing.
    fn submit_tracked(&self, now: Ts, cmd: &Command, dry: bool, ticket: Ticket) -> Result<()> {
        if dry {
            ticket.finish(Ok(Outcome { applied: Applied::Done, writes: vec![Write::command(cmd)] }));
            return Ok(());
        }
        if !ticket.allowed() {
            ticket.finish(Err("blocked: control is switched off".into()));
            return Ok(());
        }
        self.submit(now, cmd)?;
        ticket.finish(Ok(Outcome { applied: Applied::Done, writes: Vec::new() }));
        Ok(())
    }
    /// Driver health, for native drivers.
    fn health(&self) -> Option<DriverHealth> {
        None
    }
}

/// One native device instance as the dashboard, the entity registry and the history see it.
pub trait DeviceView: Send + Sync {
    /// The `[[device]]` id.
    fn id(&self) -> &str;
    fn kind(&self) -> &str;
    /// The slots it reads.
    fn slots(&self) -> &[Slot];
    /// Its own latest readings (only its slots' sections are filled).
    fn world(&self) -> World;
    /// What the driver exposes beyond its typed reading ([`energie_drivers::Source::entities`]).
    fn extra(&self) -> Vec<Entity>;
    fn health(&self) -> DriverHealth;
    /// Its own settable entities can be set now (`read_only = false`, outside shadow mode).
    fn settable(&self) -> bool {
        false
    }
    /// The driver's forecasts from its last good read ([`energie_drivers::Source::forecasts`]).
    fn forecasts(&self) -> Vec<(String, Vec<(i64, f64)>)> {
        Vec::new()
    }
    /// Queue setting its entity `key` ([`energie_drivers::Actuator::set`]); must not block.
    fn set(&self, _key: &str, _value: &Value) -> Result<()> {
        Err(Error::new(format!("{}: entities cannot be set", self.id())))
    }
    /// Queue new credentials ([`energie_drivers::Source::set_credentials`]); the receiver gets
    /// the driver's answer. Must not block.
    fn set_credentials(
        &self,
        _fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<crate::drivers::CredentialsReply> {
        Err(Error::new(format!("{}: takes no credentials", self.id())))
    }
}

/// Hands out the native device instances, by `[[device]]` id: [`crate::drivers::Drivers`] in
/// the binary, [`NoDrivers`] where none are wanted.
pub trait DriverFactory {
    fn source(&self, id: &str) -> Option<Arc<dyn WorldSource>>;
    fn actuator(&self, id: &str) -> Option<Arc<dyn Actuator>>;
    /// Every instance, for entities and health.
    fn devices(&self) -> Vec<Arc<dyn DeviceView>>;
}

/// No native drivers: every native device in the config fails at start.
pub struct NoDrivers;

impl DriverFactory for NoDrivers {
    fn source(&self, _: &str) -> Option<Arc<dyn WorldSource>> {
        None
    }
    fn actuator(&self, _: &str) -> Option<Arc<dyn Actuator>> {
        None
    }
    fn devices(&self) -> Vec<Arc<dyn DeviceView>> {
        Vec::new()
    }
}

/// The source of a section nobody reads (standalone, no device for its slot): every field
/// `None`, HA's `unavailable`.
pub struct Unavailable;

impl WorldSource for Unavailable {
    fn name(&self) -> &str {
        "unavailable"
    }
    fn read(&self, _now: Ts) -> World {
        World::default()
    }
}

/// Several PV sources as one: `omnik.power_w` is the sum of the parts that have a value
/// (`None` only when none has; HA's `states(x)|float(0)` per inverter). The other sections stay
/// at their default. Built only for two or more parts, so one PV source is read as before.
pub struct PvSum {
    name: String,
    parts: Vec<Arc<dyn WorldSource>>,
}

impl PvSum {
    pub fn new(parts: Vec<Arc<dyn WorldSource>>) -> Self {
        let name = parts.iter().map(|p| p.name()).collect::<Vec<_>>().join("+");
        PvSum { name, parts }
    }
}

impl WorldSource for PvSum {
    fn name(&self) -> &str {
        &self.name
    }
    fn read(&self, now: Ts) -> World {
        let vals: Vec<f64> = self.parts.iter().filter_map(|p| p.read(now).omnik.power_w).collect();
        let mut w = World::default();
        w.omnik.power_w = (!vals.is_empty()).then(|| vals.iter().sum());
        w
    }
}

/// The selected source per section; sources are shared when one serves several sections.
#[derive(Clone)]
pub struct Sources {
    pub per_section: BTreeMap<Section, Arc<dyn WorldSource>>,
}

impl Sources {
    /// Read every distinct source once and assemble the tick's world.
    pub fn assemble(&self, now: Ts) -> World {
        let mut cache: Vec<(*const (), World)> = Vec::new();
        let mut w = World::default();
        for (section, src) in &self.per_section {
            let key = Arc::as_ptr(src) as *const ();
            let idx = match cache.iter().position(|(k, _)| *k == key) {
                Some(i) => i,
                None => {
                    cache.push((key, src.read(now)));
                    cache.len() - 1
                }
            };
            section.copy(&mut w, &cache[idx].1);
        }
        w
    }

    /// `section → source name`, for the dashboard.
    pub fn describe(&self) -> BTreeMap<Section, String> {
        self.per_section.iter().map(|(s, src)| (*s, src.name().to_string())).collect()
    }

    /// `source name → health` of the native drivers among the sources.
    pub fn health(&self) -> BTreeMap<String, DriverHealth> {
        self.per_section.values().filter_map(|s| s.health().map(|h| (s.name().to_string(), h))).collect()
    }
}

/// The selected actuator per device.
#[derive(Clone)]
pub struct Router {
    pub per_device: BTreeMap<Device, Arc<dyn Actuator>>,
}

impl Router {
    /// Send `cmd` to its device's actuator. Notifications are not routed here.
    pub fn route(&self, now: Ts, cmd: &Command) -> Result<()> {
        let dev = Device::of(cmd).ok_or_else(|| Error::new("notifications go to the notifier"))?;
        let act = self.per_device.get(&dev).ok_or_else(|| Error::new(format!("no actuator for {dev:?}")))?;
        act.submit(now, cmd)
    }

    /// [`Router::route`] for control mode: the outcome goes to `ticket`.
    pub fn route_tracked(&self, now: Ts, cmd: &Command, dry: bool, ticket: Ticket) -> Result<()> {
        let dev = Device::of(cmd).ok_or_else(|| Error::new("notifications go to the notifier"))?;
        let act = self.per_device.get(&dev).ok_or_else(|| Error::new(format!("no actuator for {dev:?}")))?;
        act.submit_tracked(now, cmd, dry, ticket)
    }

    /// The health of `d`'s actuator, when it is a native driver.
    pub fn device_health(&self, d: Device) -> Option<DriverHealth> {
        self.per_device.get(&d).and_then(|a| a.health())
    }

    /// The name of `d`'s actuator (`none` without one).
    pub fn name(&self, d: Device) -> String {
        self.per_device.get(&d).map_or("none".into(), |a| a.name().to_string())
    }

    pub fn describe(&self) -> BTreeMap<Device, String> {
        self.per_device.iter().map(|(d, a)| (*d, a.name().to_string())).collect()
    }

    /// `actuator name → health` of the native drivers among the actuators.
    pub fn health(&self) -> BTreeMap<String, DriverHealth> {
        self.per_device.values().filter_map(|a| a.health().map(|h| (a.name().to_string(), h))).collect()
    }
}

/// Logs every command and does nothing: the shadow-mode actuator.
pub struct LogActuator;

impl Actuator for LogActuator {
    fn name(&self) -> &str {
        "log"
    }
    fn submit(&self, _now: Ts, cmd: &Command) -> Result<()> {
        tracing::info!(?cmd, "command (not actuated)");
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use energie_core::command::AircoUnit;
    use energie_core::world::{EvseMode as Em, RcMode};

    pub(crate) fn t0() -> Ts {
        use chrono::TimeZone;
        energie_core::time::TZ.with_ymd_and_hms(2026, 11, 7, 13, 46, 0).unwrap()
    }

    /// A source that counts its reads and returns a fixed world.
    pub(crate) struct Fixed(pub World, pub AtomicUsize);
    impl WorldSource for Fixed {
        fn name(&self) -> &str {
            "fixed"
        }
        fn read(&self, _: Ts) -> World {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.clone()
        }
    }

    /// An actuator that records what it got.
    #[derive(Default)]
    pub(crate) struct Recorder(pub Mutex<Vec<Command>>);
    impl Actuator for Recorder {
        fn name(&self) -> &str {
            "recorder"
        }
        fn submit(&self, _: Ts, cmd: &Command) -> Result<()> {
            self.0.lock().unwrap().push(cmd.clone());
            Ok(())
        }
    }

    /// Each section is taken from its own source; a source serving several sections is read
    /// once per tick.
    #[test]
    fn assemble_takes_each_section_from_its_source_and_reads_shared_sources_once() {
        let mut a = World::default();
        a.p1.power_w = Some(100.0);
        a.solis.soc = Some(1.0);
        let mut b = World::default();
        b.p1.power_w = Some(200.0);
        b.solis.soc = Some(99.0);
        let sa = Arc::new(Fixed(a, AtomicUsize::new(0)));
        let sb = Arc::new(Fixed(b, AtomicUsize::new(0)));
        let mut per_section: BTreeMap<Section, Arc<dyn WorldSource>> = BTreeMap::new();
        for s in Section::ALL {
            per_section.insert(s, sb.clone());
        }
        per_section.insert(Section::P1, sa.clone());
        let src = Sources { per_section };
        let w = src.assemble(t0());
        assert_eq!(w.p1.power_w, Some(100.0));
        assert_eq!(w.solis.soc, Some(99.0));
        assert_eq!(sa.1.load(Ordering::SeqCst), 1);
        assert_eq!(sb.1.load(Ordering::SeqCst), 1);
        assert_eq!(src.describe()[&Section::P1], "fixed");
    }

    /// Copying every section reproduces the source world exactly.
    #[test]
    fn copying_all_sections_reproduces_the_world() {
        let mut src = World::default();
        src.p1.power_w = Some(1.0);
        src.solis.soc = Some(2.0);
        src.omnik.power_w = Some(3.0);
        src.evse.override_a = Some(4.0);
        src.cable.current_a = Some(5.0);
        src.ev3.soc = Some(6.0);
        src.buzz.soc = Some(7.0);
        src.prices.afname_now = Some(8.0);
        src.airco.zolder_on = Some(true);
        src.vriezer.power_w = Some(9.0);
        src.fluvius.peak_kw = Some(10.0);
        let mut dst = World::default();
        for s in Section::ALL {
            s.copy(&mut dst, &src);
        }
        assert_eq!(dst, src);
    }

    /// Every command maps to the device that carries it out; notifications to none.
    #[test]
    fn commands_route_to_their_device() {
        use Command::*;
        let cases = [
            (EvseCurrentDeciA(60), Some(Device::Evse)),
            (EvseMode(Em::Normal), Some(Device::Evse)),
            (CableStart { amps: 6 }, Some(Device::Cable)),
            (CableStop, Some(Device::Cable)),
            (CableSetCurrent(7), Some(Device::Cable)),
            (CableReconnect, Some(Device::Cable)),
            (SolisRcMode(RcMode::None), Some(Device::Solis)),
            (SolisRcChargeW(1), Some(Device::Solis)),
            (SolisRcDischargeW(1), Some(Device::Solis)),
            (SolisRcTimeoutMin(1), Some(Device::Solis)),
            (SolisAllowGridCharge(true), Some(Device::Solis)),
            (SolisFeedInLimit(true), Some(Device::Solis)),
            (SolisBackflowW(1), Some(Device::Solis)),
            (SolisReconnect, Some(Device::Solis)),
            (Airco { unit: AircoUnit::Zolder, on: true }, Some(Device::Airco)),
            (RefreshPrices, Some(Device::Prices)),
            (Command::notify("a", "b"), None),
            (Command::persistent("a", "b"), None),
        ];
        for (c, d) in cases {
            assert_eq!(Device::of(&c), d, "{c:?}");
        }
    }

    /// The router hands a command to its device's actuator and refuses what it cannot route.
    #[test]
    fn router_delivers_to_the_selected_actuator() {
        let rec = Arc::new(Recorder::default());
        let mut per_device: BTreeMap<Device, Arc<dyn Actuator>> = BTreeMap::new();
        per_device.insert(Device::Evse, rec.clone());
        per_device.insert(Device::Solis, Arc::new(LogActuator));
        let r = Router { per_device };
        r.route(t0(), &Command::EvseCurrentDeciA(145)).unwrap();
        r.route(t0(), &Command::SolisReconnect).unwrap();
        assert!(r.route(t0(), &Command::CableStop).unwrap_err().0.contains("Cable"));
        assert!(r.route(t0(), &Command::notify("a", "b")).is_err());
        assert_eq!(*rec.0.lock().unwrap(), vec![Command::EvseCurrentDeciA(145)]);
        assert_eq!(r.describe()[&Device::Solis], "log");
    }

    /// Without the drivers crate, no driver is available.
    #[test]
    fn no_drivers_provides_nothing() {
        assert!(NoDrivers.source("p1").is_none());
        assert!(NoDrivers.actuator("evse").is_none());
        assert!(NoDrivers.devices().is_empty());
        assert_eq!(Recorder::default().name(), "recorder");
    }

    /// Every section and every command device has its own slot, and maps back from it.
    #[test]
    fn sections_and_devices_map_to_slots() {
        for s in Section::ALL {
            assert_eq!(Section::of_slot(s.slot()), Some(s));
        }
        for d in Device::ALL {
            assert_eq!(Device::of_slot(d.slot()), Some(d));
        }
        assert_eq!(Section::of_slot(Slot::SolarForecast), None);
        assert_eq!(Device::of_slot(Slot::GridMeter), None);
        assert_eq!(Section::Omnik.slot(), Slot::Pv);
        assert_eq!(Device::Evse.slot(), Slot::ChargerBuzz);
    }

    /// LOGIC: PV sources are summed; an unavailable part counts as nothing, all unavailable is
    /// unavailable; only the PV field is filled.
    #[test]
    fn pv_sum_adds_the_parts_that_have_a_value() {
        let pv = |w: Option<f64>| {
            let mut x = World::default();
            x.omnik.power_w = w;
            x.p1.power_w = Some(1.0);
            Arc::new(Fixed(x, AtomicUsize::new(0))) as Arc<dyn WorldSource>
        };
        let sum = PvSum::new(vec![pv(Some(489.0)), pv(Some(1250.5)), pv(None)]);
        assert_eq!(sum.name(), "fixed+fixed+fixed");
        let w = sum.read(t0());
        assert_eq!(w.omnik.power_w, Some(1739.5));
        assert_eq!(w.p1.power_w, None);
        assert_eq!(PvSum::new(vec![pv(None), pv(None)]).read(t0()).omnik.power_w, None);
    }
}
