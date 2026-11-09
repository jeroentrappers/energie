//! Port of HA automation `ev_laden_buzz_handshake_herstel` ("Buzz: handshake-herstel
//! (vastgelopen 'Connected to EV')"). The SmartEVSE offers current (`Normal`, override ≥ 6) but
//! the car sits in `Connected to EV` for 5 minutes. The first time per session
//! (`counter.buzz_laadweigering < 1`) cycle the mode once: `Pause`, 30 s, `Normal`. If it is stuck
//! again, the car has latched a charge fault that only a replug clears (LOGIC.md
//! "Charge-session protection"): `Pause` and tell the user.
//!
//! Trigger: `sensor.smartevse_1234_state` to `Connected to EV` for 5 min ([`Held`], so a state
//! already present at start does not fire). Conditions: mode `Normal`, override above 5, the car
//! plugged (latched plug or pilot B/C), and the Buzz below its charge target.
//!
//! `mode: single`, `max_exceeded: silent`: while the 30 s delay runs, a new trigger is dropped.
//! The delay is the `normal_due` state below.
use super::{Automation, Cx};
use crate::cascade;
use crate::command::Command;
use crate::time::Ts;
use crate::trigger::Held;
use crate::world::EvseMode;

/// The `delay: seconds: 30` between `Pause` and `Normal`.
pub const CYCLE_DELAY_S: i64 = 30;
/// The `for: minutes: 5` on `Connected to EV`.
pub const STUCK_S: f64 = 300.0;

#[derive(Debug, Default)]
pub struct BuzzHandshake {
    stuck: Held,
    /// When the pending `Normal` of a mode cycle is due; `Some` while the actions run.
    normal_due: Option<Ts>,
}

