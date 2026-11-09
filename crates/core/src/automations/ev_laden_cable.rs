//! Port of HA automation `ev_laden_cable` ("EV laden EV3 (laadkabel)",
//! `automation.ev_laden_ev3_laadkabel`): the EV3's modulator on the Tuya cable. The target is the
//! shared law in [`super::ev_modulator`]; this file has the cable's triggers, conditions and
//! actions.
//!
//! The cable is a real 1 A device (6–16 A), so the shared target is applied as
//! `[6, target|round(0)|int]|max` — `round`, not `int`, or a 9.8 A target would under-draw at 9 A
//! (LOGIC.md "Charge-current resolution"). Jinja's `round` ties to even: 8.5 A → 8 A.
//!
//! Note the two different "drawing" tests in the live automation: the law inside `target` uses
//! the cable *current* (`> 0.5 A`), while the action-level `drawing` variable uses the cable
//! *power* (`> 0.2 kW`).
//!
//! A full EV3 (`charged`) that does not draw gets `script.laadkabel_start_laden` at most every
//! [`START_RETRY_S`]; the script's `last_triggered` is kept under [`START_SCRIPT_ID`] in
//! `Memory::last_triggered`, set whenever this automation sends [`Command::CableStart`] (it is
//! the script's only caller).
use super::ev_modulator::{ChargerView, FLOOR_A, debounce_ok, law_cx, observe_risk, triggered};
use super::{Automation, Cx};
use crate::cascade;
use crate::command::Command;
use crate::jinja::{int, max2, round};
use crate::world::World;

#[derive(Debug, Default)]
pub struct EvLadenCable;

/// The `Memory::last_triggered` key of `script.laadkabel_start_laden` (an HA script, not an
/// automation id).
pub const START_SCRIPT_ID: &str = "script.laadkabel_start_laden";
/// A `charged` EV3 is re-sent the start script only when it last ran more than this long ago.
pub const START_RETRY_S: f64 = 1800.0;

/// The automation's own `drawing` variable: `states('sensor.ev_charging_cable_vermogen')|float(0) > 0.2`.
pub fn drawing(w: &World) -> bool {
    cascade::ev3_power_above(w, 0.2)
}

/// `[6, target|float(6)|round(0)|int]|max`: whole amps for the cable.
pub fn amps(target: f64) -> f64 {
    max2(FLOOR_A, int(round(target, 0)) as f64)
}

/// `not drawing and (status != 'charged' or last_triggered is none or (now − last_triggered)
/// > 1800)`: run the start script. `since_start_s` is the seconds since the script last ran.
pub fn should_start(w: &World, since_start_s: Option<f64>) -> bool {
    !drawing(w) && (w.cable.status.as_deref() != Some("charged") || since_start_s.is_none_or(|s| s > START_RETRY_S))
}

/// The actions for a rendered `target` (A); `since_start_s` as in [`should_start`].
pub fn actions(target: f64, w: &World, since_start_s: Option<f64>) -> Vec<Command> {
    let mut out = Vec::new();
    let set = w.cable.set_current_a;
    if target >= FLOOR_A {
        let a = amps(target);
        if should_start(w, since_start_s) {
            out.push(Command::CableStart { amps: a as u8 });
        } else if set.unwrap_or(-1.0) != a {
            out.push(Command::CableSetCurrent(a as u8));
        }
    } else {
        if drawing(w) {
            out.push(Command::CableStop);
        }
        if set.unwrap_or(6.0) != 6.0 {
            out.push(Command::CableSetCurrent(6));
        }
    }
    out
}

