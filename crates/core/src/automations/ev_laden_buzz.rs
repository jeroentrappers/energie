//! Port of HA automation `ev_laden_buzz` ("EV laden Buzz (SmartEVSE)",
//! `automation.ev_laden_buzz_smartevse`): the Buzz's modulator. The target is the shared law in
//! [`super::ev_modulator`]; this file has the Buzz's triggers, conditions and actions.
//!
//! Actions (LOGIC.md "Charge-current resolution" and "Charge-session protection"):
//! - target ≥ 6 A: offer `Normal` if the charger is not in `Normal` and the last real mode change
//!   is at least [`MIN_PAUSE_S`] ago; while in `Normal`, publish the target over MQTT in tenths
//!   when it differs from the override by ≥ 0.05 A (the HA number entity would round to 1 A).
//! - otherwise: park the override at 6 A, and `Pause` only when [`buzz_pause_ok`] (never abort a
//!   live session within [`MIN_CHARGE_S`] of the last mode change).
use super::ev_modulator::{
    self, ChargerView, MIN_PAUSE_S, buzz_pause_ok, debounce_ok, law_cx, observe_risk, since_mode_change, triggered,
};
use super::{Automation, Cx};
use crate::cascade;
use crate::command::Command;
use crate::jinja::{int, round};
use crate::world::{EvseMode, World};

#[derive(Debug, Default)]
pub struct EvLadenBuzz;

/// The actions for a rendered `target` (A), given the seconds since the last real mode change.
pub fn actions(target: f64, w: &World, since_mode_change_s: f64) -> Vec<Command> {
    let mut out = Vec::new();
    let normal = w.evse.mode == Some(EvseMode::Normal);
    if target >= ev_modulator::FLOOR_A {
        if !normal && since_mode_change_s >= MIN_PAUSE_S {
            out.push(Command::EvseMode(EvseMode::Normal));
        }
        // The `Normal` just sent is not visible yet: the override follows on a later run, once the
        // charger reports `Normal` (HA reads the select's state, which updates on the MQTT echo).
        if normal && (w.evse.override_a.unwrap_or(-1.0) - target).abs() >= 0.05 {
            out.push(Command::EvseCurrentDeciA(int(round(target * 10.0, 0)) as u32));
        }
    } else {
        if w.evse.override_a.unwrap_or(6.0) != 6.0 {
            out.push(Command::EvseCurrentDeciA(60));
        }
        if buzz_pause_ok(w, since_mode_change_s) {
            out.push(Command::EvseMode(EvseMode::Pause));
        }
    }
    out
}

