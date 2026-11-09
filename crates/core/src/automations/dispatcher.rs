//! Port of HA automation `soluna_dispatcher`.
//!
//! The dumb execution layer: every 30 s (and at once on a P1 or phase-current excursion) it
//! reads `sensor.batterij_setpoint` and puts the Solis remote control on it, after folding the
//! residual P1 into the slow bias integral.
//!
//! Choices:
//! - `sp` is this tick's `d.batterij_setpoint_w`, computed at the start of the tick. `new_bias`
//!   re-reads the preamble ([`Brain::read`]) from the current memory, as HA renders the
//!   automation variables at trigger time; an automation earlier in the tick that changed memory
//!   (e.g. `ev_hoger_laadstil`) shows in the bias a tick before it shows in `sp`.
//! - The numeric_state triggers are fed every tick, also while the dispatcher is off, as HA's
//!   triggers run independently of the conditions.

use chrono::Duration;

use super::{Automation, Cx};
use crate::battery::{Brain, next_bias};
use crate::command::Command;
use crate::jinja::{max2, round};
use crate::trigger::Held;
use crate::world::RcMode;

#[derive(Debug, Default)]
pub struct SolunaDispatcher {
    /// `sensor.p1_meter_vermogen above: 5800`.
    p1_high: Held,
    /// `sensor.p1_meter_stroom_fase_n above: 28`.
    phase_high: [Held; 3],
    /// `sensor.p1_meter_stroom_fase_n below: -28`.
    phase_low: [Held; 3],
}

impl SolunaDispatcher {
    /// Feed every numeric_state trigger; true if any crossed this tick.
    fn crossed(&mut self, cx: &Cx) -> bool {
        let now = cx.now;
        let p1 = cx.w.p1.power_w;
        let mut fired = self.p1_high.update(p1.is_some_and(|v| v > 5800.0), now, 0.0);
        for (n, a) in cx.w.p1.phase_a.iter().enumerate() {
            fired |= self.phase_high[n].update(a.is_some_and(|v| v > 28.0), now, 0.0);
            fired |= self.phase_low[n].update(a.is_some_and(|v| v < -28.0), now, 0.0);
        }
        fired
    }
}

