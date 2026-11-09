//! Port of HA automation `breaker_guard` (Hoofdzekering-beveiliging per fase). See
//! LOGIC.md "Safety".
//!
//! Every 5 s, and whenever a phase current changes, while the worst conductor carries more than
//! 28 A in either direction: first throttle (one proportional step aims that conductor at
//! ~26 A), and only above 29 A, with the cars already at their floor, hard-cut in the order
//! Buzz → EV3 → battery grid-charge. The first matching branch wins (`choose`).

use super::{Automation, Cx};
use crate::cascade;
use crate::command::Command;
use crate::jinja;

/// The guard's trigger level: `max(|fase_n|) > 28`.
const ARM_A: f64 = 28.0;
/// Hard cuts only above this.
const CUT_A: f64 = 29.0;
/// The proportional step aims the worst conductor here.
const AIM_A: f64 = 26.0;

#[derive(Debug, Default)]
pub struct BreakerGuard;

impl Automation for BreakerGuard {
    fn id(&self) -> &'static str {
        "breaker_guard"
    }

    fn step(&mut self, cx: &mut Cx) {
        let phase_changed = (0..3).any(|i| cx.pw.p1.phase_a[i] != cx.w.p1.phase_a[i]);
        if !(cx.every(5) || phase_changed) {
            return;
        }
        let mp = cascade::maxphase_abs(cx.w);
        if mp <= ARM_A {
            return;
        }
        cx.mark(self.id());
        let w = cx.w;
        let ov = w.evse.override_a.unwrap_or(6.0);
        let cable_set = w.cable.set_current_a.unwrap_or(6.0);
        let cable_kw = w.cable.power_kw.unwrap_or(0.0);
        // `Fase ~{{ maxphase|round(0) }} A`: HA's round(0) returns an int.
        let fase = || format!("Fase ~{} A", jinja::round(mp, 0) as i64);

        // `is_state(smartevse_1234_state, 'Charging') and override|float(6) > 6`: only a Buzz
        // that draws is stepped down, so an idle one does not shadow the EV3 step below.
        if cascade::buzz_drawing(w) && ov > 6.0 {
            // `(([6, ov - (mp - 26)]|max|round(0))|float * 10)|round(0)|int`: whole amps
            // first (ties to even), then tenths for the MQTT override.
            let amps = jinja::round(jinja::max2(6.0, ov - (mp - AIM_A)), 0);
            let deci = jinja::round(amps * 10.0, 0) as u32;
            cx.send(Command::EvseCurrentDeciA(deci));
            cx.m.peak_guard_interventies += 1;
        } else if cable_kw > 0.2 && cable_set > 6.0 {
            let amps = jinja::round(jinja::max2(6.0, cable_set - (mp - AIM_A)), 0);
            cx.send(Command::CableSetCurrent(amps as u8));
            cx.m.peak_guard_interventies += 1;
        } else if mp > CUT_A && cascade::buzz_drawing(w) {
            cx.send(Command::EvseMode(crate::world::EvseMode::Pause));
            cx.m.peak_guard_interventies += 1;
            let msg = format!("{} — Buzz al op 6A, toch afgekapt (3x32A grens).", fase());
            cx.send(Command::notify("🛑 Hoofdzekering", msg));
        } else if mp > CUT_A && cable_kw > 0.3 {
            cx.send(Command::CableStop);
            cx.m.peak_guard_interventies += 1;
            let msg = format!("{} — EV3 al op 6A, toch afgekapt (3x32A grens).", fase());
            cx.send(Command::notify("🛑 Hoofdzekering", msg));
        } else if mp > CUT_A && cx.m.batterij_net_laden {
            cx.m.batterij_net_laden = false;
            cx.m.peak_guard_interventies += 1;
            let msg = format!(
                "{} — batterij-net-laden gepauzeerd (3x32A grens). Zet handmatig terug aan als veilig.",
                fase()
            );
            cx.send(Command::notify("🛑 Hoofdzekering", msg));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::dagbalans::rig::Rig;
    use super::*;
    use crate::time::at;
    use crate::world::EvseMode;

    /// A rig on a 5 s boundary (`/5` fires) with the phases already at `phases` last tick, so
    /// only the time pattern triggers.
    fn rig(phases: [f64; 3]) -> Rig {
        let mut r = Rig::new(at(2026, 7, 1, 12, 0, 5));
        r.w.p1.phase_a = phases.map(Some);
        r.pw.p1.phase_a = r.w.p1.phase_a;
        r
    }

    fn hoofdzekering(msg: &str) -> Command {
        Command::notify("🛑 Hoofdzekering", msg)
    }

    /// LOGIC.md Safety: "at > 28 A it first throttles the charging cars down fast (one
    /// proportional step aims the worst conductor back to ~26 A)". Buzz at 16 A, worst
    /// phase 30.5 A: 16 − (30.5 − 26) = 11.5 → round → 12 (ties to even) → payload 120.
    #[test]
    fn the_breaker_guard_throttles_before_it_cuts() {
        let mut r = rig([30.5, 10.0, -3.0]);
        r.w.evse.override_a = Some(16.0);
        r.w.evse.state = Some("Charging".into());
        r.w.cable.power_kw = Some(3.0);
        r.w.cable.set_current_a = Some(13.0);
        r.m.batterij_net_laden = true;
        let out = r.step(&mut BreakerGuard);
        assert_eq!(out, vec![Command::EvseCurrentDeciA(120)], "only the Buzz step, nothing cut");
        assert_eq!(r.m.peak_guard_interventies, 1);
        assert!(r.m.batterij_net_laden);
        assert_eq!(r.m.last_triggered("breaker_guard"), Some(r.now));
    }

    /// `breaker_guard` payload chain `[6, ov − (mp − 26)]|max|round(0)` then ×10: the step
    /// rounds to whole amps with ties to even before the tenths. Override 15.0, phase 30.5:
    /// 10.5 → 10 → 100 (not 105, not 110). Override 14.3, phase 28.4: 11.9 → 12 → 120.
    /// Override 7, phase 31: 2 → floored at 6 → 60.
    #[test]
    fn the_buzz_step_rounds_to_whole_amps_ties_to_even_before_tenths() {
        for (ov, mp, deci) in [(15.0, 30.5, 100), (14.3, 28.4, 120), (7.0, 31.0, 60)] {
            let mut r = rig([1.0, mp, 2.0]);
            r.w.evse.override_a = Some(ov);
            r.w.evse.state = Some("Charging".into());
            assert_eq!(r.step(&mut BreakerGuard), vec![Command::EvseCurrentDeciA(deci)], "{ov} A at {mp} A");
        }
    }

    /// The guard looks at the conductor in either direction (`|abs`): 29.5 A of *export* on
    /// one phase arms it like import would. EV3 at 13 A, cable drawing: 13 − 3.5 = 9.5 → 10
    /// (ties to even). Buzz override at 6 A does not take the Buzz branch.
    #[test]
    fn export_current_arms_the_guard_and_the_ev3_step_follows_the_buzz() {
        let mut r = rig([-29.5, 0.0, 0.0]);
        r.w.evse.override_a = Some(6.0);
        r.w.cable.power_kw = Some(0.25);
        r.w.cable.set_current_a = Some(13.0);
        assert_eq!(r.step(&mut BreakerGuard), vec![Command::CableSetCurrent(10)]);
        assert_eq!(r.m.peak_guard_interventies, 1);
    }

    /// Exactly 28 A does not arm (`> 28`).
    #[test]
    fn at_exactly_28_a_the_guard_stays_idle() {
        let mut r = rig([28.0, 28.0, -28.0]);
        r.w.evse.override_a = Some(16.0);
        assert!(r.step(&mut BreakerGuard).is_empty());
        assert_eq!(r.m.last_triggered("breaker_guard"), None);
    }

    /// LOGIC.md Safety: "only at > 29 A with a car already at its 6 A floor does it
    /// hard-cut". Buzz charging at 6 A: 29.0 A only arms; 29.4 A pauses it, and the message
    /// prints the phase rounded to an integer (29.4 → 29).
    #[test]
    fn a_car_already_at_6_a_is_cut_only_above_29_a() {
        let mut r = rig([29.0, 0.0, 0.0]);
        r.w.evse.override_a = Some(6.0);
        r.w.evse.state = Some("Charging".into());
        assert!(r.step(&mut BreakerGuard).is_empty(), "29 A: armed but nothing to throttle or cut");
        assert_eq!(r.m.peak_guard_interventies, 0);
        assert_eq!(r.m.last_triggered("breaker_guard"), Some(r.now), "the actions ran (empty choose)");

        r.advance(5);
        r.w.p1.phase_a = [Some(29.4), Some(0.0), Some(0.0)];
        assert_eq!(
            r.step(&mut BreakerGuard),
            vec![
                Command::EvseMode(EvseMode::Pause),
                hoofdzekering("Fase ~29 A — Buzz al op 6A, toch afgekapt (3x32A grens).")
            ]
        );
        assert_eq!(r.m.peak_guard_interventies, 1);
    }

    /// Cut order Buzz → EV3 → battery (`breaker_guard` choose): with the Buzz not charging the
    /// EV3 at its floor (6 A, 1.4 kW) is stopped; 30.5 A prints as 30 (ties to even).
    #[test]
    fn the_ev3_is_cut_when_the_buzz_is_not_charging() {
        let mut r = rig([0.0, 30.5, 0.0]);
        r.w.cable.power_kw = Some(1.4);
        r.w.cable.set_current_a = Some(6.0);
        r.m.batterij_net_laden = true;
        assert_eq!(
            r.step(&mut BreakerGuard),
            vec![Command::CableStop, hoofdzekering("Fase ~30 A — EV3 al op 6A, toch afgekapt (3x32A grens).")]
        );
        assert!(r.m.batterij_net_laden, "the battery is only next in line");
    }

    /// Last in the cut order: grid-charging the battery is switched off (and stays off until
    /// someone turns it back on). A cable between 0.2 and 0.3 kW is neither throttled (already
    /// at 6 A) nor cut (≤ 0.3 kW).
    #[test]
    fn battery_grid_charge_is_the_last_thing_cut() {
        let mut r = rig([0.0, 0.0, 31.6]);
        r.w.cable.power_kw = Some(0.25);
        r.w.cable.set_current_a = Some(6.0);
        r.m.batterij_net_laden = true;
        assert_eq!(
            r.step(&mut BreakerGuard),
            vec![hoofdzekering(
                "Fase ~32 A — batterij-net-laden gepauzeerd (3x32A grens). Zet handmatig terug aan als veilig."
            )]
        );
        assert!(!r.m.batterij_net_laden);
        assert_eq!(r.m.peak_guard_interventies, 1);
    }

    /// Triggers: `/5 s` or a change of any phase. Between 5 s boundaries with steady phases
    /// nothing runs; a phase change runs it at once.
    #[test]
    fn the_guard_runs_every_5_s_or_on_a_phase_change() {
        let mut r = Rig::new(at(2026, 7, 1, 12, 0, 6));
        r.w.p1.phase_a = [Some(31.0), None, None];
        r.pw.p1.phase_a = r.w.p1.phase_a;
        r.w.evse.override_a = Some(10.0);
        r.w.evse.state = Some("Charging".into());
        assert!(r.step(&mut BreakerGuard).is_empty(), "12:00:06, no change");
        r.advance(1);
        r.w.p1.phase_a[1] = Some(2.0);
        // 10 − (31 − 26) = 5 → 6 A floor.
        assert_eq!(r.step(&mut BreakerGuard), vec![Command::EvseCurrentDeciA(60)]);
    }

    /// `breaker_guard` Buzz step-down requires `sensor.smartevse_1234_state == Charging`: an
    /// idle Buzz with a 16 A override is skipped and the charging EV3 is stepped down instead
    /// (13 − (30 − 26) = 9 A); with an unavailable Buzz state likewise.
    #[test]
    fn an_idle_buzz_override_does_not_shadow_the_ev3_step() {
        for state in [Some("Connected to EV"), None] {
            let mut r = rig([30.0, 0.0, 0.0]);
            r.w.evse.override_a = Some(16.0);
            r.w.evse.state = state.map(Into::into);
            r.w.cable.power_kw = Some(3.0);
            r.w.cable.set_current_a = Some(13.0);
            assert_eq!(r.step(&mut BreakerGuard), vec![Command::CableSetCurrent(9)], "{state:?}");
        }
    }

    /// `breaker_guard` choose, EV3 step: `ev_charging_cable_vermogen > 0.2`. A cable at
    /// exactly 0.2 kW is not stepped down; at 29.5 A nothing else matches either (Buzz idle,
    /// cable ≤ 0.3 kW, no grid charge), so the run is empty.
    #[test]
    fn a_cable_at_exactly_0_2_kw_is_not_stepped_down() {
        let mut r = rig([29.5, 0.0, 0.0]);
        r.w.cable.power_kw = Some(0.2);
        r.w.cable.set_current_a = Some(13.0);
        assert!(r.step(&mut BreakerGuard).is_empty());
        r.w.cable.power_kw = Some(0.21);
        assert_eq!(r.step(&mut BreakerGuard), vec![Command::CableSetCurrent(10)], "13 − 3.5 = 9.5 → 10");
    }

    /// `breaker_guard` choose, EV3 cut: `maxphase > 29 and cable > 0.3`. At exactly 29 A or
    /// with the cable at exactly 0.3 kW the EV3 at its floor is not cut; the cut bumps
    /// `counter.peak_guard_interventies` by one.
    #[test]
    fn the_ev3_cut_needs_strictly_above_29_a_and_0_3_kw() {
        let mut r = rig([29.0, 0.0, 0.0]);
        r.w.cable.power_kw = Some(1.4);
        r.w.cable.set_current_a = Some(6.0);
        assert!(r.step(&mut BreakerGuard).is_empty(), "29.0 A is not above 29");
        let mut r = rig([30.0, 0.0, 0.0]);
        r.w.cable.power_kw = Some(0.3);
        r.w.cable.set_current_a = Some(6.0);
        assert!(r.step(&mut BreakerGuard).is_empty(), "0.3 kW is not above 0.3");
        r.w.cable.power_kw = Some(0.31);
        r.m.peak_guard_interventies = 4;
        assert_eq!(
            r.step(&mut BreakerGuard),
            vec![Command::CableStop, hoofdzekering("Fase ~30 A — EV3 al op 6A, toch afgekapt (3x32A grens).")]
        );
        assert_eq!(r.m.peak_guard_interventies, 5);
    }

    /// `breaker_guard` choose, battery cut: `maxphase > 29` *and* `batterij_net_laden` on. At
    /// exactly 29 A grid-charging stays on; above 29 A with grid-charging already off there is
    /// nothing to cut and nothing is counted.
    #[test]
    fn the_battery_cut_needs_strictly_above_29_a_and_grid_charging_on() {
        let mut r = rig([0.0, 29.0, 0.0]);
        r.m.batterij_net_laden = true;
        assert!(r.step(&mut BreakerGuard).is_empty());
        assert!(r.m.batterij_net_laden);
        let mut r = rig([0.0, 30.0, 0.0]);
        assert!(r.step(&mut BreakerGuard).is_empty());
        assert_eq!(r.m.peak_guard_interventies, 0);
    }
}
