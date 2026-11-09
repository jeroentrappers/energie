//! Port of HA automation `smartevse_pilot_arm` ("SmartEVSE: pilot activeren (auto ingeplugd,
//! pilot op A)"). The control pilot only picks a car up if the mode is `Normal` at the moment of
//! plugging in. Plugged in while paused, the pilot stays at state `A` until a replug. This detects
//! exactly that (plug `Connected`, latched, while the pilot is `A`) and sets `Normal` to force the
//! detection; once the pilot reaches B/C the condition lapses and the modulator takes over.
//!
//! Triggers: the template "plugged and pilot A" true for 45 s ([`Held`]), and
//! `time_pattern minutes: /2`. Conditions: the same template; at least 300 s since its own last
//! run; and more than 1500 W of quarter-hour allowance above P1 (`kwartier_toegestaan_vermogen
//! |float(0) − p1|float(0) > 1500`).
use super::{Automation, Cx};
use crate::cascade;
use crate::command::Command;
use crate::memory::Memory;
use crate::trigger::Held;
use crate::world::{EvseMode, World};

/// The template trigger's `for: 45 s`.
pub const HOLD_S: f64 = 45.0;
/// `(now() - this.attributes.last_triggered).total_seconds() > 300`.
pub const REPEAT_S: f64 = 300.0;

#[derive(Debug, Default)]
pub struct SmartevsePilotArm {
    stuck_on_a: Held,
}

/// `known == 'Connected' and stateid == 'A'`.
pub fn plugged_on_a(w: &World, m: &Memory) -> bool {
    cascade::buzz_plug_known(w, m) == "Connected" && w.evse.state_id.as_deref() == Some("A")
}

/// `states('select.smartevse_1234_mode')` as HA renders it.
pub fn mode_label(mode: Option<EvseMode>) -> &'static str {
    match mode {
        None => "unavailable",
        Some(EvseMode::Off) => "Off",
        Some(EvseMode::Normal) => "Normal",
        Some(EvseMode::Smart) => "Smart",
        Some(EvseMode::Solar) => "Solar",
        Some(EvseMode::Pause) => "Pause",
    }
}

impl Automation for SmartevsePilotArm {
    fn id(&self) -> &'static str {
        "smartevse_pilot_arm"
    }

    fn step(&mut self, cx: &mut Cx) {
        let on_a = plugged_on_a(cx.w, cx.m);
        let held = self.stuck_on_a.update(on_a, cx.now, HOLD_S);
        if !(held || cx.every(120)) {
            return;
        }
        let room = cx.d.kwartier_toegestaan_vermogen_w.unwrap_or(0.0) - cx.w.p1.power_w.unwrap_or(0.0);
        if !on_a || cx.since_triggered(self.id()).is_some_and(|s| s <= REPEAT_S) || room <= 1500.0 {
            return;
        }
        cx.mark(self.id());
        cx.send(Command::EvseMode(EvseMode::Normal));
        cx.send(Command::notify(
            "🔌 SmartEVSE pilot geactiveerd",
            format!(
                "Auto stond ingeplugd terwijl de pilot op A bleef hangen (mode was {}). \
                 Mode op Normal gezet om de detectie te forceren.",
                mode_label(cx.w.evse.mode)
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;

    /// Plugged while paused, pilot stuck at A, 3000 W of room (allowance 3800, P1 800).
    fn rig() -> Rig {
        let mut r = Rig::new();
        r.w.evse.plug = Some("Disconnected".into());
        r.w.evse.state_id = Some("A".into());
        r.w.evse.mode = Some(EvseMode::Pause);
        r.d.kwartier_toegestaan_vermogen_w = Some(3800.0);
        r.w.p1.power_w = Some(800.0);
        r
    }

    fn plug_in(r: &mut Rig, a: &mut SmartevsePilotArm) {
        r.run(a);
        r.wait(5); // 12:00:15
        r.w.evse.plug = Some("Connected".into());
        assert!(r.run(a).is_empty());
    }

    #[test]
    fn arms_the_pilot_after_45_s_on_a() {
        let mut r = rig();
        let mut a = SmartevsePilotArm::default();
        plug_in(&mut r, &mut a);
        r.wait(44);
        assert!(r.run(&mut a).is_empty(), "44 s");
        r.wait(1);
        let out = r.run(&mut a);
        assert_eq!(out[0], Command::EvseMode(EvseMode::Normal));
        assert_eq!(
            out[1],
            Command::notify(
                "🔌 SmartEVSE pilot geactiveerd",
                "Auto stond ingeplugd terwijl de pilot op A bleef hangen (mode was Pause). \
                 Mode op Normal gezet om de detectie te forceren."
            )
        );
    }

    /// The /2 min pattern retries, but not within 300 s of the last run.
    #[test]
    fn the_two_minute_pattern_retries_after_300_s() {
        let mut r = rig();
        let mut a = SmartevsePilotArm::default();
        plug_in(&mut r, &mut a);
        r.wait(45); // 12:01:00 fires
        assert!(!r.run(&mut a).is_empty());
        r.wait(60); // 12:02:00, 60 s later
        assert!(r.run(&mut a).is_empty());
        r.wait(240); // 12:06:00, 300 s: not > 300
        assert!(r.run(&mut a).is_empty());
        r.wait(120); // 12:08:00
        assert!(!r.run(&mut a).is_empty());
    }

    /// Already stuck at start: the pattern (not the template trigger) picks it up.
    #[test]
    fn the_pattern_catches_a_state_present_at_start() {
        let mut r = rig();
        r.w.evse.plug = Some("Connected".into());
        let mut a = SmartevsePilotArm::default();
        r.run(&mut a);
        r.wait(100); // 12:01:50
        assert!(r.run(&mut a).is_empty());
        r.wait(10); // 12:02:00
        assert!(!r.run(&mut a).is_empty());
    }

    /// No room (≤ 1500 W), or the pilot not on A: nothing. A plug blip reads the latch.
    #[test]
    fn conditions() {
        let mut r = rig();
        r.w.p1.power_w = Some(2300.0); // 1500 exactly
        let mut a = SmartevsePilotArm::default();
        plug_in(&mut r, &mut a);
        r.wait(45);
        assert!(r.run(&mut a).is_empty());
        let mut r = rig();
        r.d.kwartier_toegestaan_vermogen_w = None; // |float(0)
        let mut a = SmartevsePilotArm::default();
        plug_in(&mut r, &mut a);
        r.wait(45);
        assert!(r.run(&mut a).is_empty());
        // The pattern fires, but the pilot is at B.
        let mut r = rig();
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.state_id = Some("B".into());
        let mut a = SmartevsePilotArm::default();
        r.run(&mut a);
        r.wait(110);
        assert!(r.run(&mut a).is_empty());
        // Plug unavailable, latch Connected: still "plugged".
        let mut w = World::default();
        let mut m = Memory::default();
        w.evse.state_id = Some("A".into());
        m.smartevse_plug_laatst_bekend = "Connected".into();
        assert!(plugged_on_a(&w, &m));
    }

    #[test]
    fn mode_labels() {
        let all = [None, Some(EvseMode::Off), Some(EvseMode::Normal), Some(EvseMode::Smart), Some(EvseMode::Solar)];
        let labels: Vec<_> = all.into_iter().map(mode_label).collect();
        assert_eq!(labels, ["unavailable", "Off", "Normal", "Smart", "Solar"]);
    }
}
