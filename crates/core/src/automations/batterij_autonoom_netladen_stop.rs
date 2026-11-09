//! Port of HA automation `batterij_autonoom_netladen_stop`.
//!
//! Cuts grid → battery when the inverter charges hard from the grid on its own (battery
//! < −1200 W, P1 > 2500 W) while the setpoint does not ask for it (> −200 W). Triggers: that
//! template held 20 s, and a `/1 min` re-check; conditions: the grid-charge switch is on and the
//! template is still true.

use super::{Automation, Cx};
use crate::battery::{fmt_float_state, fmt_num_state};
use crate::command::Command;
use crate::trigger::Held;

#[derive(Debug, Default)]
pub struct BatterijAutonoomNetladenStop {
    /// The template trigger with `for: 20 s`.
    held: Held,
}

/// `batt|float(0) < -1200 and p1|float(0) > 2500 and setpoint|float(0) > -200`.
fn autonomous_grid_charge(cx: &Cx) -> bool {
    cx.w.solis.battery_power_w.unwrap_or(0.0) < -1200.0
        && cx.w.p1.power_w.unwrap_or(0.0) > 2500.0
        && cx.d.batterij_setpoint_w > -200.0
}

impl Automation for BatterijAutonoomNetladenStop {
    fn id(&self) -> &'static str {
        "batterij_autonoom_netladen_stop"
    }

    fn step(&mut self, cx: &mut Cx) {
        let cond = autonomous_grid_charge(cx);
        let held = self.held.update(cond, cx.now, 20.0);
        if !(held || cx.every(60)) || cx.w.solis.allow_grid_charge != Some(true) || !cond {
            return;
        }
        cx.send(Command::SolisAllowGridCharge(false));
        // The dispatcher's charge branch turns the switch back on once it wants to charge itself.
        let message = format!(
            "Grid->batterij afgesloten: batterij {} W, P1 {} W, setpoint {} W, SoC {}%. De dispatcher zet het weer aan zodra hij zelf wil laden.",
            fmt_num_state(cx.w.solis.battery_power_w),
            fmt_float_state(cx.w.p1.power_w),
            fmt_num_state(Some(cx.d.batterij_setpoint_w)),
            fmt_num_state(cx.w.solis.soc),
        );
        cx.send(Command::notify("🔋 Omvormer laadde zelf van het net", message));
        let id = self.id();
        cx.mark(id);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::battery::rig::Rig;
    use crate::time::{Ts, at};

    fn t0() -> Ts {
        at(2026, 9, 1, 6, 0, 5)
    }

    fn secs(s: i64) -> Ts {
        t0() + Duration::seconds(s)
    }

    /// Started with the inverter behaving; grid-charge switch on.
    fn rig() -> Rig<BatterijAutonoomNetladenStop> {
        let mut r = Rig::new(BatterijAutonoomNetladenStop::default());
        r.w.solis.allow_grid_charge = Some(true);
        r.w.solis.battery_power_w = Some(0.0);
        r.w.solis.soc = Some(9.0);
        r.w.p1.power_w = Some(1500.0);
        r.tick(t0());
        r
    }

    /// The 2026-09-01 incident in numbers.
    fn misbehave(r: &mut Rig<BatterijAutonoomNetladenStop>) {
        r.w.solis.battery_power_w = Some(-5277.0);
        r.w.p1.power_w = Some(7046.0);
        r.d.batterij_setpoint_w = -220.0 + 100.0;
    }

    /// Template trigger `for: 20 s`: switch off and notify with the raw states, once.
    #[test]
    fn cuts_grid_charge_after_20_s() {
        let mut r = rig();
        misbehave(&mut r);
        assert!(r.tick(secs(1)).is_empty());
        assert!(r.tick(secs(20)).is_empty(), "19 s");
        let out = r.tick(secs(21));
        assert_eq!(
            out,
            vec![
                Command::SolisAllowGridCharge(false),
                Command::notify(
                    "🔋 Omvormer laadde zelf van het net",
                    "Grid->batterij afgesloten: batterij -5277 W, P1 7046.0 W, setpoint -120 W, SoC 9%. De dispatcher zet het weer aan zodra hij zelf wil laden."
                ),
            ]
        );
        assert!(r.tick(secs(22)).is_empty(), "held run already fired");
    }

    /// `time_pattern minutes: /1` re-checks while it persists (06:01:00).
    #[test]
    fn rechecks_every_minute() {
        let mut r = rig();
        misbehave(&mut r);
        r.tick(secs(1));
        r.tick(secs(21));
        assert_eq!(r.tick(secs(55)).len(), 2, "06:01:00");
    }

    /// Conditions: the switch must be on and the template true at run time; each clause of the
    /// template matters (−1200, 2500, −200 are strict).
    #[test]
    fn conditions_gate_the_minute_recheck() {
        let mut r = rig();
        misbehave(&mut r);
        r.w.solis.allow_grid_charge = Some(false);
        assert!(r.tick(secs(55)).is_empty(), "switch already off");
        r.w.solis.allow_grid_charge = None;
        assert!(r.tick(secs(115)).is_empty(), "switch unavailable");

        type Poke = fn(&mut Rig<BatterijAutonoomNetladenStop>);
        let pokes: [Poke; 4] = [
            |r| r.w.solis.battery_power_w = Some(-1200.0),
            |r| r.w.p1.power_w = Some(2500.0),
            |r| r.d.batterij_setpoint_w = -200.0,
            |r| r.w.solis.battery_power_w = None,
        ];
        for poke in pokes {
            let mut r = rig();
            misbehave(&mut r);
            poke(&mut r);
            assert!(r.tick(secs(55)).is_empty());
        }
        let mut r = rig();
        misbehave(&mut r);
        assert_eq!(r.tick(secs(55)).len(), 2, "all clauses true");
    }
}
