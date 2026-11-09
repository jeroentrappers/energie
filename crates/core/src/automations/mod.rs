//! The HA automations, one module each. [`all`] lists them in evaluation order.
//!
//! Every tick the engine runs each automation once; an automation decides from the context
//! whether one of its triggers fired (time patterns, transitions, `for:` holds) and whether
//! its conditions pass, then emits commands and updates settings/memory like HA's actions.

use crate::command::Command;
use crate::derived::Derived;
use crate::memory::Memory;
use crate::settings::Settings;
use crate::time::{self, Ts};
use crate::world::World;

pub mod airco_dump;
pub mod batterij_autonoom_netladen_stop;
pub mod batterij_naar_auto;
pub mod breaker_guard;
pub mod buzz_handshake;
pub mod buzz_laadweigering_reset;
pub mod buzz_mode_stempel;
pub mod buzz_reset_op_uitplug;
pub mod dagbalans;
pub mod dispatcher;
pub mod dispatcher_off_target_alert;
pub mod entsoe_qh_refresh;
pub mod entsoe_refresh;
pub mod ev_cable_autoheal;
pub mod ev_hoger_laadstil;
pub mod ev_laden_buzz;
pub mod ev_laden_cable;
pub mod ev_modulator;
pub mod goedkoop_laden;
pub mod nachtdekking;
pub mod neg_injectie;
pub mod peakguard;
pub mod piekhistoriek;
pub mod smartevse_pilot_arm;
pub mod smartevse_plug_latch;
pub mod solis_watchdog;
/// `vriezer_bewaking`: switched off in HA on 2026-11-07 (its entities do not exist), so it is
/// not in [`all`]; the port is kept for when the freezer plug returns.
pub mod vriezer;

/// Everything an automation sees during one tick. `p*` fields are the previous tick, for
/// transition triggers. Settings and memory are shared and mutated in evaluation order, like
/// HA's sequential service calls.
pub struct Cx<'a> {
    pub now: Ts,
    pub prev_now: Ts,
    /// The first tick after start (HA's `homeassistant: start` event).
    pub started: bool,
    pub w: &'a World,
    pub pw: &'a World,
    pub d: &'a Derived,
    pub pd: &'a Derived,
    pub s: &'a mut Settings,
    pub ps: &'a Settings,
    pub m: &'a mut Memory,
    pub out: &'a mut Vec<Command>,
}

impl Cx<'_> {
    /// A `time_pattern` with a period of `period_s` seconds fired this tick.
    pub fn every(&self, period_s: i64) -> bool {
        !self.started && time::crossed_period(self.prev_now, self.now, period_s)
    }

    /// A `time: at HH:MM:SS` trigger fired this tick.
    pub fn daily(&self, h: u32, m: u32, s: u32) -> bool {
        !self.started && time::crossed_daily(self.prev_now, self.now, h, m, s)
    }

    pub fn send(&mut self, c: Command) {
        self.out.push(c);
    }

    /// Seconds since automation `id` last ran its actions, or `None` if it never did.
    pub fn since_triggered(&self, id: &str) -> Option<f64> {
        self.m.last_triggered(id).map(|t| time::secs(t, self.now))
    }

    /// Record that automation `id` ran its actions now (HA's `last_triggered`).
    pub fn mark(&mut self, id: &str) {
        self.m.mark_triggered(id, self.now);
    }
}

pub trait Automation: Send {
    fn id(&self) -> &'static str;
    fn step(&mut self, cx: &mut Cx);
}

