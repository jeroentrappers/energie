//! Predicates that the live templates repeat verbatim in many places. Defined once here so
//! every controller reads "is the car plugged", "is power cheap" etc. the same way.

use crate::derived::Derived;
use crate::memory::Memory;
use crate::settings::{NetNiveau, Settings};
use crate::world::World;

/// States HA treats as "no value" for a text sensor.
pub fn is_missing(s: Option<&str>) -> bool {
    matches!(s, None | Some("unavailable" | "unknown" | "none" | ""))
}

/// SmartEVSE plug state, falling back to the latch when the integration blips:
/// `p if p not in ['unavailable','unknown','none',''] else input_text.smartevse_plug_laatst_bekend`.
pub fn buzz_plug_known<'a>(w: &'a World, m: &'a Memory) -> &'a str {
    match w.evse.plug.as_deref() {
        Some(p) if !is_missing(Some(p)) => p,
        _ => m.smartevse_plug_laatst_bekend.as_str(),
    }
}

/// `plug_known == 'Connected' or stateid in ['B', 'C']`.
pub fn buzz_wants(w: &World, m: &Memory) -> bool {
    buzz_plug_known(w, m) == "Connected" || matches!(w.evse.state_id.as_deref(), Some("B" | "C"))
}

/// `is_state('sensor.smartevse_1234_state', 'Charging')`.
pub fn buzz_drawing(w: &World) -> bool {
    w.evse.state.as_deref() == Some("Charging")
}

/// `has_value('number.smartevse_1234_charge_current_override')`.
pub fn buzz_ctrl(w: &World) -> bool {
    w.evse.override_a.is_some()
}

/// `state_attr('number.smartevse_1234_charge_current_override', 'max') | float(32)`.
pub fn buzz_max_a(w: &World) -> f64 {
    w.evse.override_max_a.unwrap_or(32.0)
}

/// Cable status says a car is there, or (cable offline) the car itself says it is plugged:
/// `status in ['plugged_in','charging','charged'] or (status in ['unavailable','unknown'] and
/// binary_sensor.ev3_ev_battery_plug == 'on')`.
pub fn ev3_wants(w: &World) -> bool {
    match w.cable.status.as_deref() {
        Some("plugged_in" | "charging" | "charged") => true,
        None | Some("unavailable" | "unknown") => w.ev3.plugged == Some(true),
        Some(_) => false,
    }
}

/// `has_value('number.ev_charging_cable_charge_current')`.
pub fn ev3_ctrl(w: &World) -> bool {
    w.cable.set_current_a.is_some()
}

/// `states('sensor.ev_charging_cable_vermogen') | float(0) > threshold_kw`.
pub fn ev3_power_above(w: &World, threshold_kw: f64) -> bool {
    w.cable.power_kw.unwrap_or(0.0) > threshold_kw
}

/// `states('sensor.ev_charging_cable_stroom') | float(0) > 0.5`.
pub fn ev3_current_flowing(w: &World) -> bool {
    w.cable.current_a.unwrap_or(0.0) > 0.5
}

/// `state_attr('number.ev_charging_cable_charge_current', 'max') | float(16)`.
pub fn ev3_max_a(w: &World) -> f64 {
    w.cable.set_current_max_a.unwrap_or(16.0)
}

/// The control price: `states('sensor.stroomprijs_nu') | float(states(
/// 'sensor.jeroen_current_electricity_market_price') | float(9))`.
pub fn control_price(w: &World, d: &Derived) -> f64 {
    d.stroomprijs_nu.unwrap_or(w.prices.afname_now.unwrap_or(9.0))
}

/// `price < input_number.goedkoop_laden_prijs_drempel`.
pub fn cheap(w: &World, d: &Derived, s: &Settings) -> bool {
    control_price(w, d) < s.goedkoop_laden_prijs_drempel
}

/// `ev_net_niveau == 'Max tot piek' and cheap`.
pub fn grid_max(w: &World, d: &Derived, s: &Settings) -> bool {
    s.ev_net_niveau == NetNiveau::MaxTotPiek && cheap(w, d, s)
}

/// `ev_net_niveau == '6A-brug' and cheap`.
pub fn grid_bridge(w: &World, d: &Derived, s: &Settings) -> bool {
    s.ev_net_niveau == NetNiveau::Brug6A && cheap(w, d, s)
}

/// Worst conductor in the IMPORT direction: `max([0, fase_n|float(0)]|max for n in 1..3)`.
pub fn maxphase(w: &World) -> f64 {
    w.p1.phase_a.iter().map(|a| a.unwrap_or(0.0).max(0.0)).fold(0.0, f64::max)
}

/// Worst conductor in either direction: `max(fase_n|float(0)|abs)`. Used by `breaker_guard`.
pub fn maxphase_abs(w: &World) -> f64 {
    w.p1.phase_a.iter().map(|a| a.unwrap_or(0.0).abs()).fold(0.0, f64::max)
}

/// `states('sensor.solis_s6_eh1p_battery_power_net_2') | float(0)`: + discharge / − charge.
pub fn batt_w(w: &World) -> f64 {
    w.solis.battery_power_w.unwrap_or(0.0)
}

/// `[0, -batt] | max`: what the battery is charging right now, W.
pub fn batt_charge_w(w: &World) -> f64 {
    (-batt_w(w)).max(0.0)
}

