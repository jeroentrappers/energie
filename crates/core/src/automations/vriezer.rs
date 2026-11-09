//! Port of HA automation `vriezer_bewaking`.
//!
//! Alarms when the freezer draws nothing for 4 h (compressor dead or switched off), when its
//! smart plug is switched off, or when the plug has been unreachable for 15 min.
//!
//! `sensor.vriezer_vermogen` and `switch.vriezer` do not exist in HA (last run 2026-06-30), and
//! the automation was switched off there on 2026-11-07. It is not in the active registry
//! ([`super::all`]); the port is kept for when the freezer plug returns. With `World::vriezer`
//! all `None` it would stay silent anyway: an entity that never changes never triggers.

use super::{Automation, Cx};
use crate::command::Command;
use crate::jinja::py_float;
use crate::trigger::Held;

#[derive(Debug, Default)]
pub struct VriezerBewaking {
    /// `geen_verbruik`: power below 5 W for 4 h.
    idle: Held,
    /// `plug_offline`: power `unavailable` for 15 min.
    offline: Held,
}

impl Automation for VriezerBewaking {
    fn id(&self) -> &'static str {
        "vriezer_bewaking"
    }

    fn step(&mut self, cx: &mut Cx) {
        let power = cx.w.vriezer.power_w;
        // `numeric_state below: 5`: an unavailable source does not match.
        let idle = self.idle.update(power.is_some_and(|p| p < 5.0), cx.now, 4.0 * 3600.0);
        // `state to: 'off'`, any from-state.
        let switched_off = cx.pw.vriezer.switch_on != Some(false) && cx.w.vriezer.switch_on == Some(false);
        // `state to: unavailable for 15 min`. `None` stands for both `unavailable` and
        // `unknown`; a real plug that drops off the network reports `unavailable`.
        let offline = self.offline.update(power.is_none(), cx.now, 900.0);
        // Simultaneous triggers: `mode: single` runs the first, the others are dropped.
        let trigger_id = if idle {
            "geen_verbruik"
        } else if switched_off {
            "stekker_uit"
        } else if offline {
            "plug_offline"
        } else {
            return;
        };
        cx.mark(self.id());
        // `states('sensor.vriezer_vermogen')`: the numeric state as HA stores it.
        let vermogen = power.map_or_else(|| "unavailable".to_string(), py_float);
        cx.send(Command::notify_high(
            "🧊 VRIEZER ALARM",
            format!("Vriezer-probleem: {trigger_id} — controleer de diepvries! (vermogen: {vermogen} W)"),
        ));
        cx.send(Command::persistent("Vriezer alarm", format!("Trigger: {trigger_id} om {}", cx.now.format("%H:%M"))));
    }
}

#[cfg(test)]
mod tests {
    use super::super::dagbalans::rig::Rig;
    use super::*;
    use crate::time::at;

    fn alarm(id: &str, vermogen: &str, hhmm: &str) -> Vec<Command> {
        vec![
            Command::notify_high(
                "🧊 VRIEZER ALARM",
                format!("Vriezer-probleem: {id} — controleer de diepvries! (vermogen: {vermogen} W)"),
            ),
            Command::persistent("Vriezer alarm", format!("Trigger: {id} om {hhmm}")),
        ]
    }

    /// `vriezer_bewaking` `geen_verbruik`: below 5 W for 4 h, high priority, with the power
    /// state and the time in the texts.
    #[test]
    fn a_freezer_idle_for_4_h_raises_the_alarm() {
        let mut r = Rig::new(at(2026, 7, 1, 2, 0, 0));
        let mut a = VriezerBewaking::default();
        r.w.vriezer.power_w = Some(80.0);
        r.w.vriezer.switch_on = Some(true);
        r.pw = r.w.clone();
        assert!(r.step(&mut a).is_empty());
        r.w.vriezer.power_w = Some(4.9);
        assert!(r.tick(&mut a, 1).is_empty());
        assert!(r.tick(&mut a, 4 * 3600 - 1).is_empty());
        assert_eq!(r.tick(&mut a, 1), alarm("geen_verbruik", "4.9", "06:00"));
        assert_eq!(r.m.last_triggered("vriezer_bewaking"), Some(r.now));
        assert!(r.tick(&mut a, 60).is_empty(), "once per idle run");
    }

    /// `geen_verbruik` is `numeric_state below: 5`, strict: a freezer idling at exactly 5.0 W
    /// for 4 h raises nothing.
    #[test]
    fn exactly_5_w_is_not_idle() {
        let mut r = Rig::new(at(2026, 7, 1, 2, 0, 0));
        let mut a = VriezerBewaking::default();
        r.w.vriezer.power_w = Some(80.0);
        r.w.vriezer.switch_on = Some(true);
        r.pw = r.w.clone();
        r.step(&mut a);
        r.w.vriezer.power_w = Some(5.0);
        assert!(r.tick(&mut a, 1).is_empty());
        assert!(r.tick(&mut a, 4 * 3600).is_empty());
        assert_eq!(r.m.last_triggered("vriezer_bewaking"), None);
    }

    /// `stekker_uit`: the plug switched off, at once; staying off does not repeat it.
    #[test]
    fn switching_the_plug_off_raises_the_alarm_at_once() {
        let mut r = Rig::new(at(2026, 7, 1, 9, 15, 0));
        let mut a = VriezerBewaking::default();
        r.w.vriezer.power_w = Some(0.0);
        r.w.vriezer.switch_on = Some(true);
        r.pw = r.w.clone();
        r.step(&mut a);
        r.advance(1);
        r.w.vriezer.switch_on = Some(false);
        assert_eq!(r.step(&mut a), alarm("stekker_uit", "0.0", "09:15"));
        assert!(r.tick(&mut a, 1).is_empty());
    }

    /// `plug_offline`: power unavailable for 15 min; the message prints `unavailable`.
    #[test]
    fn a_plug_offline_for_15_min_raises_the_alarm() {
        let mut r = Rig::new(at(2026, 7, 1, 12, 0, 0));
        let mut a = VriezerBewaking::default();
        r.w.vriezer.power_w = Some(60.0);
        r.step(&mut a);
        r.w.vriezer.power_w = None;
        assert!(r.tick(&mut a, 1).is_empty());
        assert!(r.tick(&mut a, 899).is_empty());
        assert_eq!(r.tick(&mut a, 1), alarm("plug_offline", "unavailable", "12:15"));
    }

    /// With the entities absent nothing ever transitions and the automation stays silent, even
    /// for days of "unavailable".
    #[test]
    fn absent_entities_never_trigger() {
        let mut r = Rig::new(at(2026, 11, 7, 12, 0, 0));
        let mut a = VriezerBewaking::default();
        for _ in 0..48 {
            assert!(r.tick(&mut a, 3600).is_empty());
        }
    }
}
