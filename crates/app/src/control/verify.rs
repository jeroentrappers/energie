//! The read-back check: what the device's own reading must show once a command took.
//!
//! The expectation is on the World field HA's entity for the command shows (the field the
//! dashboard's control sets, [`crate::entities::world_fields`]), so it holds for every driver of
//! a role and is checked against what the logic itself reads:
//!
//! | Command | Field | Must read |
//! | --- | --- | --- |
//! | `SolisRcMode(m)` | `battery_inverter_rc_mode` | `m` |
//! | `SolisRcChargeW(w)` / `SolisRcDischargeW(w)` | `…_rc_charge_w` / `…_rc_discharge_w` | `round(w / 10) × 10` (ties to even, the register's step) |
//! | `SolisRcTimeoutMin(m)` | `…_rc_timeout_min` | `m` |
//! | `SolisAllowGridCharge(b)` / `SolisFeedInLimit(b)` | `…_allow_grid_charge` / `…_feed_in_limit_on` | `b` |
//! | `SolisBackflowW(w)` | `…_backflow_w` | `round(w / 100) × 100` |
//! | `EvseCurrentDeciA(d)` | `charger_buzz_override_a` | `d / 10` A |
//! | `EvseMode(m)` | `charger_buzz_mode` | `m` |
//! | `CableSetCurrent(a)`, `CableStart { amps: a }` | `charger_ev3_set_current_a` | `a` |
//! | `Airco { unit, on }` | `load_airco_<unit>_on` | `on` |
//!
//! `CableStop`, the reconnects and `RefreshPrices` have no field that shows them: they are
//! logged as sent, not verified.

use energie_core::command::{AircoUnit, Command};
use energie_core::world::World;
use energie_drivers::entity::Value;
use serde_json::json;

use crate::entities::{WorldField, world_fields};
use crate::plug::Section;

/// What a command must show in the World.
#[derive(Clone)]
pub struct Expect {
    pub field: &'static WorldField,
    pub want: Value,
}

impl std::fmt::Debug for Expect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Expect").field("field", &self.key()).field("want", &self.want).finish()
    }
}

impl PartialEq for Expect {
    fn eq(&self, o: &Self) -> bool {
        std::ptr::eq(self.field, o.field) && self.want == o.want
    }
}

impl Expect {
    /// `<slot>_<key>`, the World entity's key.
    pub fn key(&self) -> String {
        format!("{}_{}", self.field.section.slot().key(), self.field.key)
    }

    /// The field's value in `w`.
    pub fn read(&self, w: &World) -> Value {
        (self.field.get)(w)
    }

    /// Whether `w` shows the wanted value (numbers within 0.05, the override's resolution).
    pub fn holds(&self, w: &World) -> bool {
        match (&self.want, self.read(w)) {
            (Value::Number(a), Value::Number(b)) => (a - b).abs() < 0.05,
            (a, b) => *a == b,
        }
    }
}

fn field(section: Section, key: &str) -> &'static WorldField {
    world_fields().iter().find(|f| f.section == section && f.key == key).expect("a World field")
}

/// Python's `round(value / step) * step`.
fn stepped(value: u32, step: f64) -> f64 {
    (f64::from(value) / step).round_ties_even() * step
}

/// The read-back of `cmd`; `None` when nothing in the World shows it.
pub fn expect(cmd: &Command) -> Option<Expect> {
    use Command::*;
    use Section::{Cable, Evse, Solis};
    let text = |v: serde_json::Value| Value::Text(v.as_str().expect("unit variants serialise as text").to_string());
    let (section, key, want) = match *cmd {
        SolisRcMode(m) => (Solis, "rc_mode", text(json!(m))),
        SolisRcChargeW(w) => (Solis, "rc_charge_w", Value::Number(stepped(w, 10.0))),
        SolisRcDischargeW(w) => (Solis, "rc_discharge_w", Value::Number(stepped(w, 10.0))),
        SolisRcTimeoutMin(m) => (Solis, "rc_timeout_min", Value::Number(f64::from(m))),
        SolisAllowGridCharge(on) => (Solis, "allow_grid_charge", Value::Bool(on)),
        SolisFeedInLimit(on) => (Solis, "feed_in_limit_on", Value::Bool(on)),
        SolisBackflowW(w) => (Solis, "backflow_w", Value::Number(stepped(w, 100.0))),
        EvseCurrentDeciA(d) => (Evse, "override_a", Value::Number(f64::from(d) / 10.0)),
        EvseMode(m) => (Evse, "mode", text(json!(m))),
        CableSetCurrent(a) | CableStart { amps: a } => (Cable, "set_current_a", Value::Number(f64::from(a))),
        Airco { unit: AircoUnit::Beneden, on } => (Section::Airco, "beneden_on", Value::Bool(on)),
        Airco { unit: AircoUnit::Zolder, on } => (Section::Airco, "zolder_on", Value::Bool(on)),
        _ => return None,
    };
    Some(Expect { field: field(section, key), want })
}

