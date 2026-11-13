//! The Tuya EV charger of category `qccdz` (Tuya's standard charging-pile DPs), protocol 3.5:
//! the portable charger on the EV3 that replaced the Kolanky cable (product
//! `dsmsam7xpb3ht7rl`, firmware 2.0.0). In HA it comes through the Tuya cloud integration as
//! `EV Charger`.
//!
//! DP map (the device's `dpStatusRelationDTOS`, checked against a local query):
//!
//! | DP | Code | Encoding | `charger:ev3` |
//! | --- | --- | --- | --- |
//! | 1 | `forward_energy_total` | kWh ×100 | entity `energy_total_kwh` |
//! | 3 | `work_state` | `charger_free`, `charger_insert`, `charger_wait`, `charger_charging`, `charger_pause`, `charger_end`, `charger_fault`, `charger_free_fault` | `status` ([`status_name`]) |
//! | 4 | `charge_cur_set` | A (the device takes 1..255; we write 6..=[`MAX_A`]) | `set_current_a` |
//! | 5 | `sigle_phase_power` | kW ×1000 | entity `phase_power_kw` |
//! | 9 | `power_total` | kW ×1000 | `power_kw`, and `current_a` = W / 230 (see below) |
//! | 10 | fault bitfield (not in the cloud map) | 0 = none | entity `fault` |
//! | 14 | `work_mode` | `charge_now`, `charge_pct`, `charge_energy`, `charge_schedule` | entity `mode`; a start sets `charge_now` |
//! | 18 | `switch` | bool | start / stop |
//! | 23 | firmware version | text | entity `firmware` |
//! | 24 | `temp_current` | °C | entity `temperature_c` |
//! | 25 | `charge_energy_once` | kWh ×100 | entity `energy_kwh` |
//!
//! The device reports no current and no voltage. `current_a` is the power over 230 V, as the
//! logic already does for the SmartEVSE (`sensor.ev_laadvermogen_smartevse_w / 230`), and
//! `voltage_v` stays unset (the logic's `|float(230)`).
//!
//! The EV3's charger does not meter at all: DP 9 and DP 5 stay 0, and the energy counters do
//! not move, while the car draws 2.2 kW at 10 A (checked 2026-11-13, also 0 in the Tuya cloud).
//! So while it is `charger_charging` with no power reported, the set current (DP 4) is taken as
//! the current and the power is that at 230 V. That is an upper bound: a car that takes less
//! (near full, or its own limit lower) still reads the set current. A charger that does report
//! power is read as measured.
//!
//! The logic counts the EV3 as there when the status is `plugged_in`, `charging` or `charged`,
//! so the states with a car connected and not charging (`charger_insert`, `charger_wait`,
//! `charger_pause`) all read `plugged_in`: otherwise a paused session could not be restarted.
//! The raw state is the `status_raw` entity.

use energie_core::world::Cable;
use serde_json::{Value, json};

use super::charger::{ChargerConfig, Dps, Profile, TuyaCharger, dp};
use crate::entity::{Entity, StateClass};
use crate::error::{DriverError, Result};

/// The charger: [`TuyaCharger`] with the [`EvCharger`] profile. Spawn it with a
/// [`ChargerConfig`] whose `version` is [`Version::V35`](super::Version::V35).
pub type EvChargerHandle = TuyaCharger<EvCharger>;

/// The lowest current written (IEC 61851's minimum).
pub const MIN_A: u8 = 6;
/// The highest current written: the charger's rating (it reports 16 A as its setting at full).
pub const MAX_A: u8 = 16;
/// The voltage the current is derived at.
const VOLT: f64 = 230.0;

/// The `work_state` (DP 3) in the cable's vocabulary. Unknown values pass through.
pub fn status_name(dp: &str) -> String {
    match dp {
        "charger_free" => "available",
        "charger_insert" | "charger_wait" | "charger_pause" => "plugged_in",
        "charger_charging" => "charging",
        "charger_end" => "charged",
        "charger_fault" | "charger_free_fault" => "fault",
        other => other,
    }
    .to_string()
}

/// The `charger:ev3` reading from the DPs.
pub fn cable_from_dps(dps: &Dps) -> Cable {
    let num = |k: &str| dps.get(k).and_then(Value::as_f64);
    let charging = dps.get("3").and_then(Value::as_str) == Some("charger_charging");
    let measured = num("9").map(|v| v / 1000.0);
    // No power while charging: the charger does not meter, so estimate from the set current.
    let power_kw = match (measured, num("4")) {
        (Some(kw), _) if kw > 0.0 || !charging => Some(kw),
        (_, Some(amps)) if charging => Some(amps * VOLT / 1000.0),
        (m, _) => m,
    };
    Cable {
        status: dps.get("3").and_then(Value::as_str).map(status_name),
        current_a: power_kw.map(|kw| (kw * 1000.0 / VOLT * 100.0).round() / 100.0),
        voltage_v: None,
        power_kw,
        set_current_a: num("4"),
        set_current_max_a: Some(f64::from(MAX_A)),
    }
}

