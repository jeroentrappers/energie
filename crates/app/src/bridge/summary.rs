//! `<prefix>/summary`: the house in one compact JSON for a small display (an e-ink panel, a
//! watch, an ESP32), from the World and the derived values of one tick. Unknown values are
//! `null`; powers are W, prices €/kWh, the battery's `w` is + discharging / − charging (the
//! Solis convention), the grid's + import / − export.
//!
//! ```json
//! {"at": "2026-11-08T12:00:00+01:00", "mode": "control",
//!  "grid_w": 3066, "pv_w": 533, "house_w": 3599,
//!  "battery": {"soc": 54, "w": -1200, "setpoint_w": -1200},
//!  "ev3": {"w": 0, "a": null, "status": "...", "soc": 54.5, "plugged": false},
//!  "buzz": {"w": 3404, "a": 14.8, "mode": "Normal", "plug": "Connected", "soc": 43, "target_soc": 100},
//!  "active_car": "Buzz",
//!  "price": {"now": 0.37692, "next": 0.3787, "level": "goedkoop"},
//!  "quarter": {"avg_w": 3270, "target_w": 3900, "projection_w": 3085, "allowed_w": 3830},
//!  "peak_risk": false}
//! ```

use serde_json::{Value, json};

use crate::runtime::Snapshot;

/// The summary of one snapshot.
pub fn summary(s: &Snapshot) -> Value {
    let (w, d) = (&s.world, &s.derived);
    json!({
        "at": s.now,
        "mode": s.mode,
        "grid_w": w.p1.power_w,
        "pv_w": d.pv_vermogen_totaal_w,
        "house_w": d.huisverbruik_vermogen_w,
        "battery": {"soc": w.solis.soc, "w": w.solis.battery_power_w, "setpoint_w": d.batterij_setpoint_w},
        "ev3": {
            "w": d.ev_laadkabel_vermogen_w,
            "a": w.cable.current_a,
            "status": w.cable.status,
            "soc": w.ev3.soc,
            "plugged": w.ev3.plugged,
        },
        "buzz": {
            "w": d.ev_laadvermogen_smartevse_w,
            "a": w.evse.charge_current_a,
            "mode": w.evse.mode.map(|m| m.label()),
            "plug": w.evse.plug,
            "soc": w.buzz.soc,
            "target_soc": w.buzz.target_soc,
        },
        "active_car": d.ev_actieve_laadauto,
        "price": {"now": d.stroomprijs_nu, "next": d.kwartierprijs_volgend, "level": d.stroomprijs_niveau},
        "quarter": {
            "avg_w": d.kwartier_gemiddelde_w,
            "target_w": d.capaciteit_maandtarget_kw * 1000.0,
            "projection_w": d.kwartiervermogen_projectie_w,
            "allowed_w": d.kwartier_toegestaan_vermogen_w,
        },
        "peak_risk": d.capaciteitspiek_risico,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use energie_core::derived::Derived;
    use energie_core::world::{EvseMode, World};

    /// The summary carries the tick's values in the documented shape; unknowns are null.
    #[test]
    fn summary_of_a_tick() {
        let mut world = World::default();
        world.p1.power_w = Some(3066.0);
        world.solis.soc = Some(54.0);
        world.evse.mode = Some(EvseMode::Normal);
        world.evse.charge_current_a = Some(14.8);
        let derived = Derived {
            pv_vermogen_totaal_w: 533.0,
            capaciteit_maandtarget_kw: 3.9,
            kwartier_gemiddelde_w: 3270.0,
            stroomprijs_nu: Some(0.37692),
            capaciteitspiek_risico: true,
            ..Default::default()
        };
        let s = Snapshot { mode: "control", world, derived, ..Default::default() };
        let j = summary(&s);
        assert_eq!(j["grid_w"], 3066.0);
        assert_eq!(j["pv_w"], 533.0);
        assert_eq!(j["battery"]["soc"], 54.0);
        assert_eq!(j["battery"]["w"], Value::Null);
        assert_eq!(j["buzz"]["mode"], "Normal");
        assert_eq!(j["buzz"]["a"], 14.8);
        assert_eq!(j["ev3"]["plugged"], Value::Null);
        assert_eq!(j["quarter"]["target_w"], 3900.0);
        assert_eq!(j["price"]["now"], 0.37692);
        assert_eq!(j["peak_risk"], true);
        assert_eq!(j["active_car"], "Buzz");
        assert_eq!(j["mode"], "control");
        let keys: Vec<_> = j.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "active_car",
                "at",
                "battery",
                "buzz",
                "ev3",
                "grid_w",
                "house_w",
                "mode",
                "peak_risk",
                "price",
                "pv_w",
                "quarter"
            ]
        );
    }
}
