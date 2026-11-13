//! The driver kinds the binary knows, by `kind = "…"` name.
//!
//! Two kinds are bridges the app serves itself: `ha` (the Home Assistant mirror: every World
//! section, commands as service calls) and `sim` (the house model). Every other kind is an
//! [`energie_drivers::plugin::Factory`] of `energie-drivers` or `energie-cloud`;
//! [`Registry::builtin`] is where a new crate of kinds is added (see `docs/PLUGINS.md`).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use energie_drivers::plugin::{Factory, Slot};

use crate::plug::Section;

/// One registered kind.
pub enum Kind {
    /// The Home Assistant bridge.
    Ha,
    /// The built-in house model.
    Sim,
    /// A native driver.
    Native(Box<dyn Factory>),
}

impl Kind {
    /// The slots an instance can read. The bridges read every World section.
    pub fn slots(&self) -> Vec<Slot> {
        match self {
            Kind::Ha | Kind::Sim => Section::ALL.iter().map(|s| s.slot()).collect(),
            Kind::Native(f) => f.slots().to_vec(),
        }
    }

    /// Whether it can carry out commands.
    pub fn writable(&self) -> bool {
        match self {
            Kind::Ha | Kind::Sim => true,
            Kind::Native(f) => f.writable(),
        }
    }

    pub fn is_bridge(&self) -> bool {
        !matches!(self, Kind::Native(_))
    }
}

/// The kinds by name.
pub struct Registry {
    kinds: BTreeMap<&'static str, Kind>,
}

impl Registry {
    /// Only the two bridges.
    pub fn bridges() -> Registry {
        Registry { kinds: BTreeMap::from([("ha", Kind::Ha), ("sim", Kind::Sim)]) }
    }

    /// Every kind compiled into the binary.
    pub fn builtin() -> &'static Registry {
        static R: OnceLock<Registry> = OnceLock::new();
        R.get_or_init(|| {
            let mut r = Registry::bridges();
            for f in energie_drivers::kinds::all() {
                r.add(f);
            }
            for f in energie_cloud::kinds() {
                r.add(f);
            }
            r
        })
    }

    /// Register a kind. Two kinds with one name are a programming error.
    pub fn add(&mut self, f: Box<dyn Factory>) {
        let name = f.name();
        assert!(self.kinds.insert(name, Kind::Native(f)).is_none(), "driver kind {name} registered twice");
    }

    pub fn get(&self, name: &str) -> Option<&Kind> {
        self.kinds.get(name)
    }

    /// The registered names, sorted.
    pub fn names(&self) -> Vec<&'static str> {
        self.kinds.keys().copied().collect()
    }

    /// The native factory of `name` (the caller resolved the kind before).
    pub fn factory(&self, name: &str) -> Option<&dyn Factory> {
        match self.get(name)? {
            Kind::Native(f) => Some(f.as_ref()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The binary knows the two bridges and every driver kind of `energie-drivers` and
    /// `energie-cloud`.
    #[test]
    fn builtin_kinds() {
        let r = Registry::builtin();
        assert_eq!(
            r.names(),
            [
                "cupra_data_act",
                "energy_charts",
                "fluvius",
                "forecast_solar",
                "ha",
                "homewizard",
                "http_json",
                "kia_connect",
                "lg_thinq",
                "omnik",
                "sim",
                "smartevse_mqtt",
                "solis_modbus",
                "static",
                "tuya_cable",
                "tuya_evcharger"
            ]
        );
        assert!(r.get("ha").unwrap().is_bridge() && r.get("sim").unwrap().is_bridge());
        assert!(r.get("ha").unwrap().writable() && r.get("sim").unwrap().writable());
        assert_eq!(r.get("sim").unwrap().slots().len(), Section::ALL.len());
        assert!(!r.get("sim").unwrap().slots().contains(&Slot::SolarForecast));
        let omnik = r.get("omnik").unwrap();
        assert!(!omnik.is_bridge() && !omnik.writable());
        assert_eq!(omnik.slots(), [Slot::Pv]);
        assert!(r.factory("omnik").is_some());
        assert!(r.factory("ha").is_none() && r.factory("wasmachine").is_none());
    }

    /// Registering a name twice is refused.
    #[test]
    #[should_panic(expected = "registered twice")]
    fn duplicate_kinds_are_refused() {
        let mut r = Registry::bridges();
        r.add(energie_drivers::plugin::factory::<energie_drivers::kinds::OmnikKind>());
        r.add(energie_drivers::plugin::factory::<energie_drivers::kinds::OmnikKind>());
    }
}