/// All automations, in evaluation order: bookkeeping first, then the decisions that read it.
#[allow(clippy::default_constructed_unit_structs)] // uniform registry; some carry state
pub fn all() -> Vec<Box<dyn Automation>> {
    vec![
        Box::new(smartevse_plug_latch::SmartevsePlugLatch::default()),
        Box::new(buzz_mode_stempel::BuzzModeStempel::default()),
        Box::new(buzz_laadweigering_reset::BuzzLaadweigeringReset::default()),
        Box::new(goedkoop_laden::GoedkoopLaden::default()),
        Box::new(ev_hoger_laadstil::EvHogerLaadstil::default()),
        Box::new(batterij_naar_auto::BatterijNaarAuto::default()),
        Box::new(ev_laden_buzz::EvLadenBuzz::default()),
        Box::new(ev_laden_cable::EvLadenCable::default()),
        Box::new(buzz_handshake::BuzzHandshake::default()),
        Box::new(buzz_reset_op_uitplug::BuzzResetOpUitplug::default()),
        Box::new(smartevse_pilot_arm::SmartevsePilotArm::default()),
        Box::new(ev_cable_autoheal::EvCableAutoheal::default()),
        Box::new(breaker_guard::BreakerGuard::default()),
        Box::new(peakguard::PeakguardAfschakelen::default()),
        Box::new(peakguard::PeakguardHerstel::default()),
        Box::new(dispatcher::SolunaDispatcher::default()),
        Box::new(batterij_autonoom_netladen_stop::BatterijAutonoomNetladenStop::default()),
        Box::new(dispatcher_off_target_alert::DispatcherOffTargetAlert::default()),
        Box::new(solis_watchdog::SolisWatchdog::default()),
        Box::new(neg_injectie::NegInjectieCurtail::default()),
        Box::new(neg_injectie::NegInjectieHerstel::default()),
        Box::new(airco_dump::AircoDumpStart::default()),
        Box::new(airco_dump::AircoDumpStop::default()),
        Box::new(nachtdekking::NachtdekkingStart::default()),
        Box::new(nachtdekking::NachtdekkingEind::default()),
        Box::new(dagbalans::DagbalansRapport::default()),
        Box::new(entsoe_refresh::EntsoeDayaheadMorgen::default()),
        Box::new(entsoe_qh_refresh::EntsoeQhKwartierVerversen::default()),
        Box::new(piekhistoriek::CapaciteitPiekhistoriek::default()),
    ]
}

#[cfg(test)]
mod tests {
    /// Every enabled live HA automation (the original configuration) is ported exactly once,
    /// under its HA `id` (the key of `last_triggered`, which other automations read, e.g.
    /// `buzz_mode_stempel`). `vriezer_bewaking` is disabled in HA since 2026-11-07 and not run;
    /// `entsoe_qh_kwartier_verversen` was added that day.
    #[test]
    fn the_registry_holds_the_29_enabled_live_automations_by_their_ha_ids() {
        let ids: Vec<&str> = super::all().iter().map(|a| a.id()).collect();
        assert_eq!(
            ids,
            vec![
                "smartevse_plug_latch",
                "buzz_mode_stempel",
                "buzz_laadweigering_reset",
                "goedkoop_laden",
                "ev_hoger_laadstil_beheer",
                "batterij_naar_auto_beheer",
                "ev_laden_buzz",
                "ev_laden_cable",
                "ev_laden_buzz_handshake_herstel",
                "ev_laden_buzz_reset_op_uitplug",
                "smartevse_pilot_arm",
                "ev_cable_autoheal",
                "breaker_guard",
                "peakguard_afschakelen",
                "peakguard_herstel",
                "soluna_dispatcher",
                "batterij_autonoom_netladen_stop",
                "dispatcher_off_target_alert",
                "solis_watchdog",
                "neg_injectie_curtail",
                "neg_injectie_herstel",
                "airco_dump_start",
                "airco_dump_stop",
                "nachtdekking_start",
                "nachtdekking_eind",
                "dagbalans_rapport",
                "entsoe_dayahead_morgen_ophalen",
                "entsoe_qh_kwartier_verversen",
                "capaciteit_piekhistoriek_bijwerken",
            ]
        );
        assert!(!ids.contains(&"vriezer_bewaking"), "disabled in HA");
    }
}