/// `[0, batt] | max`: what the battery is discharging right now, W.
pub fn batt_discharge_w(w: &World) -> f64 {
    batt_w(w).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        World::default()
    }

    #[test]
    fn missing_text_states() {
        for s in [None, Some("unavailable"), Some("unknown"), Some("none"), Some("")] {
            assert!(is_missing(s));
        }
        assert!(!is_missing(Some("Connected")));
    }

    #[test]
    fn buzz_plug_falls_back_to_latch() {
        let mut w = world();
        let m = Memory { smartevse_plug_laatst_bekend: "Connected".into(), ..Memory::default() };
        assert_eq!(buzz_plug_known(&w, &m), "Connected");
        assert!(buzz_wants(&w, &m));
        w.evse.plug = Some("Disconnected".into());
        assert_eq!(buzz_plug_known(&w, &m), "Disconnected");
        assert!(!buzz_wants(&w, &m));
        w.evse.plug = Some("unavailable".into());
        assert!(buzz_wants(&w, &m));
    }

    #[test]
    fn buzz_wants_on_pilot_b_or_c() {
        let mut w = world();
        let m = Memory::default();
        w.evse.plug = Some("Disconnected".into());
        for (id, want) in [("A", false), ("B", true), ("C", true)] {
            w.evse.state_id = Some(id.into());
            assert_eq!(buzz_wants(&w, &m), want, "{id}");
        }
    }

    #[test]
    fn buzz_flags() {
        let mut w = world();
        assert!(!buzz_drawing(&w));
        assert!(!buzz_ctrl(&w));
        assert_eq!(buzz_max_a(&w), 32.0);
        w.evse.state = Some("Charging".into());
        w.evse.override_a = Some(10.0);
        w.evse.override_max_a = Some(29.0);
        assert!(buzz_drawing(&w));
        assert!(buzz_ctrl(&w));
        assert_eq!(buzz_max_a(&w), 29.0);
    }

    #[test]
    fn ev3_wants_from_cable_or_car() {
        let mut w = world();
        assert!(!ev3_wants(&w));
        w.ev3.plugged = Some(true);
        assert!(ev3_wants(&w), "cable offline, car says plugged");
        w.cable.status = Some("unknown".into());
        assert!(ev3_wants(&w));
        w.cable.status = Some("available".into());
        assert!(!ev3_wants(&w), "cable online and says no car");
        for s in ["plugged_in", "charging", "charged"] {
            w.cable.status = Some(s.into());
            w.ev3.plugged = Some(false);
            assert!(ev3_wants(&w), "{s}");
        }
    }

    #[test]
    fn ev3_flags() {
        let mut w = world();
        assert!(!ev3_ctrl(&w));
        assert!(!ev3_power_above(&w, 0.2));
        assert!(!ev3_current_flowing(&w));
        assert_eq!(ev3_max_a(&w), 16.0);
        w.cable.set_current_a = Some(8.0);
        w.cable.power_kw = Some(0.25);
        w.cable.current_a = Some(0.6);
        w.cable.set_current_max_a = Some(13.0);
        assert!(ev3_ctrl(&w));
        assert!(ev3_power_above(&w, 0.2));
        assert!(!ev3_power_above(&w, 0.3));
        assert!(ev3_current_flowing(&w));
        assert_eq!(ev3_max_a(&w), 13.0);
    }

    #[test]
    fn price_gates() {
        let mut w = world();
        let mut d = Derived::default();
        let mut s = Settings::default();
        assert_eq!(control_price(&w, &d), 9.0);
        w.prices.afname_now = Some(0.30);
        assert_eq!(control_price(&w, &d), 0.30);
        d.stroomprijs_nu = Some(0.20);
        assert_eq!(control_price(&w, &d), 0.20);
        s.goedkoop_laden_prijs_drempel = 0.25;
        assert!(cheap(&w, &d, &s));
        s.ev_net_niveau = NetNiveau::MaxTotPiek;
        assert!(grid_max(&w, &d, &s));
        assert!(!grid_bridge(&w, &d, &s));
        s.ev_net_niveau = NetNiveau::Brug6A;
        assert!(grid_bridge(&w, &d, &s));
        d.stroomprijs_nu = Some(0.25);
        assert!(!cheap(&w, &d, &s), "strictly below the threshold");
        assert!(!grid_bridge(&w, &d, &s));
    }

    #[test]
    fn phase_views() {
        let mut w = world();
        w.p1.phase_a = [Some(-19.0), Some(10.0), None];
        assert_eq!(maxphase(&w), 10.0, "export does not eat headroom");
        assert_eq!(maxphase_abs(&w), 19.0, "the breaker sees both directions");
    }

    #[test]
    fn battery_views() {
        let mut w = world();
        assert_eq!(batt_w(&w), 0.0);
        w.solis.battery_power_w = Some(-1200.0);
        assert_eq!(batt_charge_w(&w), 1200.0);
        assert_eq!(batt_discharge_w(&w), 0.0);
        w.solis.battery_power_w = Some(800.0);
        assert_eq!(batt_charge_w(&w), 0.0);
        assert_eq!(batt_discharge_w(&w), 800.0);
    }
}
