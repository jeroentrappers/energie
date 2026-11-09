//! The plug-in contract: a driver *kind* is registered once, an *instance* of it is declared in
//! the config (`[[device]]`) with the *roles* it fills.
//!
//! - [`Role`]: what a device is for the control logic (grid meter, battery inverter, PV, ...).
//! - [`Slot`]: a role as the logic consumes it. The logic is written for two named chargers,
//!   two named cars and two named loads, so those roles have one slot per name
//!   (`charger:buzz`, `charger:ev3`, ...); every other role has one slot. Each slot carries one
//!   typed [`Reading`] variant (a `World` sub-struct), so swapping the P1 meter brand is a
//!   config change as long as the new driver returns a `Reading::P1`.
//! - [`DriverKind`]: what a new driver implements: its config table, the slots it can fill, and
//!   a constructor. [`factory`] turns it into a [`Factory`] the app's registry stores.
//!
//! See `docs/PLUGINS.md` for the walkthrough.

use std::marker::PhantomData;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use serde::de::DeserializeOwned;

use crate::driver::{Actuator, Reading, Source};
use crate::error::{DriverError, Result};

/// What a device does for the control logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    GridMeter,
    BatteryInverter,
    /// A PV inverter; several are summed.
    Pv,
    Charger,
    Car,
    /// A switchable load (airco, freezer).
    Load,
    PriceFeed,
    SolarForecast,
    /// The grid operator's capacity peak (Fluvius).
    CapacityPeak,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::GridMeter => "grid_meter",
            Role::BatteryInverter => "battery_inverter",
            Role::Pv => "pv",
            Role::Charger => "charger",
            Role::Car => "car",
            Role::Load => "load",
            Role::PriceFeed => "price_feed",
            Role::SolarForecast => "solar_forecast",
            Role::CapacityPeak => "capacity_peak",
        }
    }
}

/// A role as the control logic consumes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    GridMeter,
    BatteryInverter,
    Pv,
    /// The SmartEVSE-style charger of the Buzz (`Reading::Evse`).
    ChargerBuzz,
    /// The cable-style charger of the EV3 (`Reading::Cable`).
    ChargerEv3,
    CarBuzz,
    CarEv3,
    LoadAirco,
    LoadVriezer,
    PriceFeed,
    SolarForecast,
    CapacityPeak,
}

impl Slot {
    pub const ALL: [Slot; 12] = [
        Slot::GridMeter,
        Slot::BatteryInverter,
        Slot::Pv,
        Slot::ChargerBuzz,
        Slot::ChargerEv3,
        Slot::CarBuzz,
        Slot::CarEv3,
        Slot::LoadAirco,
        Slot::LoadVriezer,
        Slot::PriceFeed,
        Slot::SolarForecast,
        Slot::CapacityPeak,
    ];

    pub fn role(self) -> Role {
        match self {
            Slot::GridMeter => Role::GridMeter,
            Slot::BatteryInverter => Role::BatteryInverter,
            Slot::Pv => Role::Pv,
            Slot::ChargerBuzz | Slot::ChargerEv3 => Role::Charger,
            Slot::CarBuzz | Slot::CarEv3 => Role::Car,
            Slot::LoadAirco | Slot::LoadVriezer => Role::Load,
            Slot::PriceFeed => Role::PriceFeed,
            Slot::SolarForecast => Role::SolarForecast,
            Slot::CapacityPeak => Role::CapacityPeak,
        }
    }