impl Automation for EvLadenCable {
    fn id(&self) -> &'static str {
        "ev_laden_cable"
    }

    fn step(&mut self, cx: &mut Cx) {
        observe_risk(cx);
        if !triggered(cx, cx.w.cable.status != cx.pw.cable.status) {
            return;
        }
        if !debounce_ok(cx, self.id()) || !cascade::ev3_ctrl(cx.w) {
            return;
        }
        cx.mark(self.id());
        let target = law_cx(&ChargerView::cable(cx.w, cx.m), cx).target;
        for c in actions(target, cx.w, cx.since_triggered(START_SCRIPT_ID)) {
            if matches!(c, Command::CableStart { .. }) {
                cx.mark(START_SCRIPT_ID);
            }
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
    use crate::time::at;

    /// The EV3 first in line, charging at 8 A (8 A measured, 1.84 kW) with 460 W export
    /// → target 10.0 A. The Buzz is not plugged.
    fn rig() -> Rig {
        let mut r = Rig::new();
        r.w.cable.status = Some("charging".into());
        r.w.cable.set_current_a = Some(8.0);
        r.w.cable.set_current_max_a = Some(16.0);
        r.w.cable.current_a = Some(8.0);
        r.w.cable.voltage_v = Some(230.0);
        r.w.cable.power_kw = Some(1.84);
        r.w.evse.plug = Some("Disconnected".into());
        r.w.evse.state_id = Some("A".into());
        r.w.evse.override_a = Some(6.0);
        r.w.solis.soc = Some(99.0);
        r.d.sun_above_horizon = true;
        r.d.pv_overschot_beschikbaar_w = 460.0;
        r.d.pv_bruto_overschot_w = 2300.0;
        r.d.stroomprijs_nu = Some(0.40);
        r.s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        r.s.ev_net_niveau = NetNiveau::Uit;
        r.s.ev_batterij_niveau = BatterijNiveau::Uit;
        r.s.batterij_reservering_actief = false;
        r
    }

    fn first_run(r: &mut Rig, a: &mut EvLadenCable) -> Vec<Command> {
        assert!(r.run(a).is_empty(), "nothing fires on the start tick");
        r.wait(20);
        r.run(a)
    }

    /// Drawing: set the new current when it differs (10.0 → 10 A).
    #[test]
    fn sets_the_rounded_current_while_drawing() {
        let mut r = rig();
        let mut a = EvLadenCable;
        assert_eq!(first_run(&mut r, &mut a), vec![Command::CableSetCurrent(10)]);
        assert_eq!(r.m.last_triggered("ev_laden_cable"), Some(r.now));
    }

    /// `round(0)`, not `int`: 9.8 → 10; ties to even: 8.5 → 8, 9.5 → 10; never below 6.
    #[test]
    fn whole_amps_round_ties_to_even() {
        assert_eq!(amps(9.8), 10.0);
        assert_eq!(amps(8.5), 8.0);
        assert_eq!(amps(9.5), 10.0);
        assert_eq!(amps(6.0), 6.0);
        assert_eq!(amps(5.4), 6.0);
    }

    /// Same current: nothing to do. Unknown set current (|float(-1)) is re-sent.
    #[test]
    fn no_command_when_the_current_already_matches() {
        let mut w = rig().w;
        w.cable.set_current_a = Some(10.0);
        assert!(actions(10.2, &w, None).is_empty());
        w.cable.set_current_a = None;
        assert_eq!(actions(10.2, &w, None), vec![Command::CableSetCurrent(10)]);
    }

    /// Not drawing (power ≤ 0.2 kW, whatever the current reads): start the cable at the amps.
    #[test]
    fn not_drawing_starts_the_cable() {
        let mut w = rig().w;
        w.cable.power_kw = Some(0.2);
        assert_eq!(actions(6.0, &w, None), vec![Command::CableStart { amps: 6 }]);
        w.cable.power_kw = None;
        assert_eq!(actions(11.6, &w, None), vec![Command::CableStart { amps: 12 }]);
    }

    /// `ev_laden_cable` start branch: a `charged` EV3 that does not draw is sent the start
    /// script only if it never ran or ran more than 1800 s ago; otherwise the else branch sets
    /// the current (8 → 10 A). Any other status starts at once, however recent the last start.
    #[test]
    fn a_charged_ev3_is_restarted_at_most_every_30_min() {
        let mut w = rig().w;
        w.cable.power_kw = Some(0.0);
        w.cable.status = Some("charged".into());
        let start = vec![Command::CableStart { amps: 10 }];
        assert_eq!(actions(10.0, &w, None), start, "never ran");
        assert_eq!(actions(10.0, &w, Some(1800.5)), start, "> 1800 s");
        assert_eq!(actions(10.0, &w, Some(1800.0)), vec![Command::CableSetCurrent(10)], "1800 s is not > 1800");
        w.cable.set_current_a = Some(10.0);
        assert!(actions(10.0, &w, Some(30.0)).is_empty(), "already at 10 A");
        w.cable.status = Some("plugged_in".into());
        assert_eq!(actions(10.0, &w, Some(30.0)), start, "not charged: no retry limit");
    }

    /// End to end: the start is recorded as the script's `last_triggered`, so the next run 30 s
    /// later with the EV3 still `charged` and idle only re-sets the current; 1801 s after the
    /// start it is sent again.
    #[test]
    fn end_to_end_the_start_script_is_rate_limited_on_a_charged_ev3() {
        let mut r = rig();
        r.w.cable.status = Some("charged".into());
        r.w.cable.power_kw = Some(0.0);
        r.w.cable.current_a = Some(0.0);
        r.w.cable.set_current_a = Some(6.0);
        r.d.pv_overschot_beschikbaar_w = 1840.0; // 1840 / 230 = 8 A
        let mut a = EvLadenCable;
        assert_eq!(first_run(&mut r, &mut a), vec![Command::CableStart { amps: 6 }]);
        assert_eq!(r.m.last_triggered(START_SCRIPT_ID), Some(r.now));
        r.wait(30);
        assert!(r.run(&mut a).is_empty(), "set current already 6, no new start");
        r.wait(1771);
        assert_eq!(r.run(&mut a), vec![Command::CableStart { amps: 6 }]);
    }

    /// Stop branch: stop if drawing, and park the set current at 6.
    #[test]
    fn stop_stops_and_parks_at_six() {
        let mut w = rig().w;
        assert_eq!(actions(0.0, &w, None), vec![Command::CableStop, Command::CableSetCurrent(6)]);
        w.cable.power_kw = Some(0.0);
        w.cable.set_current_a = Some(6.0);
        assert!(actions(5.9, &w, None).is_empty());
        w.cable.set_current_a = None;
        assert!(actions(0.0, &w, None).is_empty(), "|float(6)");
    }

    /// End to end: the Buzz ranked above and plugged, not maxed → the EV3 waits (stops).
    #[test]
    fn end_to_end_yields_to_a_hungry_higher_car() {
        let mut r = rig();
        r.s.zonprioriteit = Zonprioriteit([Buzz, Ev3, Batterij]);
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.override_a = Some(10.0);
        let mut a = EvLadenCable;
        assert_eq!(first_run(&mut r, &mut a), vec![Command::CableStop, Command::CableSetCurrent(6)]);
    }

    /// Triggers: the /30 pattern, the shared state changes, and the cable status.
    #[test]
    fn triggers_on_cable_status() {
        let mut r = rig();
        let mut a = EvLadenCable;
        r.run(&mut a);
        r.wait(5);
        assert!(r.run(&mut a).is_empty());
        r.wait(1);
        r.w.cable.status = Some("charged".into());
        r.run(&mut a);
        assert_eq!(r.m.last_triggered("ev_laden_cable"), Some(r.now));
    }

    /// Conditions: 25 s debounce and a reachable set-current entity.
    #[test]
    fn conditions() {
        let mut r = rig();
        let mut a = EvLadenCable;
        first_run(&mut r, &mut a);
        r.wait(10);
        r.w.cable.status = Some("charged".into());
        assert!(r.run(&mut a).is_empty(), "debounced");
        let mut r = rig();
        r.w.cable.set_current_a = None;
        assert!(first_run(&mut r, &mut a).is_empty());
        assert_eq!(r.m.last_triggered("ev_laden_cable"), None);
    }

    /// Risk is tracked by the cable modulator too (it may run without the Buzz one).
    #[test]
    fn risk_long_stops_the_cable() {
        let mut r = rig();
        let mut a = EvLadenCable;
        r.d.capaciteitspiek_risico = true;
        r.run(&mut a);
        assert_eq!(r.m.capaciteitspiek_risico_sinds, Some(at(2026, 11, 7, 12, 0, 10)));
        r.wait(20); // 20 s of risk: clamp to 6
        assert_eq!(r.run(&mut a), vec![Command::CableSetCurrent(6)]);
        r.w.cable.set_current_a = Some(6.0);
        r.wait(60); // 80 s: stop
        assert_eq!(r.run(&mut a), vec![Command::CableStop]);
    }

    #[test]
    fn id() {
        assert_eq!(EvLadenCable.id(), "ev_laden_cable");
    }
}