impl Automation for EvLadenBuzz {
    fn id(&self) -> &'static str {
        "ev_laden_buzz"
    }

    fn step(&mut self, cx: &mut Cx) {
        observe_risk(cx);
        if !triggered(cx, cx.w.evse.plug != cx.pw.evse.plug) {
            return;
        }
        // `has_value('sensor.ev_laadvermogen_smartevse_w')` follows that sensor's availability
        // template (`Derived::ev_laadvermogen_smartevse_beschikbaar`).
        if !debounce_ok(cx, self.id())
            || !cascade::buzz_ctrl(cx.w)
            || !cx.d.ev_laadvermogen_smartevse_beschikbaar
            || cx.w.evse.state.is_none()
        {
            return;
        }
        cx.mark(self.id());
        let target = law_cx(&ChargerView::buzz(cx.w, cx.d, cx.m), cx).target;
        for c in actions(target, cx.w, since_mode_change(cx)) {
            cx.send(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;
    use crate::settings::Consumer::*;
    use crate::settings::{BatterijNiveau, NetNiveau, Zonprioriteit};
    use chrono::Duration;

    /// The Buzz charging at 10 A in Normal on a sunny noon with 1150 W export (target 13.0 A).
    fn rig() -> Rig {
        let mut r = Rig::new();
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.state = Some("Charging".into());
        r.w.evse.state_id = Some("C".into());
        r.w.evse.mode = Some(EvseMode::Normal);
        r.w.evse.override_a = Some(10.0);
        r.w.evse.override_max_a = Some(29.0);
        r.w.solis.soc = Some(99.0);
        r.w.cable.status = Some("available".into());
        r.d.sun_above_horizon = true;
        r.d.pv_overschot_beschikbaar_w = 1150.0;
        r.d.pv_bruto_overschot_w = 3450.0;
        r.d.ev_laadvermogen_smartevse_w = 2300.0;
        r.d.ev_laadvermogen_smartevse_beschikbaar = true;
        r.d.stroomprijs_nu = Some(0.40);
        r.s.zonprioriteit = Zonprioriteit([Buzz, Batterij, Ev3]);
        r.s.ev_net_niveau = NetNiveau::Uit;
        r.s.ev_batterij_niveau = BatterijNiveau::Uit;
        r.s.batterij_reservering_actief = false;
        r
    }

    /// Start, then advance to the next /30 s boundary (12:00:30) and run.
    fn first_run(r: &mut Rig, a: &mut EvLadenBuzz) -> Vec<Command> {
        let start = r.run(a);
        assert!(start.is_empty(), "nothing fires on the start tick");
        r.wait(20);
        r.run(a)
    }

    /// Normal and a different override: publish the target in tenths (13.0 A → 130).
    #[test]
    fn publishes_the_target_in_tenths_over_mqtt() {
        let mut r = rig();
        let mut a = EvLadenBuzz;
        assert_eq!(first_run(&mut r, &mut a), vec![Command::EvseCurrentDeciA(130)]);
        assert_eq!(r.m.last_triggered("ev_laden_buzz"), Some(r.now));
    }

    /// Condition `has_value('sensor.ev_laadvermogen_smartevse_w')`: while that sensor's
    /// availability template is false the modulator does not run (and records no run).
    #[test]
    fn waits_while_the_smartevse_power_sensor_is_unavailable() {
        let mut r = rig();
        r.d.ev_laadvermogen_smartevse_beschikbaar = false;
        let mut a = EvLadenBuzz;
        assert!(first_run(&mut r, &mut a).is_empty());
        assert_eq!(r.m.last_triggered("ev_laden_buzz"), None);
    }

    /// The tenths payload is `(target × 10)|round(0)|int`: 12.2 → 122.
    #[test]
    fn tenths_payload_rounds() {
        let w = rig().w;
        assert_eq!(actions(12.2, &w, 99999.0), vec![Command::EvseCurrentDeciA(122)]);
        assert_eq!(actions(14.5, &w, 99999.0), vec![Command::EvseCurrentDeciA(145)]);
    }

    /// No publish when the override is within 0.05 A of the target; 0.1 A apart is published.
    #[test]
    fn no_publish_within_five_hundredths() {
        let mut w = rig().w;
        w.evse.override_a = Some(13.0);
        assert!(actions(13.0, &w, 99999.0).is_empty());
        w.evse.override_a = Some(13.1);
        assert_eq!(actions(13.0, &w, 99999.0), vec![Command::EvseCurrentDeciA(130)]);
        w.evse.override_a = None;
        assert_eq!(actions(13.0, &w, 99999.0), vec![Command::EvseCurrentDeciA(130)], "|float(-1)");
    }

    /// Not Normal: offer Normal only ≥ 60 s after the last real mode change, and publish
    /// nothing until the charger reports Normal.
    #[test]
    fn normal_is_offered_only_60_s_after_the_last_mode_change() {
        let mut w = rig().w;
        w.evse.mode = Some(EvseMode::Pause);
        assert!(actions(8.0, &w, 59.0).is_empty());
        assert_eq!(actions(8.0, &w, 60.0), vec![Command::EvseMode(EvseMode::Normal)]);
        w.evse.mode = None;
        assert_eq!(actions(8.0, &w, 99999.0), vec![Command::EvseMode(EvseMode::Normal)], "unavailable");
    }

    /// The minimum-pause timer reads `buzz_mode_stempel`'s last run; never stamped = 99999 s.
    #[test]
    fn the_pause_timer_is_the_mode_stamp() {
        let mut r = rig();
        r.w.evse.mode = Some(EvseMode::Pause);
        let mut a = EvLadenBuzz;
        r.m.mark_triggered("buzz_mode_stempel", r.now - Duration::seconds(30));
        assert!(first_run(&mut r, &mut a).is_empty(), "paused 50 s ago");
        r.wait(30);
        assert_eq!(r.run(&mut a), vec![Command::EvseMode(EvseMode::Normal)], "paused 80 s ago");
        let mut r = rig();
        r.w.evse.mode = Some(EvseMode::Pause);
        assert_eq!(first_run(&mut r, &mut a), vec![Command::EvseMode(EvseMode::Normal)], "never stamped");
    }

    /// Stop branch: park the override at 6 A (payload 60), and Pause when allowed.
    #[test]
    fn stop_parks_at_six_and_pauses_when_allowed() {
        let w = rig().w;
        assert_eq!(actions(0.0, &w, 300.0), vec![Command::EvseCurrentDeciA(60), Command::EvseMode(EvseMode::Pause)]);
        let mut w6 = w.clone();
        w6.evse.override_a = Some(6.0);
        assert_eq!(actions(5.9, &w6, 300.0), vec![Command::EvseMode(EvseMode::Pause)]);
        w6.evse.override_a = None;
        assert_eq!(actions(0.0, &w6, 300.0), vec![Command::EvseMode(EvseMode::Pause)], "|float(6)");
    }

    /// Pause is not sent within 300 s of the last mode change while charging in Normal: the
    /// car is throttled to 6 A instead.
    #[test]
    fn pause_is_not_sent_within_300_s_while_charging() {
        let w = rig().w;
        assert_eq!(actions(0.0, &w, 299.0), vec![Command::EvseCurrentDeciA(60)]);
        let mut paused = w.clone();
        paused.evse.mode = Some(EvseMode::Pause);
        paused.evse.override_a = Some(6.0);
        assert!(actions(0.0, &paused, 99999.0).is_empty(), "already paused");
    }

    /// End to end: no sun → the session is stopped, but only after its 300 s.
    #[test]
    fn end_to_end_stop_respects_session_protection() {
        let mut r = rig();
        r.d.sun_above_horizon = false;
        r.m.mark_triggered("buzz_mode_stempel", r.now - Duration::seconds(100));
        let mut a = EvLadenBuzz;
        assert_eq!(first_run(&mut r, &mut a), vec![Command::EvseCurrentDeciA(60)]);
        r.w.evse.override_a = Some(6.0);
        r.wait(180);
        assert_eq!(r.run(&mut a), vec![Command::EvseMode(EvseMode::Pause)], "300 s after the change");
    }

    /// Risk: 6 A at once, Pause only once risk has been on > 60 s (and the session is old).
    #[test]
    fn end_to_end_risk_throttles_then_stops() {
        let mut r = rig();
        let mut a = EvLadenBuzz;
        r.run(&mut a);
        r.wait(20); // 12:00:30
        r.d.capaciteitspiek_risico = true;
        assert_eq!(r.run(&mut a), vec![Command::EvseCurrentDeciA(60)], "throttle to the floor");
        r.w.evse.override_a = Some(6.0);
        r.wait(30); // 30 s of risk
        assert!(r.run(&mut a).is_empty(), "at 6 A already, not yet long");
        r.wait(30); // 60 s
        assert!(r.run(&mut a).is_empty(), "60 s is not > 60");
        r.wait(30); // 90 s
        assert_eq!(r.run(&mut a), vec![Command::EvseMode(EvseMode::Pause)]);
    }

    /// Triggers: the /30 s pattern and state changes; nothing in between.
    #[test]
    fn triggers() {
        let mut r = rig();
        let mut a = EvLadenBuzz;
        r.run(&mut a);
        r.wait(5); // 12:00:15, no pattern
        assert!(r.run(&mut a).is_empty());
        assert_eq!(r.m.last_triggered("ev_laden_buzz"), None);
        r.wait(1);
        r.d.pv_overschot_beschikbaar_w = 1160.0;
        assert!(!r.run(&mut a).is_empty(), "pv_overschot_beschikbaar changed");
        for change in 0..4 {
            let mut r = rig();
            r.run(&mut a);
            r.wait(5);
            match change {
                0 => r.s.ev_net_niveau = NetNiveau::Brug6A,
                1 => r.s.ev_batterij_niveau = BatterijNiveau::Brug6A,
                2 => r.s.zonprioriteit = Zonprioriteit([Buzz, Ev3, Batterij]),
                _ => r.w.evse.plug = None,
            }
            r.run(&mut a);
            assert_eq!(r.m.last_triggered("ev_laden_buzz"), Some(r.now), "change {change}");
        }
    }

    /// Conditions: 25 s debounce on its own last run, a reachable override and a known state.
    #[test]
    fn conditions() {
        let mut r = rig();
        let mut a = EvLadenBuzz;
        first_run(&mut r, &mut a);
        r.wait(25);
        r.d.pv_overschot_beschikbaar_w = 1200.0;
        assert!(r.run(&mut a).is_empty(), "25 s is not > 25");
        r.wait(1);
        r.d.pv_overschot_beschikbaar_w = 1210.0;
        assert!(!r.run(&mut a).is_empty(), "26 s");
        let mut r = rig();
        r.w.evse.override_a = None;
        assert!(first_run(&mut r, &mut a).is_empty());
        assert_eq!(r.m.last_triggered("ev_laden_buzz"), None, "no run, no last_triggered");
        let mut r = rig();
        r.w.evse.state = None;
        assert!(first_run(&mut r, &mut a).is_empty());
    }

    #[test]
    fn id() {
        assert_eq!(EvLadenBuzz.id(), "ev_laden_buzz");
    }
}
