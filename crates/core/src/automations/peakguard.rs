//! Port of HA automations `peakguard_afschakelen` and `peakguard_herstel`. See
//! LOGIC.md "Safety" (Peak guard: shed) and
//! "Quarter-hour budget control" (the guard window lives in `binary_sensor.capaciteitspiek_risico`,
//! i.e. `Derived::capaciteitspiek_risico`).
//!
//! `afschakelen` sheds flexible → comfort while the quarter-hour projection is at risk: one
//! `choose` step per iteration of a `repeat while risk and index <= 15`, 20 s apart. Each
//! iteration re-reads the live states, so a step that took effect moves the next iteration on
//! to the next consumer. `mode: single`: a trigger while the loop runs is dropped.

use chrono::Duration;

use super::{Automation, Cx};
use crate::cascade;
use crate::command::{AircoUnit, Command};
use crate::jinja;
use crate::jinja::py_float;
use crate::time::Ts;
use crate::trigger::Held;
use crate::world::{EvseMode, RcMode};

/// `repeat.index <= 15`.
const MAX_ITERATIONS: u32 = 15;
/// The `delay` between iterations.
const ITERATION_S: i64 = 20;
/// The Buzz session protection: no Pause within 300 s of a real mode change.
const SESSION_PROTECT_S: f64 = 300.0;
/// The mode stamp's `last_triggered`. HA reads it as `automation.buzz_modewissel_stempel` (the
/// entity id comes from the alias), but `Cx::mark` keys by automation id, which is
/// `buzz_mode_stempel` (`BuzzModeStempel::id`).
const BUZZ_MODE_STEMPEL: &str = "buzz_mode_stempel";

/// The repeat loop in progress: the `repeat.index` of the next iteration and when its delay
/// ends.
#[derive(Debug, Clone, PartialEq)]
struct Loop {
    index: u32,
    due: Ts,
}

#[derive(Debug, Default)]
pub struct PeakguardAfschakelen {
    /// `binary_sensor.capaciteitspiek_risico` to `on` for 10 s.
    trigger: Held,
    /// `Some` while the actions run (in the repeat loop or its trailing delay).
    running: Option<Loop>,
}

impl PeakguardAfschakelen {
    /// The `while` check, then one `choose` step and the delay; or the end of the run.
    fn iterate(&mut self, cx: &mut Cx, index: u32) {
        if !(cx.d.capaciteitspiek_risico && index <= MAX_ITERATIONS) {
            self.running = None;
            return;
        }
        shed_one(cx);
        self.running = Some(Loop { index: index + 1, due: cx.now + Duration::seconds(ITERATION_S) });
    }
}

/// One iteration's `choose`: the first consumer that can still give something up.
fn shed_one(cx: &mut Cx) {
    let w = cx.w;
    let buzz_charging = cascade::buzz_drawing(w);
    let cable_kw = w.cable.power_kw.unwrap_or(0.0);
    if buzz_charging && w.evse.override_a.unwrap_or(6.0) > 6.0 {
        // `([6, ov|float(8) - 3]|max * 10)|round(0)|int`; ov has a value here (> 6).
        let ov = w.evse.override_a.unwrap_or(8.0);
        let deci = jinja::round(jinja::max2(6.0, ov - 3.0) * 10.0, 0) as u32;
        cx.send(Command::EvseCurrentDeciA(deci));
    } else if buzz_charging && session_may_pause(cx) {
        cx.send(Command::EvseMode(EvseMode::Pause));
    } else if let Some(cur) = w.cable.set_current_a.filter(|&cur| cable_kw > 0.3 && cur > 6.0) {
        // `has_value(number.ev_charging_cable_charge_current) and …|float(16) > 6`: with the
        // number unavailable this step is skipped and the stop branch below gets its turn.
        cx.send(Command::CableSetCurrent(jinja::max2(6.0, cur - 3.0) as u8));
    } else if cable_kw > 0.3 {
        cx.send(Command::CableStop);
    } else if !w.no_battery && w.solis.rc_mode != Some(RcMode::ForceDischarge) && w.solis.soc.unwrap_or(0.0) > 25.0 {
        cx.send(Command::SolisRcTimeoutMin(10));
        cx.send(Command::SolisRcDischargeW(2500));
        cx.send(Command::SolisRcMode(RcMode::ForceDischarge));
        cx.send(Command::notify("⚡ Peak guard", "Batterij-ontlaadboost geactiveerd (2,5 kW, max 10 min)."));
    } else if w.airco.beneden_on == Some(true) || w.airco.zolder_on == Some(true) {
        cx.send(Command::Airco { unit: AircoUnit::Beneden, on: false });
        cx.send(Command::Airco { unit: AircoUnit::Zolder, on: false });
        cx.m.airco_dump_sessie_bezig = false;
    }
}

