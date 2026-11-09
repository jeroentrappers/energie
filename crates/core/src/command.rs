//! What the controllers ask the outside world to do. Drivers turn these into wire traffic.

use serde::{Deserialize, Serialize};

use crate::world::{EvseMode, RcMode};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// MQTT `SmartEVSE/1234/Set/CurrentOverride`, payload in tenths of an amp (`145` = 14.5 A).
    EvseCurrentDeciA(u32),
    /// `select.smartevse_1234_mode`.
    EvseMode(EvseMode),
    /// `script.laadkabel_start_laden`: select `Start charging`, wait 3 s, set the current.
    /// The driver owns the 3 s sequence (HA script mode `restart`).
    CableStart { amps: u8 },
    /// `script.laadkabel_stop_laden`: select `Stop charging`.
    CableStop,
    /// `number.ev_charging_cable_charge_current`, whole amps.
    CableSetCurrent(u8),
    /// Drop and re-open the Tuya session (HA: `homeassistant.reload_config_entry`).
    CableReconnect,
    /// `select.solis_s6_eh1p_rc_force_charge_discharge_2`.
    SolisRcMode(RcMode),
    /// `number.solis_s6_eh1p_rc_force_battery_charge_power_2`, W.
    SolisRcChargeW(u32),
    /// `number.solis_s6_eh1p_rc_force_battery_discharge_power_2`, W.
    SolisRcDischargeW(u32),
    /// `number.solis_s6_eh1p_rc_timeout_2`, minutes.
    SolisRcTimeoutMin(u32),
    /// `switch.solis_s6_eh1p_allow_grid_to_charge_the_battery_2`.
    SolisAllowGridCharge(bool),
    /// `switch.solis_s6_eh1p_grid_feed_in_power_limit_switch_2`.
    SolisFeedInLimit(bool),
    /// `number.solis_s6_eh1p_backflow_power_2`, W (export limit).
    SolisBackflowW(u32),
    /// Drop and re-open the Modbus session (HA: the watchdog restarts all of HA).
    SolisReconnect,
    /// `switch.airco_beneden_power` / `switch.airco_zolder_power`.
    Airco { unit: AircoUnit, on: bool },
    /// Re-fetch the day-ahead prices now (HA: `homeassistant.update_entity` on the price feeds).
    RefreshPrices,
    /// `script.notify_jeroen`: a push message to the phone.
    Notify { title: String, message: String, high_priority: bool },
    /// `persistent_notification.create`: a note on the dashboard. With a `notification_id`
    /// (`id`) a new note replaces the earlier one with that id instead of adding another.
    Persistent { title: String, message: String, id: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AircoUnit {
    Beneden,
    Zolder,
}

impl Command {
    pub fn notify(title: impl Into<String>, message: impl Into<String>) -> Self {
        Command::Notify { title: title.into(), message: message.into(), high_priority: false }
    }

    pub fn notify_high(title: impl Into<String>, message: impl Into<String>) -> Self {
        Command::Notify { title: title.into(), message: message.into(), high_priority: true }
    }

    pub fn persistent(title: impl Into<String>, message: impl Into<String>) -> Self {
        Command::Persistent { title: title.into(), message: message.into(), id: None }
    }

    /// A persistent note with a `notification_id`.
    pub fn persistent_id(id: impl Into<String>, title: impl Into<String>, message: impl Into<String>) -> Self {
        Command::Persistent { title: title.into(), message: message.into(), id: Some(id.into()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors() {
        assert_eq!(
            Command::notify("a", "b"),
            Command::Notify { title: "a".into(), message: "b".into(), high_priority: false }
        );
        assert_eq!(
            Command::notify_high("a", "b"),
            Command::Notify { title: "a".into(), message: "b".into(), high_priority: true }
        );
        assert_eq!(
            Command::persistent("a", "b"),
            Command::Persistent { title: "a".into(), message: "b".into(), id: None }
        );
        assert_eq!(
            Command::persistent_id("i", "a", "b"),
            Command::Persistent { title: "a".into(), message: "b".into(), id: Some("i".into()) }
        );
    }
}