/// The key of each extra entity and the HA entity (Tuya cloud integration) it stands for.
pub const HA_ENTITIES: [(&str, Option<&str>); 9] = [
    ("energy_total_kwh", Some("sensor.ev_charger_totaal_energieverbruik")),
    ("energy_kwh", Some("sensor.ev_charger_session_energy")),
    ("phase_power_kw", None),
    ("temperature_c", Some("sensor.ev_charger_temperatuur")),
    ("mode", Some("select.ev_charger_charging_mode")),
    ("switch", Some("switch.ev_charger_schakelaar")),
    ("status_raw", Some("sensor.ev_charger_status")),
    ("fault", None),
    ("firmware", None),
];

/// The entities beyond the reading, in [`HA_ENTITIES`] order.
pub fn extra_entities(dps: &Dps) -> Vec<Entity> {
    use crate::entity::Value as V;
    let num = |k: &str, scale: f64| -> V { dps.get(k).and_then(Value::as_f64).map(|v| v / scale).into() };
    let text = |k: &str| -> V { dps.get(k).and_then(Value::as_str).into() };
    let es = vec![
        Entity::sensor("energy_total_kwh", "Energie (totaal)", num("1", 100.0))
            .with_unit("energy", "kWh")
            .with_state_class(StateClass::TotalIncreasing),
        Entity::sensor("energy_kwh", "Energie (sessie)", num("25", 100.0)).with_unit("energy", "kWh"),
        Entity::sensor("phase_power_kw", "Vermogen (fase)", num("5", 1000.0)).with_unit("power", "kW"),
        Entity::sensor("temperature_c", "Temperatuur", num("24", 1.0)).with_unit("temperature", "°C"),
        Entity::sensor("mode", "Laadmodus", text("14")),
        Entity::sensor("switch", "Schakelaar", dps.get("18").and_then(Value::as_bool).into()),
        Entity::sensor("status_raw", "Status (DP 3)", text("3")),
        Entity::sensor("fault", "Probleem", dps.get("10").and_then(Value::as_f64).map(|v| v != 0.0).into()),
        Entity::sensor("firmware", "Firmware", text("23")),
    ];
    es.into_iter()
        .zip(HA_ENTITIES)
        .map(|(e, (_, ha))| match ha {
            Some(ha) => e.with_ha(ha),
            None => e,
        })
        .collect()
}

/// The `qccdz` charger as a [`Profile`]. A start is one DP set: `charge_now` (when another mode
/// is selected), the current (when the charger takes it) and the switch on. A stop switches
/// off.
pub struct EvCharger;

impl Profile for EvCharger {
    const NAME: &'static str = "tuya_evcharger";

    fn cable(dps: &Dps) -> Cable {
        cable_from_dps(dps)
    }

    fn entities(dps: &Dps) -> Vec<Entity> {
        extra_entities(dps)
    }

    fn start(amps: u8, dps: &Dps) -> (Dps, Option<u8>) {
        let mut m = Dps::new();
        if dps.get("14").and_then(Value::as_str) != Some("charge_now") {
            m.insert("14".into(), json!("charge_now"));
        }
        if let Ok(cur) = Self::current(amps, dps) {
            m.extend(cur);
        }
        m.insert("18".into(), json!(true));
        (m, None)
    }

    fn stop(_: &Dps) -> Dps {
        dp("18", json!(false))
    }

    fn current(amps: u8, _: &Dps) -> Result<Dps> {
        if !(MIN_A..=MAX_A).contains(&amps) {
            return Err(DriverError::OutOfRange(format!("{amps} A, charger accepts {MIN_A}..{MAX_A}")));
        }
        Ok(dp("4", json!(amps)))
    }
}

/// [`ChargerConfig::new`] for this charger: protocol 3.5, no start delay (nothing follows a
/// start).
pub fn config(addr: std::net::SocketAddr, device_id: &str, local_key: &str) -> ChargerConfig {
    ChargerConfig { version: super::Version::V35, ..ChargerConfig::new(addr, device_id, local_key) }
}

#[cfg(test)]
mod tests;
