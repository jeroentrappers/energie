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
//! | 5 | `sigle_phase_power` | kW ×1000 (in practice whole kW) | entity `phase_power_kw` |
//! | 6 | phase meter (not in the cloud map) | base64 of 8 bytes: V ×10 (u16), mA (u24), W (u24); only sent on `UPDATEDPS` | `voltage_v`, `current_a`, `power_kw` ([`phase`]) |
//! | 9 | `power_total` | kW ×1000 (in practice whole kW) | `power_kw` when DP 6 is not there (see below) |
//! | 10 | fault bitfield (not in the cloud map) | 0 = none | entity `fault` |
//! | 14 | `work_mode` | `charge_now`, `charge_pct`, `charge_energy`, `charge_schedule` | entity `mode`; a start sets `charge_now` |
//! | 18 | `switch` | bool | start / stop |
//! | 23 | firmware version | text | entity `firmware` |
//! | 24 | `temp_current` | °C | entity `temperature_c` |
//! | 25 | `charge_energy_once` | kWh ×100 | entity `energy_kwh` |
//!
//! The meter is DP 6, which the charger only reports when asked: the worker requests it every
//! [`ChargerConfig::refresh`] (5 s) with `UPDATEDPS`, as the Smart Life app does. When it holds
//! a voltage while the charger is `charger_charging`, it gives the reading's voltage, current and
//! power (in other states it may still hold the last session's values until the next refresh).
//! Otherwise (not asked yet, or all zeros, as at rest) the reading falls back to DP 9: `current_a` is the power over 230 V,
//! as the logic already does for the SmartEVSE (`sensor.ev_laadvermogen_smartevse_w / 230`), and
//! `voltage_v` stays unset (the logic's `|float(230)`).
//!
//! Without DP 6 the EV3's charger looks as if it does not meter: DP 9 and DP 5 stay 0, and the energy counters do
//! not move, while the car draws 2.2 kW at 10 A (checked 2026-11-13, also 0 in the Tuya cloud).
//! It has also been seen reporting 1 W (DP 9 = 1) and leaving the switch (DP 18) `false` while
//! the car charges at 1.2 kW, and its app shows voltage and current from the cloud that never
//! reach the local DPs. So while it is `charger_charging` with less than 0.1 kW reported, the
//! set current (DP 4) is taken as the current and the power is that at 230 V. That is an upper
//! bound: a car that takes less (near full, or its own limit lower) still reads the set current.
//! A charger that does report power is read as measured.
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
/// Below this a power reported while charging is not a measurement (the EV3's charger reports
/// 0 or 1 W while the car draws 1.2 kW); a car charges at 6 A × 230 V = 1.38 kW at least.
const MIN_METERED_KW: f64 = 0.1;

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

/// The phase meter in DP 6: (V, A, W). `None` when it is missing or malformed.
pub fn phase(dps: &Dps) -> Option<(f64, f64, f64)> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD.decode(dps.get("6")?.as_str()?).ok()?;
    let b: [u8; 8] = raw.try_into().ok()?;
    let u24 = |i: usize| f64::from(u32::from_be_bytes([0, b[i], b[i + 1], b[i + 2]]));
    Some((f64::from(u16::from_be_bytes([b[0], b[1]])) / 10.0, u24(2) / 1000.0, u24(5)))
}

/// DP 6's encoding of (V, A, W), for the simulator and tests.
pub fn encode_phase(volts: f64, amps: f64, watts: f64) -> String {
    use base64::Engine as _;
    let v = ((volts * 10.0).round() as u16).to_be_bytes();
    let a = ((amps * 1000.0).round() as u32).to_be_bytes();
    let w = (watts.round() as u32).to_be_bytes();
    base64::engine::general_purpose::STANDARD.encode([v[0], v[1], a[1], a[2], a[3], w[1], w[2], w[3]])
}

/// The `charger:ev3` reading from the DPs.
pub fn cable_from_dps(dps: &Dps) -> Cable {
    let num = |k: &str| dps.get(k).and_then(Value::as_f64);
    let charging = dps.get("3").and_then(Value::as_str) == Some("charger_charging");
    // DP 6 keeps its last value until the next refresh: only a charging charger's meter counts.
    if let Some((volts, amps, watts)) = phase(dps).filter(|(v, _, _)| charging && *v > 0.0) {
        return Cable {
            status: dps.get("3").and_then(Value::as_str).map(status_name),
            current_a: Some((amps * 100.0).round() / 100.0),
            voltage_v: Some(volts),
            power_kw: Some(watts / 1000.0),
            set_current_a: num("4"),
            set_current_max_a: Some(f64::from(MAX_A)),
        };
    }
    let measured = num("9").map(|v| v / 1000.0);
    // No power while charging: the charger does not meter, so estimate from the set current.
    let power_kw = match (measured, num("4")) {
        (Some(kw), _) if kw >= MIN_METERED_KW || !charging => Some(kw),
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
    const REFRESH: &'static [u32] = &[6];

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
