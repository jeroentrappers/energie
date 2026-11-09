//! Raw device readings: everything the stack reads from hardware or the cloud.
//!
//! Each field names the HA entity it replaces. `None` is HA's `unavailable`/`unknown`, so
//! `states(x)|float(d)` becomes `x.unwrap_or(d)` and `has_value(x)` becomes `x.is_some()`.

use serde::{Deserialize, Serialize};

use crate::time::Ts;

/// HomeWizard P1 meter (`homewizard`, API v2).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct P1 {
    /// `sensor.p1_meter_vermogen`, W, + import / − export.
    pub power_w: Option<f64>,
    /// `sensor.p1_meter_stroom_fase_1..3`, A, signed (− while exporting).
    pub phase_a: [Option<f64>; 3],
    /// `sensor.p1_meter_energie_import`, kWh, the import register.
    pub import_kwh: Option<f64>,
    /// `sensor.p1_meter_energie_export`, kWh.
    pub export_kwh: Option<f64>,
    /// `sensor.p1_meter_piekverbruik_huidige_maand`, W, the meter's own month peak.
    pub month_peak_w: Option<f64>,
}

/// Solis S6-EH1P hybrid inverter over Modbus (`solis_modbus`, Waveshare RS485 gateway).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Solis {
    /// `sensor.solis_s6_eh1p_battery_soc_2`, %.
    pub soc: Option<f64>,
    /// `sensor.solis_s6_eh1p_battery_power_net_2`, W, + discharge / − charge.
    pub battery_power_w: Option<f64>,
    /// `sensor.solis_s6_eh1p_total_pv_power_2`, W (the DC strings on the hybrid).
    pub pv_power_w: Option<f64>,
    /// `select.solis_s6_eh1p_rc_force_charge_discharge_2`.
    pub rc_mode: Option<RcMode>,
    /// `number.solis_s6_eh1p_rc_force_battery_charge_power_2`, W.
    pub rc_charge_w: Option<f64>,
    /// `number.solis_s6_eh1p_rc_force_battery_discharge_power_2`, W.
    pub rc_discharge_w: Option<f64>,
    /// `number.solis_s6_eh1p_rc_timeout_2`, minutes.
    pub rc_timeout_min: Option<f64>,
    /// `switch.solis_s6_eh1p_allow_grid_to_charge_the_battery_2`.
    pub allow_grid_charge: Option<bool>,
    /// `switch.solis_s6_eh1p_grid_feed_in_power_limit_switch_2`.
    pub feed_in_limit_on: Option<bool>,
    /// `number.solis_s6_eh1p_backflow_power_2`, W, the export limit.
    pub backflow_w: Option<f64>,
}

/// Remote-control mode of the Solis (`select.…_rc_force_charge_discharge_2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RcMode {
    None,
    ForceCharge,
    ForceDischarge,
}

/// Omnik string inverter on the main roof (`omnik_inverter`, AC-coupled, not controllable).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Omnik {
    /// `sensor.omnik_hoofddak_inverter_omnik_hoofddak_current_power_production`, W.
    pub power_w: Option<f64>,
}

/// SmartEVSE v3 for the Buzz (MQTT, `SmartEVSE/1234/...`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Evse {
    /// `sensor.smartevse_1234_ev_plug_state`: `Connected` / `Disconnected`.
    pub plug: Option<String>,
    /// `sensor.smartevse_1234_state`: `Charging`, `Charging Stopped`, `Connected to EV`, ...
    pub state: Option<String>,
    /// `sensor.smartevse_1234_stateid`: pilot state `A`, `B`, `C`, ...
    pub state_id: Option<String>,
    /// `select.smartevse_1234_mode`.
    pub mode: Option<EvseMode>,
    /// `number.smartevse_1234_charge_current_override`, A (0.1 A resolution).
    pub override_a: Option<f64>,
    /// `state_attr(number.smartevse_1234_charge_current_override, 'max')`, A.
    pub override_max_a: Option<f64>,
    /// `sensor.smartevse_1234_charge_current`, A.
    pub charge_current_a: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvseMode {
    Off,
    Normal,
    Smart,
    Solar,
    Pause,
}

impl EvseMode {
    /// The `select.smartevse_1234_mode` option (HA state string).
    pub fn label(self) -> &'static str {
        match self {
            EvseMode::Off => "Off",
            EvseMode::Normal => "Normal",
            EvseMode::Smart => "Smart",
            EvseMode::Solar => "Solar",
            EvseMode::Pause => "Pause",
        }
    }
}