impl Automation for BuzzHandshake {
    fn id(&self) -> &'static str {
        "ev_laden_buzz_handshake_herstel"
    }

    fn step(&mut self, cx: &mut Cx) {
        if let Some(due) = self.normal_due
            && cx.now >= due
        {
            self.normal_due = None;
            cx.send(Command::EvseMode(EvseMode::Normal));
            cx.send(Command::notify(
                "🔌 Buzz laden herstart",
                "SmartEVSE stond vast op 'Connected to EV' — mode eenmalig gecycled (Pause -> 30 s -> Normal). \
                 Lukt het niet, dan volgt een melding om opnieuw in te pluggen.",
            ));
        }

        let fired = self.stuck.update(cx.w.evse.state.as_deref() == Some("Connected to EV"), cx.now, STUCK_S);
        if !fired || self.normal_due.is_some() {
            return;
        }
        let w = cx.w;
        let conditions = w.evse.mode == Some(EvseMode::Normal)
            && w.evse.override_a.is_some_and(|a| a > 5.0)
            && cascade::buzz_wants(w, cx.m)
            && w.buzz.soc.unwrap_or(0.0) < w.buzz.target_soc.unwrap_or(100.0);
        if !conditions {
            return;
        }
        cx.mark(self.id());
        if cx.m.buzz_laadweigering < 1 {
            cx.m.buzz_laadweigering += 1;
            cx.send(Command::EvseMode(EvseMode::Pause));
            self.normal_due = Some(cx.now + chrono::Duration::seconds(CYCLE_DELAY_S));
        } else {
            cx.send(Command::EvseMode(EvseMode::Pause));
            cx.send(Command::notify(
                "🛑 Buzz weigert te laden — opnieuw inpluggen",
                "De Buzz blijft op 'Connected to EV' staan nadat de lader al opnieuw geprobeerd heeft. \
                 De auto heeft waarschijnlijk een laadfout gelatcht ('Laadstation controleren', rode ring). \
                 Dat is alleen te wissen door de kabel los te trekken en opnieuw in te pluggen. \
                 De lader staat op Pause en probeert het niet verder.",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;

    /// Plugged, Normal, 10 A offered, the car at 60 % of an 80 % target, charging.
    fn rig() -> Rig {
        let mut r = Rig::new();
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.state = Some("Charging".into());
        r.w.evse.state_id = Some("B".into());
        r.w.evse.mode = Some(EvseMode::Normal);
        r.w.evse.override_a = Some(10.0);
        r.w.buzz.soc = Some(60.0);
        r.w.buzz.target_soc = Some(80.0);
        r
    }

    fn notified(out: &[Command]) -> Vec<&str> {
        out.iter()
            .filter_map(|c| match c {
                Command::Notify { title, .. } => Some(title.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Get stuck in `Connected to EV` and wait the 5 minutes; returns the commands at 300 s.
    fn get_stuck(r: &mut Rig, a: &mut BuzzHandshake) -> Vec<Command> {
        r.run(a);
        r.wait(5);
        r.w.evse.state = Some("Connected to EV".into());
        assert!(r.run(a).is_empty());
        r.wait(299);
        assert!(r.run(a).is_empty(), "299 s");
        r.wait(1);
        r.run(a)
    }

    /// First time: count it, Pause, then 30 s later Normal and a notification.
    #[test]
    fn first_time_cycles_the_mode_once() {
        let mut r = rig();
        let mut a = BuzzHandshake::default();
        assert_eq!(get_stuck(&mut r, &mut a), vec![Command::EvseMode(EvseMode::Pause)]);
        assert_eq!(r.m.buzz_laadweigering, 1);
        assert_eq!(r.m.last_triggered("ev_laden_buzz_handshake_herstel"), Some(r.now));
        r.wait(29);
        assert!(r.run(&mut a).is_empty(), "the delay runs");
        r.wait(1);
        let out = r.run(&mut a);
        assert_eq!(out[0], Command::EvseMode(EvseMode::Normal));
        assert_eq!(notified(&out), vec!["🔌 Buzz laden herstart"]);
        r.wait(30);
        assert!(r.run(&mut a).is_empty(), "done");
    }

    /// Stuck again after the cycle (counter 1): Pause for good and ask for a replug.
    #[test]
    fn second_time_pauses_and_asks_for_a_replug() {
        let mut r = rig();
        r.m.buzz_laadweigering = 1;
        let mut a = BuzzHandshake::default();
        let out = get_stuck(&mut r, &mut a);
        assert_eq!(out[0], Command::EvseMode(EvseMode::Pause));
        assert_eq!(notified(&out), vec!["🛑 Buzz weigert te laden — opnieuw inpluggen"]);
        assert_eq!(r.m.buzz_laadweigering, 1, "not counted further");
        assert!(matches!(&out[1], Command::Notify { message, .. }
            if message.starts_with("De Buzz blijft op 'Connected to EV' staan")
                && message.ends_with("De lader staat op Pause en probeert het niet verder.")));
    }

    /// `mode: single`: a new 5-minute stuck period that completes during the 30 s delay is
    /// dropped (and, being a transition trigger, does not fire again later in that run).
    #[test]
    fn a_trigger_during_the_delay_is_dropped() {
        let mut r = rig();
        let mut a = BuzzHandshake::default();
        get_stuck(&mut r, &mut a);
        let started = r.now;
        // Force the stuck tracker to fire again while the delay runs.
        a.stuck = Held::primed();
        r.wait(1);
        a.stuck.update(true, r.now - chrono::Duration::seconds(400), STUCK_S);
        assert!(r.run(&mut a).is_empty());
        assert_eq!(r.m.last_triggered("ev_laden_buzz_handshake_herstel"), Some(started));
        assert_eq!(r.m.buzz_laadweigering, 1);
    }

    /// Each condition on its own blocks the run.
    #[test]
    fn conditions() {
        let cases: [fn(&mut Rig); 5] = [
            |r| r.w.evse.mode = Some(EvseMode::Pause),
            |r| r.w.evse.override_a = Some(5.0),
            |r| r.w.evse.override_a = None,
            |r| {
                r.w.evse.plug = Some("Disconnected".into());
                r.w.evse.state_id = Some("A".into());
            },
            |r| r.w.buzz.soc = Some(80.0),
        ];
        for (i, f) in cases.iter().enumerate() {
            let mut r = rig();
            f(&mut r);
            let mut a = BuzzHandshake::default();
            assert!(get_stuck(&mut r, &mut a).is_empty(), "case {i}");
            assert_eq!(r.m.last_triggered("ev_laden_buzz_handshake_herstel"), None);
        }
        // Unknown SoC 0 < unknown target 100: passes.
        let mut r = rig();
        r.w.buzz = Default::default();
        let mut a = BuzzHandshake::default();
        assert!(!get_stuck(&mut r, &mut a).is_empty());
    }

    /// Already stuck when we start: does not fire (a transition trigger).
    #[test]
    fn already_stuck_at_start_does_not_fire() {
        let mut r = rig();
        r.w.evse.state = Some("Connected to EV".into());
        let mut a = BuzzHandshake::default();
        r.run(&mut a);
        r.wait(600);
        assert!(r.run(&mut a).is_empty());
    }
}