/// `not Charging or not mode Normal or (now − last_triggered of the mode stamp) >= 300`, with
/// a never-triggered stamp reading 99999 s. Only called while the Buzz is charging, so the
/// first disjunct is false here.
fn session_may_pause(cx: &Cx) -> bool {
    let since = cx.since_triggered(BUZZ_MODE_STEMPEL).unwrap_or(99999.0);
    cx.w.evse.mode != Some(EvseMode::Normal) || since >= SESSION_PROTECT_S
}

impl Automation for PeakguardAfschakelen {
    fn id(&self) -> &'static str {
        "peakguard_afschakelen"
    }

    fn step(&mut self, cx: &mut Cx) {
        let fired = self.trigger.update(cx.d.capaciteitspiek_risico, cx.now, 10.0);
        if let Some(l) = &self.running {
            if cx.now >= l.due {
                let index = l.index;
                self.iterate(cx, index);
            }
            return;
        }
        if !(fired && cx.s.peak_guard_actief) {
            return;
        }
        cx.mark(self.id());
        cx.m.peak_guard_interventies += 1;
        // `(states(projectie)|float(0) / 1000)|round(2)`: the state is in W, the text in kW.
        let kw = jinja::round(cx.d.kwartiervermogen_projectie_w / 1000.0, 2);
        let msg = format!("Projectie {} kW — afschakelen gestart.", py_float(kw));
        cx.send(Command::notify("⚡ Peak guard", msg));
        self.iterate(cx, 1);
    }
}

#[derive(Debug, Default)]
pub struct PeakguardHerstel {
    /// `sensor.kwartiervermogen_projectie` below 2000 (W) for 5 min.
    trigger: Held,
}

