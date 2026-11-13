//! The Kolanky EV charging cable (`tuya_local`, device type `kolanky_evcharger`), Tuya 3.4.
//!
//! A [`Profile`] for the shared session worker ([`super::charger`]): the DPs as HA's
//! `tuya_local` entities, and the commands as HA's `script.laadkabel_*`. The [`CableHandle`] is
//! the [`Source`](crate::driver::Source) and [`Actuator`](crate::driver::Actuator).
//!
//! DP map (`devices/kolanky_evcharger.yaml`):
//!
//! | DP | Entity | Encoding |
//! | --- | --- | --- |
//! | 101 | `sensor.ev_charging_cable_status` | `no_connet`→available, `connect`→plugged_in, `charing`→charging, `wait_rfid`→waiting_for_authorization, `finish`→charged, `wait_charing`→waiting, `error`→fault |
//! | 102 | `sensor.…_spanning` | V |
//! | 105 | `sensor.…_stroom` | A ×10 |
//! | 109 | `sensor.…_vermogen` | kW ×10 |
//! | 113 | (max current constraint) | `Max16A` / `Max32A` |
//! | 114 / 115 | `number.…_charge_current` | A, 6..16 (or DP 115, 8..32, when 113 = `Max32A`) |
//! | 124 | `select.…_charging_operation` | `OpenCharging`=Start charging, `CloseCharging`=Stop charging, `WaitOperation`=No operation |

use energie_core::world::Cable;
use serde_json::{Map, Value, json};

use super::charger::{ChargerConfig, Dps, Profile, TuyaCharger, dp};
use crate::entity::Entity;
use crate::error::{DriverError, Result};

/// The cable's connection settings (protocol 3.4 by default).
pub type CableConfig = ChargerConfig;
/// The cable: [`TuyaCharger`] with the [`Kolanky`] profile.
pub type CableHandle = TuyaCharger<Kolanky>;

/// `sensor.ev_charging_cable_status` for a DP 101 value. Unknown values pass through.
pub fn status_name(dp: &str) -> String {
    match dp {
        "no_connet" => "available",
        "connect" => "plugged_in",
        "charing" => "charging",
        "wait_rfid" => "waiting_for_authorization",
        "finish" => "charged",
        "wait_charing" => "waiting",
        "error" => "fault",
        other => other,
    }
    .to_string()
}

fn max32(dps: &Map<String, Value>) -> bool {
    dps.get("113").and_then(Value::as_str) == Some("Max32A")
}

/// The HA entities of the cable from its DPs.
pub fn cable_from_dps(dps: &Map<String, Value>) -> Cable {
    let num = |k: &str| dps.get(k).and_then(Value::as_f64);
    let wide = max32(dps);
    Cable {
        status: dps.get("101").and_then(Value::as_str).map(status_name),
        current_a: num("105").map(|v| v / 10.0),
        voltage_v: num("102"),
        power_kw: num("109").map(|v| v / 10.0),
        set_current_a: num(if wide { "115" } else { "114" }),
        set_current_max_a: Some(if wide { 32.0 } else { 16.0 }),
    }
}

/// The DP and range `number.ev_charging_cable_charge_current` writes to.
fn current_dp(dps: &Map<String, Value>) -> (&'static str, u8, u8) {
    if max32(dps) { ("115", 8, 32) } else { ("114", 6, 16) }
}

/// The Kolanky cable as a [`Profile`]: `script.laadkabel_start_laden` selects `Start charging`
/// and sets the current `start_delay` later; stop selects `Stop charging`; the current goes to
/// the number's DP within its range.
pub struct Kolanky;

impl Profile for Kolanky {
    const NAME: &'static str = "tuya_cable";
    const DEFERRED_NOTE: &'static str = "after start_delay (script.laadkabel_start_laden: delay 3 s)";

    fn cable(dps: &Dps) -> Cable {
        cable_from_dps(dps)
    }

    fn entities(dps: &Dps) -> Vec<Entity> {
        extra_entities(dps).into_iter().zip(HA_ENTITIES).map(|(e, (_, ha))| e.with_ha(ha)).collect()
    }

    fn start(amps: u8, _: &Dps) -> (Dps, Option<u8>) {
        (dp("124", json!("OpenCharging")), Some(amps))
    }

    fn stop(_: &Dps) -> Dps {
        dp("124", json!("CloseCharging"))
    }

    fn current(amps: u8, dps: &Dps) -> Result<Dps> {
        let (cur, lo, hi) = current_dp(dps);
        if !(lo..=hi).contains(&amps) {
            return Err(DriverError::OutOfRange(format!("{amps} A, cable accepts {lo}..{hi}")));
        }
        Ok(dp(cur, json!(amps)))
    }
}

/// The key of each extra cable entity and the HA entity it replaces.
pub const HA_ENTITIES: [(&str, &str); 8] = [
    ("temperature_c", "sensor.ev_charging_cable_temperatuur"),
    ("energy_kwh", "sensor.ev_charging_cable_energie"),
    ("charging_time_h", "sensor.ev_charging_cable_charging_time"),
    ("operation", "select.ev_charging_cable_charging_operation"),
    ("status_raw", "sensor.ev_charging_cable_status"),
    ("fault", "binary_sensor.ev_charging_cable_probleem"),
    ("delay_h", "number.ev_charging_cable_delay"),
    ("charging_duration_h", "number.ev_charging_cable_charging_duration"),
];

/// `select.ev_charging_cable_charging_operation` for a DP 124 value; unknown values pass through.
pub fn operation_name(dp: &str) -> &str {
    match dp {
        "OpenCharging" => "Start charging",
        "CloseCharging" => "Stop charging",
        "WaitOperation" => "No operation",
        other => other,
    }
}

/// `kolanky_evcharger.yaml`: DP 110 °C ×10, DP 112 kWh ×10 (the session energy), DP 120 h ×10,
/// DP 124 the operation select, DP 101 raw, DP 10 the fault bitfield (any bit = problem),
/// DP 118 / 119 the delay and duration numbers (h).
pub fn extra_entities(dps: &Map<String, Value>) -> Vec<Entity> {
    use crate::entity::Value as V;
    let num = |k: &str, scale: f64| -> V { dps.get(k).and_then(Value::as_f64).map(|v| v / scale).into() };
    let text = |k: &str, f: fn(&str) -> &str| -> V { dps.get(k).and_then(Value::as_str).map(f).into() };
    vec![
        Entity::sensor("temperature_c", "Temperatuur", num("110", 10.0)).with_unit("temperature", "°C"),
        Entity::sensor("energy_kwh", "Energie (sessie)", num("112", 10.0)).with_unit("energy", "kWh"),
        Entity::sensor("charging_time_h", "Laadtijd", num("120", 10.0)).with_unit("duration", "h"),
        Entity::sensor("operation", "Laadbediening", text("124", operation_name)),
        Entity::sensor("status_raw", "Status (DP 101)", text("101", |s| s)),
        Entity::sensor("fault", "Probleem", dps.get("10").and_then(Value::as_f64).map(|v| v != 0.0).into()),
        Entity::sensor("delay_h", "Uitstel", num("118", 1.0)).with_unit("duration", "h"),
        Entity::sensor("charging_duration_h", "Laadduur", num("119", 1.0)).with_unit("duration", "h"),
    ]
}

#[cfg(test)]
mod tests;
