//! Internal state that persists across restarts: what HA kept in hidden helpers, counters,
//! automation timestamps, meters and filters. Users don't set these; the controllers do.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::meters::MetersState;
use crate::render::Renders;
use crate::time::Ts;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    /// `input_number.batterij_setpoint_bias`, W: the dispatcher's slow integral.
    pub batterij_setpoint_bias: f64,
    /// `input_boolean.batterij_net_laden`: grid-charge the battery to full (set by goedkoop_laden).
    pub batterij_net_laden: bool,
    /// Not in HA: `goedkoop_laden` stopped because the battery reached [`NET_LADEN_VOL_SOC`]
    /// during this cheap `Max tot piek` window. While set, grid-charging restarts only below
    /// [`NET_LADEN_HERSTART_SOC`] and the setpoint holds the battery instead of discharging
    /// it. Cleared when the window ends or the SoC falls below the restart level.
    ///
    /// [`NET_LADEN_VOL_SOC`]: crate::automations::goedkoop_laden::NET_LADEN_VOL_SOC
    /// [`NET_LADEN_HERSTART_SOC`]: crate::automations::goedkoop_laden::NET_LADEN_HERSTART_SOC
    pub batterij_net_laden_vol: bool,
    /// `input_boolean.soluna_absorptie_bezig`: the dispatcher is actively charging/discharging.
    pub soluna_absorptie_bezig: bool,
    /// `input_boolean.ev_hoger_laadstil`: a higher-ranked car is eligible but not drawing.
    pub ev_hoger_laadstil: bool,
    /// `input_number.ev_hoger_laadstil_sinds`: epoch seconds the stall started, 0 = none.
    pub ev_hoger_laadstil_sinds: f64,
    /// `input_boolean.airco_dump_sessie_bezig`.
    pub airco_dump_sessie_bezig: bool,
    /// `input_text.smartevse_plug_laatst_bekend`: last valid SmartEVSE plug state.
    pub smartevse_plug_laatst_bekend: String,
    /// `input_number.batterij_naar_auto_start_kwh`: discharge counter when the dump started.
    pub batterij_naar_auto_start_kwh: f64,
    /// `input_number.nacht_import_startmeting`, kWh (0 = no measurement running).
    pub nacht_import_startmeting: f64,
    /// `input_number.nacht_ontlaad_startmeting`, kWh.
    pub nacht_ontlaad_startmeting: f64,
    /// `input_number.nacht_ev_net_startmeting`, kWh.
    pub nacht_ev_net_startmeting: f64,
    /// `input_number.nachtdekking`, %.
    pub nachtdekking: f64,
    /// `input_number.nacht_batterij_kwh`.
    pub nacht_batterij_kwh: f64,
    /// `input_number.nacht_import_kwh`.
    pub nacht_import_kwh: f64,
    /// `input_number.nacht_duur_uren`.
    pub nacht_duur_uren: f64,
    /// `input_text.capaciteit_piekhistoriek`: `YYYY-MM:kW,...`, at most 12 entries.
    pub capaciteit_piekhistoriek: String,
    /// `input_datetime.watchdog_laatste_herstart`.
    #[serde(with = "crate::time::serde_opt")]
    pub watchdog_laatste_herstart: Option<Ts>,
    /// `counter.peak_guard_interventies`.
    pub peak_guard_interventies: u64,
    /// `counter.buzz_laadweigering`.
    pub buzz_laadweigering: u64,
    /// `timer.steering_volgende_update`: when the dispatcher's next cycle is due (display).
    #[serde(with = "crate::time::serde_opt")]
    pub steering_volgende_update: Option<Ts>,
    /// `last_triggered` of each automation, by automation id.
    #[serde(with = "crate::time::serde_map")]
    pub last_triggered: BTreeMap<String, Ts>,
    /// Utility meters, Riemann integrals and the P1 median filter.
    pub meters: MetersState,
    /// `states.binary_sensor.capaciteitspiek_risico.last_changed`: when `Derived.capaciteitspiek_risico`
    /// last flipped (or HA started, which resets `last_changed`). Kept by
    /// [`crate::automations::ev_modulator::observe_risk`]; the modulators' `risk_long` reads it.
    #[serde(with = "crate::time::serde_opt")]
    pub capaciteitspiek_risico_sinds: Option<Ts>,
    /// The last renders of the `now()` templates HA only re-renders on an input change or at
    /// the top of a minute ([`crate::render`]). Not an HA entity; kept so a restart renders
    /// afresh like HA's.
    #[serde(skip)]
    pub template_renders: Renders,
    /// `input_text.smartevse_modus_laatst_bekend`: the last valid `select.smartevse_1234_mode`
    /// (set by `buzz_mode_stempel`; HA seeded it with the mode when it was created).
    pub smartevse_modus_laatst_bekend: String,
}

impl Default for Memory {
    fn default() -> Self {
        Memory {
            batterij_setpoint_bias: 0.0,
            batterij_net_laden: false,
            batterij_net_laden_vol: false,
            soluna_absorptie_bezig: false,
            ev_hoger_laadstil: false,
            ev_hoger_laadstil_sinds: 0.0,
            airco_dump_sessie_bezig: false,
            smartevse_plug_laatst_bekend: String::new(),
            batterij_naar_auto_start_kwh: 0.0,
            nacht_import_startmeting: 0.0,
            nacht_ontlaad_startmeting: 0.0,
            nacht_ev_net_startmeting: 0.0,
            nachtdekking: 0.0,
            nacht_batterij_kwh: 0.0,
            nacht_import_kwh: 0.0,
            nacht_duur_uren: 0.0,
            capaciteit_piekhistoriek: String::new(),
            watchdog_laatste_herstart: None,
            peak_guard_interventies: 0,
            buzz_laadweigering: 0,
            steering_volgende_update: None,
            last_triggered: BTreeMap::new(),
            meters: MetersState::default(),
            capaciteitspiek_risico_sinds: None,
            template_renders: Renders::default(),
            smartevse_modus_laatst_bekend: String::new(),
        }
    }
}

impl Memory {
    pub fn last_triggered(&self, id: &str) -> Option<Ts> {
        self.last_triggered.get(id).copied()
    }

    pub fn mark_triggered(&mut self, id: &str, now: Ts) {
        self.last_triggered.insert(id.to_string(), now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::at;

    #[test]
    fn last_triggered_roundtrip_and_serde_default() {
        let mut m = Memory::default();
        assert_eq!(m.last_triggered("x"), None);
        let t = at(2026, 11, 7, 12, 0, 0);
        m.mark_triggered("x", t);
        assert_eq!(m.last_triggered("x"), Some(t));
        let json = serde_json::to_string(&m).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
        let empty: Memory = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Memory::default());
    }
}