#[cfg(test)]
mod tests {
    use super::*;
    use energie_core::world::{EvseMode, RcMode};

    /// Each command's read-back, worked out by hand: the registers' steps round half to even
    /// (1505 W → 1500, 1515 W → 1520, 5550 W → 5600), the override is in tenths.
    #[test]
    fn expectations_by_command() {
        let e = |c: Command| {
            let x = expect(&c).unwrap();
            (x.key(), x.want)
        };
        let n = Value::Number;
        assert_eq!(
            e(Command::SolisRcMode(RcMode::ForceCharge)),
            ("battery_inverter_rc_mode".into(), Value::Text("ForceCharge".into()))
        );
        assert_eq!(e(Command::SolisRcChargeW(1505)).1, n(1500.0));
        assert_eq!(e(Command::SolisRcChargeW(1515)).1, n(1520.0));
        assert_eq!(e(Command::SolisRcDischargeW(900)), ("battery_inverter_rc_discharge_w".into(), n(900.0)));
        assert_eq!(e(Command::SolisRcTimeoutMin(10)).1, n(10.0));
        assert_eq!(e(Command::SolisAllowGridCharge(true)).1, Value::Bool(true));
        assert_eq!(e(Command::SolisFeedInLimit(false)).0, "battery_inverter_feed_in_limit_on");
        assert_eq!(e(Command::SolisBackflowW(5550)).1, n(5600.0));
        assert_eq!(e(Command::EvseCurrentDeciA(145)), ("charger_buzz_override_a".into(), n(14.5)));
        assert_eq!(e(Command::EvseMode(EvseMode::Pause)).1, Value::Text("Pause".into()));
        assert_eq!(e(Command::CableStart { amps: 8 }), ("charger_ev3_set_current_a".into(), n(8.0)));
        assert_eq!(e(Command::CableSetCurrent(12)).1, n(12.0));
        assert_eq!(e(Command::Airco { unit: AircoUnit::Beneden, on: true }).0, "load_airco_beneden_on");
        assert_eq!(e(Command::Airco { unit: AircoUnit::Zolder, on: false }).0, "load_airco_zolder_on");
        for c in [Command::CableStop, Command::CableReconnect, Command::SolisReconnect, Command::RefreshPrices] {
            assert!(expect(&c).is_none(), "{c:?}");
        }
    }

    /// The check reads the field from the World: numbers within 0.05, text and bools exactly.
    #[test]
    fn holds_against_the_world() {
        let mut w = World::default();
        let x = expect(&Command::EvseCurrentDeciA(145)).unwrap();
        assert!(!x.holds(&w));
        w.evse.override_a = Some(14.52);
        assert!(x.holds(&w));
        w.evse.override_a = Some(14.6);
        assert!(!x.holds(&w));
        let m = expect(&Command::SolisRcMode(RcMode::None)).unwrap();
        w.solis.rc_mode = Some(RcMode::None);
        assert!(m.holds(&w));
        assert_eq!(m.read(&w), Value::Text("None".into()));
        assert_eq!(m, expect(&Command::SolisRcMode(RcMode::None)).unwrap());
        assert_ne!(m, expect(&Command::SolisRcMode(RcMode::ForceCharge)).unwrap());
        assert_eq!(format!("{m:?}"), r#"Expect { field: "battery_inverter_rc_mode", want: Text("None") }"#);
    }
}