    /// The config spelling: the role name, plus `:<slot>` for the named slots.
    pub fn name(self) -> &'static str {
        match self {
            Slot::ChargerBuzz => "charger:buzz",
            Slot::ChargerEv3 => "charger:ev3",
            Slot::CarBuzz => "car:buzz",
            Slot::CarEv3 => "car:ev3",
            Slot::LoadAirco => "load:airco",
            Slot::LoadVriezer => "load:vriezer",
            s => s.role().name(),
        }
    }

    /// The topic-safe spelling (`charger_buzz`), used in entity keys.
    pub fn key(self) -> String {
        self.name().replace(':', "_")
    }

    /// Parse a config spelling.
    pub fn parse(s: &str) -> Option<Slot> {
        Slot::ALL.into_iter().find(|x| x.name() == s)
    }

    /// The slots of a role.
    pub fn of_role(r: Role) -> Vec<Slot> {
        Slot::ALL.into_iter().filter(|s| s.role() == r).collect()
    }

    /// Several instances may fill this slot; their values are summed (PV).
    pub fn summed(self) -> bool {
        self == Slot::Pv
    }

    /// The slot's typed data with every field `None` (HA's `unavailable`). This names the
    /// [`Reading`] variant a driver filling the slot must return.
    pub fn unavailable(self) -> Reading {
        use energie_core::world as w;
        match self {
            Slot::GridMeter => Reading::P1(w::P1::default()),
            Slot::BatteryInverter => Reading::Solis(w::Solis::default()),
            Slot::Pv => Reading::Omnik(w::Omnik::default()),
            Slot::ChargerBuzz => Reading::Evse(w::Evse::default()),
            Slot::ChargerEv3 => Reading::Cable(w::Cable::default()),
            Slot::CarBuzz => Reading::Buzz(w::Buzz::default()),
            Slot::CarEv3 => Reading::Ev3(w::Ev3::default()),
            Slot::LoadAirco => Reading::Airco(w::Airco::default()),
            Slot::LoadVriezer => Reading::Vriezer(w::Vriezer::default()),
            Slot::PriceFeed => Reading::Prices(w::Prices::default()),
            Slot::SolarForecast => Reading::Empty,
            Slot::CapacityPeak => Reading::Fluvius(w::Fluvius::default()),
        }
    }
}

/// A driver: something that reads and (maybe) takes commands. Every [`Source`] that is also an
/// [`Actuator`] is one; a read-only device answers every command with `Applied::NotMine`.
pub trait Driver: Source + Actuator {}
impl<T: Source + Actuator> Driver for T {}

/// What an instance is built with besides its own table.
#[derive(Debug, Clone, Copy)]
pub struct BuildCx<'a> {
    /// The instance id (`p1`, `pv_garage`).
    pub id: &'a str,
    /// The slots the config binds it to (a kind that can fill several picks its reading by it).
    pub slots: &'a [Slot],
    /// The app's state dir (credentials captured at run time go to `credentials/<id>.json`).
    pub state_dir: Option<&'a std::path::Path>,
}

/// A driver kind: implement this once per device type, then register [`factory::<YourKind>()`]
/// in the app's registry.
pub trait DriverKind: 'static {
    /// The kind's own config table: the `[[device]]` keys besides the common ones (`id`,
    /// `kind`, `role`, `control`, `name`, `poll_ms`, `grace_polls`, `read_only`). Use
    /// `#[serde(deny_unknown_fields)]` so a typo is a config error.
    type Config: DeserializeOwned;
    /// `kind = "…"` in the config.
    const NAME: &'static str;
    /// The slots an instance can fill.
    const SLOTS: &'static [Slot];
    /// Whether it carries out commands (a `control` binding needs this).
    const WRITABLE: bool = false;
    /// The default poll period, ms.
    const POLL_MS: u64 = 1000;

    /// Cheap checks for `--check-config` (addresses, ranges): no I/O, no secret files.
    fn check(_cfg: &Self::Config) -> Result<()> {
        Ok(())
    }

    /// Construct the driver. Runs inside a tokio runtime; may read secret files and spawn tasks.
    fn build(cfg: Self::Config, cx: &BuildCx) -> Result<Box<dyn Driver>>;
}