impl Automation for PeakguardHerstel {
    fn id(&self) -> &'static str {
        "peakguard_herstel"
    }

    fn step(&mut self, cx: &mut Cx) {
        // `below: 2000`: the state is in W (the template renders kW, HA converts), so 2.0 kW.
        let fired = self.trigger.update(cx.d.kwartiervermogen_projectie_w < 2000.0, cx.now, 300.0);
        if !(fired && cx.s.peak_guard_actief && cx.w.solis.rc_mode == Some(RcMode::ForceDischarge)) {
            return;
        }
        cx.mark(self.id());
        cx.send(Command::SolisRcMode(RcMode::None));
        cx.send(Command::SolisRcDischargeW(0));
        cx.send(Command::notify(
            "✅ Peak guard",
            "Projectie weer veilig — ontlaadboost gestopt; laden hervat vanzelf.",
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::super::dagbalans::rig::Rig;
    use super::*;
    use crate::time::at;

    const BOOST_NOTE: &str = "Batterij-ontlaadboost geactiveerd (2,5 kW, max 10 min).";

    /// 12:05:00 (well inside the guard window; the window itself is the risk sensor's job).
    fn rig() -> Rig {
        let mut r = Rig::new(at(2026, 7, 1, 12, 5, 0));
        r.d.kwartiervermogen_projectie_w = 3142.0;
        r
    }

    /// Prime the trigger (risk off), then hold the risk on for 10 s; returns the commands of
    /// the tick it fires on, 11 s after the start.
    fn arm(r: &mut Rig, a: &mut PeakguardAfschakelen) -> Vec<Command> {
        r.d.capaciteitspiek_risico = false;
        assert!(r.step(a).is_empty());
        r.d.capaciteitspiek_risico = true;
        // True from the first tick on; fires on the tick where it has held 10 s.
        for _ in 0..10 {
            assert!(r.tick(a, 1).is_empty(), "the hold is 10 s");
        }
        r.tick(a, 1)
    }

    fn start_note() -> Command {
        Command::notify("⚡ Peak guard", "Projectie 3.14 kW — afschakelen gestart.")
    }

    /// `peakguard_afschakelen`: after 10 s of risk the counter goes up, the start note prints
    /// the projection in kW (3142 W / 1000 = 3.142 → round(2) 3.14), and the first shed step
    /// runs at once: Buzz 14.5 A − 3 = 11.5 A → payload 115.
    #[test]
    fn the_peak_guard_sheds_the_buzz_first_after_10_s_of_risk() {
        let mut r = rig();
        r.w.evse.state = Some("Charging".into());
        r.w.evse.override_a = Some(14.5);
        let mut a = PeakguardAfschakelen::default();
        assert_eq!(arm(&mut r, &mut a), vec![start_note(), Command::EvseCurrentDeciA(115)]);
        assert_eq!(r.m.peak_guard_interventies, 1);
        assert_eq!(r.m.last_triggered("peakguard_afschakelen"), Some(r.now));
    }

    /// The `peak_guard_actief` switch gates the whole run.
    #[test]
    fn a_disabled_peak_guard_does_nothing() {
        let mut r = rig();
        r.s.peak_guard_actief = false;
        r.w.evse.state = Some("Charging".into());
        r.w.evse.override_a = Some(14.5);
        let mut a = PeakguardAfschakelen::default();
        assert!(arm(&mut r, &mut a).is_empty());
        assert_eq!(r.m.peak_guard_interventies, 0);
    }

    /// LOGIC.md "Peak guard — Shed": flexible → comfort, re-evaluated each iteration. As each
    /// step takes effect the next iteration (20 s later) moves down the ladder: Buzz −3 A
    /// (7.0 → 6 A floor, payload 60), Buzz Pause, EV3 −3 A (10 → 7), EV3 −3 A (7 → 6), EV3
    /// stop, battery boost, aircos off (clearing the dump session), then nothing left.
    #[test]
    fn the_peak_guard_walks_down_the_ladder_one_step_per_iteration() {
        let mut r = rig();
        r.w.evse.state = Some("Charging".into());
        r.w.evse.mode = Some(EvseMode::Normal);
        r.w.evse.override_a = Some(7.0);
        r.w.cable.power_kw = Some(2.3);
        r.w.cable.set_current_a = Some(10.0);
        r.w.solis.soc = Some(60.0);
        r.w.solis.rc_mode = Some(RcMode::None);
        r.w.airco.beneden_on = Some(false);
        r.w.airco.zolder_on = Some(true);
        r.m.airco_dump_sessie_bezig = true;
        let mut a = PeakguardAfschakelen::default();
        assert_eq!(arm(&mut r, &mut a), vec![start_note(), Command::EvseCurrentDeciA(60)]);

        // Not yet: the delay is 20 s.
        assert!(r.tick(&mut a, 19).is_empty());
        r.w.evse.override_a = Some(6.0);
        assert_eq!(r.tick(&mut a, 1), vec![Command::EvseMode(EvseMode::Pause)]);

        r.w.evse.state = Some("Charging Stopped".into());
        assert_eq!(r.tick(&mut a, 20), vec![Command::CableSetCurrent(7)]);
        r.w.cable.set_current_a = Some(7.0);
        assert_eq!(r.tick(&mut a, 20), vec![Command::CableSetCurrent(6)]);
        r.w.cable.set_current_a = Some(6.0);
        assert_eq!(r.tick(&mut a, 20), vec![Command::CableStop]);

        r.w.cable.power_kw = Some(0.0);
        assert_eq!(
            r.tick(&mut a, 20),
            vec![
                Command::SolisRcTimeoutMin(10),
                Command::SolisRcDischargeW(2500),
                Command::SolisRcMode(RcMode::ForceDischarge),
                Command::notify("⚡ Peak guard", BOOST_NOTE),
            ]
        );
        r.w.solis.rc_mode = Some(RcMode::ForceDischarge);
        assert_eq!(
            r.tick(&mut a, 20),
            vec![
                Command::Airco { unit: AircoUnit::Beneden, on: false },
                Command::Airco { unit: AircoUnit::Zolder, on: false },
            ]
        );
        assert!(!r.m.airco_dump_sessie_bezig);
        r.w.airco.zolder_on = Some(false);
        assert!(r.tick(&mut a, 20).is_empty(), "nothing left to shed");
        assert_eq!(r.m.peak_guard_interventies, 1, "one intervention per run, not per step");
    }

    /// `peakguard_afschakelen` repeat `while risk on and repeat.index <= 15`: with nothing
    /// sheddable the loop runs its 15 iterations (14 × 20 s after the first, plus the
    /// trailing delay) and ends; while it runs a new trigger is dropped (`mode: single`).
    #[test]
    fn the_peak_guard_loop_stops_after_15_iterations_or_when_the_risk_clears() {
        // No consumer to shed, but the boost branch is re-sent each iteration because the
        // inverter never reports ForceDischarge: that makes the iterations countable.
        let mut r = rig();
        r.w.solis.soc = Some(60.0);
        let mut a = PeakguardAfschakelen::default();
        let first = arm(&mut r, &mut a);
        assert_eq!(first.len(), 5, "note + boost");
        let mut boosts = 1;
        for _ in 0..20 {
            let out = r.tick(&mut a, 20);
            boosts += out.iter().filter(|c| **c == Command::SolisRcMode(RcMode::ForceDischarge)).count();
        }
        assert_eq!(boosts, 15);
        assert_eq!(a.running, None, "the run is over");
        assert_eq!(r.m.peak_guard_interventies, 1, "a risk that stays on does not start a second run");

        // A fresh run, and the risk clears after the first step: the next check ends it.
        r.d.capaciteitspiek_risico = false;
        let mut a = PeakguardAfschakelen::default();
        assert_eq!(arm(&mut r, &mut a).len(), 5);
        assert_eq!(r.m.peak_guard_interventies, 2);
        r.d.capaciteitspiek_risico = false;
        assert!(r.tick(&mut a, 20).is_empty());
        assert_eq!(a.running, None);
    }

    /// LOGIC.md "Charge-session protection" via `peakguard_afschakelen` branch 2: a Buzz at its
    /// 6 A floor in Normal mode is not paused within 300 s of the last real mode change
    /// (`automation.buzz_modewissel_stempel` last_triggered), so the guard falls through to the
    /// EV3. The stamp ran 290 s before the guard fires; 20 s later the protection has expired
    /// and the next iteration pauses the Buzz.
    #[test]
    fn the_buzz_is_not_paused_within_300_s_of_a_mode_change() {
        let mut r = rig();
        r.w.evse.state = Some("Charging".into());
        r.w.evse.mode = Some(EvseMode::Normal);
        r.w.evse.override_a = Some(6.0);
        r.w.cable.power_kw = Some(1.4);
        r.w.cable.set_current_a = Some(6.0);
        // `arm` fires 11 s from now: 291 s after the stamp.
        r.m.mark_triggered("buzz_mode_stempel", r.now - Duration::seconds(280));
        let mut a = PeakguardAfschakelen::default();
        assert_eq!(arm(&mut r, &mut a), vec![start_note(), Command::CableStop]);
        assert_eq!(r.tick(&mut a, 20), vec![Command::EvseMode(EvseMode::Pause)], "311 s");
    }

    /// `mode: single` + `max_exceeded: silent`: a new trigger while the loop runs is dropped.
    /// The risk blips off and back on for 10 s inside one 20 s delay: no second start note.
    #[test]
    fn a_trigger_while_the_loop_runs_is_dropped() {
        let mut r = rig();
        let mut a = PeakguardAfschakelen::default();
        assert_eq!(arm(&mut r, &mut a), vec![start_note()], "nothing to shed");
        r.d.capaciteitspiek_risico = false;
        assert!(r.tick(&mut a, 1).is_empty());
        r.d.capaciteitspiek_risico = true;
        for _ in 0..10 {
            assert!(r.tick(&mut a, 1).is_empty());
        }
        assert_eq!(r.m.peak_guard_interventies, 1);
        assert!(a.running.is_some(), "still in the first run");
    }

    /// Branch 2 boundaries, checked directly on the `choose` step.
    #[test]
    fn session_protection_boundaries() {
        let mut r = rig();
        r.w.evse.state = Some("Charging".into());
        r.w.evse.mode = Some(EvseMode::Normal);
        r.w.cable.power_kw = Some(1.4);
        r.w.cable.set_current_a = Some(6.0);
        assert_eq!(shed(&mut r), vec![Command::EvseMode(EvseMode::Pause)], "stamp never ran: 99999 s");
        r.m.mark_triggered("buzz_mode_stempel", r.now - Duration::seconds(299));
        assert_eq!(shed(&mut r), vec![Command::CableStop], "299 s: protected, EV3 next");
        r.m.mark_triggered("buzz_mode_stempel", r.now - Duration::seconds(300));
        assert_eq!(shed(&mut r), vec![Command::EvseMode(EvseMode::Pause)], "300 s: may pause");
        r.m.mark_triggered("buzz_mode_stempel", r.now);
        r.w.evse.mode = Some(EvseMode::Solar);
        assert_eq!(shed(&mut r), vec![Command::EvseMode(EvseMode::Pause)], "not Normal: no protection");
    }

    /// Runs one `choose` step, for boundary tests.
    fn shed(r: &mut Rig) -> Vec<Command> {
        r.with_cx(shed_one).1
    }

    /// Boost branch: not when the inverter already force-discharges, not at 25 % SoC or
    /// below (`> 25`), and an unavailable SoC reads 0.
    #[test]
    fn the_discharge_boost_needs_soc_above_25_and_no_boost_running() {
        let mut r = rig();
        r.w.solis.soc = Some(25.0);
        assert!(shed(&mut r).is_empty());
        r.w.solis.soc = None;
        assert!(shed(&mut r).is_empty());
        r.w.solis.soc = Some(25.5);
        assert_eq!(shed(&mut r).len(), 4, "rc_mode unavailable counts as not discharging");
        r.w.solis.rc_mode = Some(RcMode::ForceDischarge);
        assert!(shed(&mut r).is_empty());
    }

    /// `peakguard_afschakelen` EV3 branches: `ev_charging_cable_vermogen > 0.3`, strict. A cable
    /// at exactly 0.3 kW is neither stepped down (13 A) nor stopped (6 A): the iteration falls
    /// through to the discharge boost.
    #[test]
    fn a_cable_at_exactly_0_3_kw_is_left_alone() {
        let mut r = rig();
        r.w.cable.power_kw = Some(0.3);
        r.w.solis.soc = Some(80.0);
        for set in [13.0, 6.0] {
            r.w.cable.set_current_a = Some(set);
            let out = shed(&mut r);
            assert_eq!(out.len(), 4, "{set} A: {out:?}");
            assert_eq!(out[2], Command::SolisRcMode(RcMode::ForceDischarge));
        }
        r.w.cable.power_kw = Some(0.31);
        assert_eq!(shed(&mut r), vec![Command::CableStop]);
        r.w.cable.set_current_a = Some(13.0);
        assert_eq!(shed(&mut r), vec![Command::CableSetCurrent(10)]);
    }

    /// `peakguard_afschakelen` EV3 −3 A branch requires `has_value(number.ev_charging_cable_
    /// charge_current)`: with the number unavailable (and the cable drawing 1.4 kW > 0.3) the
    /// iteration goes to the stop branch instead of a skipped `number.set_value`.
    #[test]
    fn an_unavailable_cable_current_stops_the_cable() {
        let mut r = rig();
        r.w.cable.power_kw = Some(1.4);
        r.w.cable.set_current_a = None;
        r.w.solis.soc = Some(80.0);
        assert_eq!(shed(&mut r), vec![Command::CableStop]);
    }

    /// The start note rounds the kW to 2 decimals and prints it as Python would (checked in
    /// Python): 0 W → `0.0`, 2500 W → `2.5`, 1995 W → 1.995 → `2.0`, 12 345 W → 12.345 (stored
    /// just above the tie) → `12.35`.
    #[test]
    fn the_start_note_prints_the_projection_in_kw_with_2_decimals() {
        for (w, text) in [(0.0, "0.0"), (2500.0, "2.5"), (1995.0, "2.0"), (12345.0, "12.35")] {
            let mut r = rig();
            r.d.kwartiervermogen_projectie_w = w;
            let mut a = PeakguardAfschakelen::default();
            let out = arm(&mut r, &mut a);
            assert_eq!(
                out[0],
                Command::notify("⚡ Peak guard", format!("Projectie {text} kW — afschakelen gestart.")),
                "{w} W"
            );
        }
    }

    /// `peakguard_herstel`: the projection must sit below 2000 W (2.0 kW) for 5 min with the
    /// boost running; then the boost is cancelled.
    #[test]
    fn the_boost_is_cancelled_after_5_min_below_2_kw() {
        let mut r = rig();
        r.w.solis.rc_mode = Some(RcMode::ForceDischarge);
        let mut a = PeakguardHerstel::default();
        r.d.kwartiervermogen_projectie_w = 2000.0;
        assert!(r.step(&mut a).is_empty(), "2000 is not below 2000; primes the trigger");
        r.d.kwartiervermogen_projectie_w = 1999.0;
        assert!(r.tick(&mut a, 1).is_empty());
        assert!(r.tick(&mut a, 299).is_empty(), "299 s");
        assert_eq!(
            r.tick(&mut a, 1),
            vec![
                Command::SolisRcMode(RcMode::None),
                Command::SolisRcDischargeW(0),
                Command::notify("✅ Peak guard", "Projectie weer veilig — ontlaadboost gestopt; laden hervat vanzelf."),
            ]
        );
        assert_eq!(r.m.last_triggered("peakguard_herstel"), Some(r.now));
    }

    /// `peakguard_herstel` `below: 2000`: a projection of 2500 W never cancels the boost,
    /// one of 1500 W does after its 5 min hold (the old `below: 2.0` read as 2 W).
    #[test]
    fn a_projection_of_1500_w_cancels_the_boost_and_2500_w_does_not() {
        let mut r = rig();
        r.w.solis.rc_mode = Some(RcMode::ForceDischarge);
        let mut a = PeakguardHerstel::default();
        r.d.kwartiervermogen_projectie_w = 3000.0;
        r.step(&mut a);
        r.d.kwartiervermogen_projectie_w = 2500.0;
        for _ in 0..10 {
            assert!(r.tick(&mut a, 60).is_empty(), "2500 W is not below 2000");
        }
        r.d.kwartiervermogen_projectie_w = 1500.0;
        assert!(r.tick(&mut a, 1).is_empty(), "the hold starts");
        assert!(r.tick(&mut a, 299).is_empty());
        assert_eq!(r.tick(&mut a, 1).len(), 3, "5 min below 2000 W");
    }

    /// `peakguard_herstel` conditions: the guard switch and a boost actually running.
    #[test]
    fn herstel_needs_the_guard_on_and_a_boost_running() {
        for (actief, mode) in [(false, Some(RcMode::ForceDischarge)), (true, Some(RcMode::None)), (true, None)] {
            let mut r = rig();
            r.s.peak_guard_actief = actief;
            r.w.solis.rc_mode = mode;
            let mut a = PeakguardHerstel::default();
            r.step(&mut a);
            r.d.kwartiervermogen_projectie_w = 0.0;
            r.tick(&mut a, 1);
            assert!(r.tick(&mut a, 300).is_empty(), "{actief} {mode:?}");
        }
    }
}