impl Automation for SolunaDispatcher {
    fn id(&self) -> &'static str {
        "soluna_dispatcher"
    }

    fn step(&mut self, cx: &mut Cx) {
        let crossed = self.crossed(cx);
        if !(cx.every(30) || crossed) || !cx.s.soluna_absorptie_actief {
            return;
        }
        let sp = cx.d.batterij_setpoint_w;
        let charge_w = round(max2(0.0, -sp), 0) as u32;
        let discharge_w = round(max2(0.0, sp), 0) as u32;
        let new_bias = next_bias(&Brain::read(cx.now, cx.w, cx.s, cx.m, cx.d));

        cx.m.steering_volgende_update = Some(cx.now + Duration::seconds(30));
        cx.send(Command::SolisRcTimeoutMin(10));
        cx.m.batterij_setpoint_bias = new_bias;
        if sp < -30.0 {
            cx.send(Command::SolisAllowGridCharge(true));
            cx.send(Command::SolisRcMode(RcMode::ForceCharge));
            cx.send(Command::SolisRcChargeW(charge_w));
            cx.m.soluna_absorptie_bezig = true;
        } else if sp > 30.0 {
            cx.send(Command::SolisAllowGridCharge(false));
            cx.send(Command::SolisRcMode(RcMode::ForceDischarge));
            cx.send(Command::SolisRcDischargeW(discharge_w));
            cx.m.soluna_absorptie_bezig = true;
        } else {
            cx.send(Command::SolisAllowGridCharge(false));
            cx.send(Command::SolisRcMode(RcMode::ForceCharge));
            cx.send(Command::SolisRcChargeW(0));
            cx.send(Command::SolisRcDischargeW(0));
            cx.m.soluna_absorptie_bezig = false;
        }
        let id = self.id();
        cx.mark(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::rig::Rig;
    use crate::time::{Ts, at};

    fn t0() -> Ts {
        at(2026, 11, 7, 12, 0, 10)
    }

    /// A rig past its start tick, with valid readings, plain self-consumption.
    fn rig() -> Rig<SolunaDispatcher> {
        let mut r = Rig::new(SolunaDispatcher::default());
        r.w.p1.power_w = Some(500.0);
        r.w.p1.phase_a = [Some(5.0), Some(5.0), Some(5.0)];
        r.w.solis.soc = Some(60.0);
        r.w.solis.battery_power_w = Some(0.0);
        assert!(r.tick(t0()).is_empty(), "start tick: no time pattern");
        r
    }

    fn secs(s: i64) -> Ts {
        t0() + Duration::seconds(s)
    }

    /// `time_pattern seconds: /30` fires on :00 and :30, not in between.
    #[test]
    fn runs_every_30_seconds() {
        let mut r = rig();
        assert!(r.tick(secs(10)).is_empty(), "12:00:20");
        assert!(!r.tick(secs(20)).is_empty(), "12:00:30");
        assert!(r.tick(secs(21)).is_empty());
        assert_eq!(r.m.last_triggered("soluna_dispatcher"), Some(secs(20)));
    }

    /// Discharge branch (`sp > 30`): timer, RC timeout 10, bias, then grid-charge off, force
    /// discharge, discharge power, `soluna_absorptie_bezig` on. Bias: plain tracking, P1 500 →
    /// 0 + 0.1·500 = 50.
    #[test]
    fn discharge_branch_commands_in_order() {
        let mut r = rig();
        r.d.batterij_setpoint_w = 1500.0;
        let out = r.tick(secs(20));
        assert_eq!(
            out,
            vec![
                Command::SolisRcTimeoutMin(10),
                Command::SolisAllowGridCharge(false),
                Command::SolisRcMode(RcMode::ForceDischarge),
                Command::SolisRcDischargeW(1500),
            ]
        );
        assert!(r.m.soluna_absorptie_bezig);
        assert_eq!(r.m.batterij_setpoint_bias, 50.0);
        assert_eq!(r.m.steering_volgende_update, Some(secs(50)));
    }

    /// Charge branch (`sp < -30`): grid-charge on, force charge, `charge_w = max(0, −sp)`.
    #[test]
    fn charge_branch_commands_in_order() {
        let mut r = rig();
        r.d.batterij_setpoint_w = -2200.0;
        let out = r.tick(secs(20));
        assert_eq!(
            out,
            vec![
                Command::SolisRcTimeoutMin(10),
                Command::SolisAllowGridCharge(true),
                Command::SolisRcMode(RcMode::ForceCharge),
                Command::SolisRcChargeW(2200),
            ]
        );
        assert!(r.m.soluna_absorptie_bezig);
    }

    /// Hold branch (`-30 <= sp <= 30`): grid-charge off, force charge at 0 and discharge 0,
    /// `soluna_absorptie_bezig` off. Both edges ±30 hold.
    #[test]
    fn hold_branch_between_minus_30_and_30() {
        for sp in [30.0, -30.0, 0.0] {
            let mut r = rig();
            r.m.soluna_absorptie_bezig = true;
            r.d.batterij_setpoint_w = sp;
            let out = r.tick(secs(20));
            assert_eq!(
                out,
                vec![
                    Command::SolisRcTimeoutMin(10),
                    Command::SolisAllowGridCharge(false),
                    Command::SolisRcMode(RcMode::ForceCharge),
                    Command::SolisRcChargeW(0),
                    Command::SolisRcDischargeW(0),
                ],
                "{sp}"
            );
            assert!(!r.m.soluna_absorptie_bezig);
        }
        let mut r = rig();
        r.d.batterij_setpoint_w = 31.0;
        assert_eq!(r.tick(secs(20))[3], Command::SolisRcDischargeW(31));
        let mut r = rig();
        r.d.batterij_setpoint_w = -31.0;
        assert_eq!(r.tick(secs(20))[3], Command::SolisRcChargeW(31));
    }

    /// `condition: input_boolean.soluna_absorptie_actief on`: off → nothing, bias untouched.
    #[test]
    fn does_nothing_while_disabled() {
        let mut r = rig();
        r.s.soluna_absorptie_actief = false;
        r.m.batterij_setpoint_bias = 12.0;
        assert!(r.tick(secs(20)).is_empty());
        assert_eq!(r.m.batterij_setpoint_bias, 12.0);
        assert_eq!(r.m.last_triggered("soluna_dispatcher"), None);
    }

    /// `numeric_state sensor.p1_meter_vermogen above: 5800`: fires on the crossing only.
    #[test]
    fn p1_above_5800_triggers_once_on_the_crossing() {
        let mut r = rig();
        r.w.p1.power_w = Some(5800.0);
        assert!(r.tick(secs(1)).is_empty(), "5800 is not above");
        r.w.p1.power_w = Some(5801.0);
        assert!(!r.tick(secs(2)).is_empty());
        assert!(r.tick(secs(3)).is_empty(), "still above: no new crossing");
    }

    /// Each phase `above: 28` and `below: -28` triggers; unavailable never matches but re-arms.
    #[test]
    fn phase_excursions_trigger_on_every_conductor() {
        for n in 0..3 {
            for v in [28.5, -28.5] {
                let mut r = rig();
                r.w.p1.phase_a[n] = Some(v);
                assert!(!r.tick(secs(1)).is_empty(), "phase {n} {v}");
            }
        }
        let mut r = rig();
        r.w.p1.phase_a[1] = Some(29.0);
        r.tick(secs(1));
        r.w.p1.phase_a[1] = None;
        assert!(r.tick(secs(2)).is_empty(), "unavailable does not match");
        r.w.p1.phase_a[1] = Some(29.0);
        assert!(!r.tick(secs(3)).is_empty(), "back above: a new crossing");
    }

    /// `numeric_state` `above: 28` / `below: -28` are strict: a phase at exactly ±28.0 A is no
    /// crossing; ±28.01 A is.
    #[test]
    fn a_phase_at_exactly_28_a_is_not_a_crossing() {
        for v in [28.0, -28.0] {
            let mut r = rig();
            r.w.p1.phase_a[2] = Some(v);
            assert!(r.tick(secs(1)).is_empty(), "{v}");
            r.w.p1.phase_a[2] = Some(v / 28.0 * 28.01);
            assert!(!r.tick(secs(2)).is_empty(), "{v} → beyond");
        }
    }

    /// A crossing while disabled is consumed: re-enabling does not replay it.
    #[test]
    fn crossing_while_disabled_is_not_replayed() {
        let mut r = rig();
        r.s.soluna_absorptie_actief = false;
        r.w.p1.power_w = Some(6000.0);
        assert!(r.tick(secs(1)).is_empty());
        r.s.soluna_absorptie_actief = true;
        assert!(r.tick(secs(2)).is_empty());
    }

    /// `new_bias` freezes outside plain tracking: SoC at the 15 % floor → `bias|round(1)`.
    #[test]
    fn bias_is_frozen_at_the_floor() {
        let mut r = rig();
        r.w.solis.soc = Some(15.0);
        r.m.batterij_setpoint_bias = 33.33;
        r.tick(secs(20));
        assert_eq!(r.m.batterij_setpoint_bias, 33.3);
    }

    /// `new_bias` reads the median P1 and the current memory: median 300 → 0.1·300 = 30 on top
    /// of the stored −10.
    #[test]
    fn bias_reads_the_median_and_the_stored_bias() {
        let mut r = rig();
        r.d.p1_mediaan_w = Some(300.0);
        r.m.batterij_setpoint_bias = -10.0;
        r.tick(secs(20));
        assert_eq!(r.m.batterij_setpoint_bias, 20.0);
    }
}