/// A registered kind, type-erased (what the app's registry holds).
pub trait Factory: Send + Sync {
    fn name(&self) -> &'static str;
    fn slots(&self) -> &'static [Slot];
    fn writable(&self) -> bool;
    fn poll_ms(&self) -> u64;
    /// Parse and check an instance table.
    fn check(&self, table: &serde_json::Value) -> Result<()>;
    fn build(&self, table: &serde_json::Value, cx: &BuildCx) -> Result<Box<dyn Driver>>;
}

struct Of<K>(PhantomData<fn() -> K>);

fn parse<K: DriverKind>(table: &serde_json::Value) -> Result<K::Config> {
    serde_json::from_value(table.clone()).map_err(|e| DriverError::Config(e.to_string()))
}

impl<K: DriverKind> Factory for Of<K> {
    fn name(&self) -> &'static str {
        K::NAME
    }
    fn slots(&self) -> &'static [Slot] {
        K::SLOTS
    }
    fn writable(&self) -> bool {
        K::WRITABLE
    }
    fn poll_ms(&self) -> u64 {
        K::POLL_MS
    }
    fn check(&self, table: &serde_json::Value) -> Result<()> {
        K::check(&parse::<K>(table)?)
    }
    fn build(&self, table: &serde_json::Value, cx: &BuildCx) -> Result<Box<dyn Driver>> {
        let cfg = parse::<K>(table)?;
        K::check(&cfg)?;
        K::build(cfg, cx)
    }
}

/// The registry entry for kind `K`.
pub fn factory<K: DriverKind>() -> Box<dyn Factory> {
    Box::new(Of::<K>(PhantomData))
}

/// Read a secret file (token, password, key) and trim surrounding whitespace.
pub fn read_secret(path: &Path) -> Result<String> {
    let s = std::fs::read_to_string(path).map_err(|e| DriverError::Config(format!("{}: {e}", path.display())))?;
    Ok(s.trim().to_string())
}