/// Kolanky smart charging cable for the EV3 (`tuya_local`, protocol 3.4).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Cable {
    /// `sensor.ev_charging_cable_status`: `available`, `plugged_in`, `charging`, `charged`, ...
    pub status: Option<String>,
    /// `sensor.ev_charging_cable_stroom`, A.
    pub current_a: Option<f64>,
    /// `sensor.ev_charging_cable_spanning`, V.
    pub voltage_v: Option<f64>,
    /// `sensor.ev_charging_cable_vermogen`, kW.
    pub power_kw: Option<f64>,
    /// `number.ev_charging_cable_charge_current`, A (6..16, whole amps).
    pub set_current_a: Option<f64>,
    /// `state_attr(number.ev_charging_cable_charge_current, 'max')`, A.
    pub set_current_max_a: Option<f64>,
}

/// Kia EV3 telemetry (`kia_uvo`, cloud).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ev3 {
    /// `binary_sensor.ev3_ev_battery_plug`.
    pub plugged: Option<bool>,
    /// `sensor.ev3_ev_battery_level`, %.
    pub soc: Option<f64>,
    /// `number.ev3_ac_charging_limit`, %.
    pub ac_limit: Option<f64>,
    /// `sensor.ev3_odometer`, km.
    pub odometer_km: Option<f64>,
}

/// VW ID.Buzz telemetry (`cupra_eu_data_act`, cloud).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Buzz {
    /// `sensor.de_buzz_accu`, %.
    pub soc: Option<f64>,
    /// `sensor.de_buzz_laaddoel`, %.
    pub target_soc: Option<f64>,
    /// `sensor.de_buzz_kilometerstand`, km.
    pub odometer_km: Option<f64>,
}

/// One 15-minute price slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceSlot {
    #[serde(with = "crate::time::serde_ts")]
    pub start: Ts,
    /// EUR/kWh, tariff applied.
    pub price: f64,
}

/// Day-ahead prices (energy-charts primary, ENTSO-E fallback; `entsoe` and `price_backup.py`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Prices {
    /// `sensor.jeroen_current_electricity_market_price`, EUR/kWh, offtake tariff incl. VAT.
    pub afname_now: Option<f64>,
    /// `sensor.jeroen_current_percentage_in_electricity_price_range`, %.
    pub afname_pct_in_range: Option<f64>,
    /// `state_attr(sensor.jeroen_average_electricity_price, 'prices')`: today + tomorrow.
    pub afname_slots: Vec<PriceSlot>,
    /// `prices_tomorrow` of the offtake feed is non-empty.
    pub afname_has_tomorrow: bool,
    /// `sensor.injectie_current_electricity_market_price`, EUR/kWh.
    pub injectie_now: Option<f64>,
    /// `prices_tomorrow` of the injection feed is non-empty.
    pub injectie_has_tomorrow: bool,
    /// `sensor.entso_e_energy_prices_15_min`, EUR/kWh, raw EPEX.
    pub epex_now: Option<f64>,
    /// `input_number.prijs_backup_nu`, EUR/kWh, written by the price fetcher; −1 disables.
    pub backup_now: Option<f64>,
}

/// The two LG airco units (`lg_thinq`, cloud).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Airco {
    /// `switch.airco_beneden_power`.
    pub beneden_on: Option<bool>,
    /// `switch.airco_zolder_power`.
    pub zolder_on: Option<bool>,
}

/// The freezer plug (`switch.vriezer`, `sensor.vriezer_vermogen`). Absent in HA today.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Vriezer {
    pub power_w: Option<f64>,
    pub switch_on: Option<bool>,
}

/// Fluvius portal data (cloud, display only).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Fluvius {
    /// `sensor.fluvius_meter_1sag0000000000_peak_power`, kW.
    pub peak_kw: Option<f64>,
}

/// Everything read from the outside world at one instant.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct World {
    pub p1: P1,
    pub solis: Solis,
    pub omnik: Omnik,
    pub evse: Evse,
    pub cable: Cable,
    pub ev3: Ev3,
    pub buzz: Buzz,
    pub prices: Prices,
    pub airco: Airco,
    pub vriezer: Vriezer,
    pub fluvius: Fluvius,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `select.smartevse_1234_mode` options, as `input_text.smartevse_modus_laatst_bekend`
    /// stores them.
    #[test]
    fn evse_mode_labels_are_the_ha_options() {
        let all = [EvseMode::Off, EvseMode::Normal, EvseMode::Smart, EvseMode::Solar, EvseMode::Pause];
        assert_eq!(all.map(EvseMode::label), ["Off", "Normal", "Smart", "Solar", "Pause"]);
    }
}