/// `ip:port`, or a bare `ip` with `port`. Host names are not resolved.
pub fn socket(s: &str, port: u16) -> Result<SocketAddr> {
    s.parse::<SocketAddr>()
        .or_else(|_| s.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, port)))
        .map_err(|_| DriverError::Config(format!("address: {s:?} is not an IP address (ip or ip:port)")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Applied;
    use async_trait::async_trait;
    use energie_core::command::Command;
    use serde::Deserialize;
    use serde_json::json;

    /// Every slot spells and parses back; the named slots belong to their role; only PV sums.
    #[test]
    fn slots_spell_parse_and_belong_to_their_role() {
        for s in Slot::ALL {
            assert_eq!(Slot::parse(s.name()), Some(s));
            assert!(Slot::of_role(s.role()).contains(&s));
            assert_eq!(s.summed(), s == Slot::Pv);
            assert!(crate::entity::is_topic_safe(&s.key()), "{s:?}");
        }
        assert_eq!(Slot::of_role(Role::Charger), vec![Slot::ChargerBuzz, Slot::ChargerEv3]);
        assert_eq!(Slot::of_role(Role::Load), vec![Slot::LoadAirco, Slot::LoadVriezer]);
        assert_eq!(Slot::of_role(Role::Car), vec![Slot::CarBuzz, Slot::CarEv3]);
        assert_eq!(Slot::ChargerBuzz.key(), "charger_buzz");
        assert_eq!(Slot::parse("charger"), None, "a role with named slots needs the slot");
        let names: Vec<_> = [
            Role::GridMeter,
            Role::BatteryInverter,
            Role::Pv,
            Role::PriceFeed,
            Role::SolarForecast,
            Role::CapacityPeak,
        ]
        .map(Role::name)
        .into();
        assert_eq!(names, ["grid_meter", "battery_inverter", "pv", "price_feed", "solar_forecast", "capacity_peak"]);
        assert_eq!([Role::Charger, Role::Car, Role::Load].map(Role::name), ["charger", "car", "load"]);
    }

    /// Each slot names the reading it takes, and that reading lands in the slot's World field.
    #[test]
    fn each_slot_takes_one_reading() {
        let mut w = energie_core::world::World::default();
        w.p1.power_w = Some(1.0);
        for s in Slot::ALL {
            s.unavailable().apply_to(&mut w);
        }
        assert_eq!(w, energie_core::world::World::default());
        assert_eq!(Slot::SolarForecast.unavailable(), Reading::Empty);
        assert!(matches!(Slot::CapacityPeak.unavailable(), Reading::Fluvius(_)));
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EchoCfg {
        value: f64,
    }

    struct Echo(f64);

    #[async_trait]
    impl Source for Echo {
        fn name(&self) -> &'static str {
            "echo"
        }
        async fn read(&mut self) -> Result<Reading> {
            Ok(Reading::Omnik(energie_core::world::Omnik { power_w: Some(self.0) }))
        }
        fn unavailable(&self) -> Reading {
            Slot::Pv.unavailable()
        }
    }

    #[async_trait]
    impl Actuator for Echo {
        async fn apply(&mut self, _: &Command) -> Result<Applied> {
            Ok(Applied::NotMine)
        }
    }

    struct EchoKind;

    impl DriverKind for EchoKind {
        type Config = EchoCfg;
        const NAME: &'static str = "echo";
        const SLOTS: &'static [Slot] = &[Slot::Pv];
        fn check(cfg: &EchoCfg) -> Result<()> {
            if cfg.value < 0.0 { Err(DriverError::Config("value must be >= 0".into())) } else { Ok(()) }
        }
        fn build(cfg: EchoCfg, cx: &BuildCx) -> Result<Box<dyn Driver>> {
            assert_eq!(cx.slots, [Slot::Pv]);
            Ok(Box::new(Echo(cfg.value)))
        }
    }

    /// A kind becomes a factory: defaults for the optional constants, its table parsed with
    /// unknown fields refused, its own check run before building.
    #[tokio::test]
    async fn a_kind_becomes_a_factory() {
        let f = factory::<EchoKind>();
        assert_eq!((f.name(), f.slots(), f.writable(), f.poll_ms()), ("echo", &[Slot::Pv][..], false, 1000));
        f.check(&json!({"value": 1.0})).unwrap();
        let typo = f.check(&json!({"valeu": 1.0})).unwrap_err().to_string();
        assert!(typo.contains("unknown field `valeu`"), "{typo}");
        assert!(f.check(&json!({"value": -1.0})).unwrap_err().to_string().contains(">= 0"));
        let cx = BuildCx { id: "pv2", slots: &[Slot::Pv], state_dir: None };
        assert!(f.build(&json!({"value": -1.0}), &cx).is_err(), "build checks too");
        assert!(f.build(&json!({}), &cx).err().unwrap().to_string().contains("missing field `value`"));
        let mut d = f.build(&json!({"value": 7.0}), &cx).unwrap();
        assert_eq!(d.read().await.unwrap(), Reading::Omnik(energie_core::world::Omnik { power_w: Some(7.0) }));
        assert_eq!(d.unavailable(), Slot::Pv.unavailable());
        assert_eq!(d.name(), "echo");
        assert_eq!(d.apply(&Command::RefreshPrices).await.unwrap(), Applied::NotMine);
    }

    /// Secrets are trimmed; a missing file names its path. Addresses are IPs with an optional
    /// port; host names are refused.
    #[test]
    fn secrets_and_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("token");
        std::fs::write(&p, "  abc\n").unwrap();
        assert_eq!(read_secret(&p).unwrap(), "abc");
        let missing = dir.path().join("x");
        assert!(read_secret(&missing).unwrap_err().to_string().contains(&*missing.to_string_lossy()));
        assert_eq!(socket("192.168.1.20", 502).unwrap(), "192.168.1.20:502".parse().unwrap());
        assert_eq!(socket("192.168.1.20:1502", 502).unwrap(), "192.168.1.20:1502".parse().unwrap());
        assert!(socket("omvormer.lan", 502).unwrap_err().to_string().contains("address: \"omvormer.lan\""));
    }
}
