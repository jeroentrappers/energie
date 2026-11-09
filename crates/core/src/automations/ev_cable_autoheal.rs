//! Port of HA automation `ev_cable_autoheal` ("EV cable: auto-heal tuya_local when
//! unavailable"): if the Tuya cable's status stays `unavailable` for 10 minutes, re-open the
//! session (HA: `homeassistant.reload_config_entry` on the entity, here
//! [`Command::CableReconnect`]) and leave a persistent note (`notification_id:
//! ev_cable_autoheal`, so a retry replaces the note instead of adding one).
//!
//! Triggers: `sensor.ev_charging_cable_status` to `unavailable` for 10 min ([`Held`]), and
//! `time_pattern minutes: /30`, which retries while it stays offline (HA started with it
//! offline, or a reload did not bring it back). Condition: the status is `unavailable` and its
//! `last_changed` is at least 600 s ago. HA sets `last_changed` when the state changes and when
//! it starts (the entity is created afresh); this automation keeps that instant itself.
//!
//! `World.cable.status == None` is HA's `unavailable`/`unknown`; an explicit `unavailable`
//! string counts too, `unknown` does not (`to: unavailable`, `is_state(…, 'unavailable')`).
use super::{Automation, Cx};
use crate::command::Command;
use crate::time::{self, Ts};
use crate::trigger::Held;

/// `for: 00:10:00` and the condition's `>= 600`.
pub const HOLD_S: f64 = 600.0;
/// `minutes: /30`.
pub const RETRY_S: i64 = 1800;
/// The persistent note's `notification_id`.
pub const NOTIFICATION_ID: &str = "ev_cable_autoheal";

#[derive(Debug, Default)]
pub struct EvCableAutoheal {
    unavailable: Held,
    /// `states.sensor.ev_charging_cable_status.last_changed`.
    last_changed: Option<Ts>,
}

impl Automation for EvCableAutoheal {
    fn id(&self) -> &'static str {
        "ev_cable_autoheal"
    }

    fn step(&mut self, cx: &mut Cx) {
        if cx.started || cx.pw.cable.status != cx.w.cable.status {
            self.last_changed = Some(cx.now);
        }
        let down = matches!(cx.w.cable.status.as_deref(), None | Some("unavailable"));
        let held = self.unavailable.update(down, cx.now, HOLD_S);
        if !(held || cx.every(RETRY_S)) {
            return;
        }
        let since = self.last_changed.map_or(0.0, |t| time::secs(t, cx.now));
        if !(down && since >= HOLD_S) {
            return;
        }
        cx.mark(self.id());
        cx.send(Command::CableReconnect);
        cx.send(Command::persistent_id(
            NOTIFICATION_ID,
            "EV cable auto-heal",
            format!(
                "Reloaded tuya_local for EV charging cable at {} (was unavailable >10 min).",
                cx.now.format("%H:%M")
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;

    fn healed(hhmm: &str) -> Vec<Command> {
        vec![
            Command::CableReconnect,
            Command::persistent_id(
                "ev_cable_autoheal",
                "EV cable auto-heal",
                format!("Reloaded tuya_local for EV charging cable at {hhmm} (was unavailable >10 min)."),
            ),
        ]
    }

    /// `ev_cable_autoheal`: 10 min unavailable reloads the cable (12:00:15 → 12:10:15); while it
    /// stays down the `/30` pattern retries at 12:30:00, not at the 10 min mark after the first
    /// heal.
    #[test]
    fn reconnects_after_ten_minutes_and_retries_every_half_hour() {
        let mut r = Rig::new();
        r.w.cable.status = Some("charging".into());
        let mut a = EvCableAutoheal::default();
        r.run(&mut a);
        r.wait(5); // 12:00:15
        r.w.cable.status = None;
        assert!(r.run(&mut a).is_empty());
        r.wait(599);
        assert!(r.run(&mut a).is_empty(), "599 s");
        r.wait(1); // 12:10:15
        assert_eq!(r.run(&mut a), healed("12:10"));
        assert_eq!(r.m.last_triggered("ev_cable_autoheal"), Some(r.now));
        r.wait(600); // 12:20:15
        assert!(r.run(&mut a).is_empty(), "no new transition, no pattern");
        r.wait(584); // 12:29:59
        assert!(r.run(&mut a).is_empty());
        r.wait(1); // 12:30:00
        assert_eq!(r.run(&mut a), healed("12:30"));
    }

    /// The explicit `unavailable` string counts; `unknown` and a blip back do not.
    #[test]
    fn only_unavailable_counts_and_must_hold() {
        let mut r = Rig::new();
        r.w.cable.status = Some("available".into());
        let mut a = EvCableAutoheal::default();
        r.run(&mut a);
        r.wait(5);
        r.w.cable.status = Some("unknown".into());
        r.run(&mut a);
        r.wait(700);
        assert!(r.run(&mut a).is_empty(), "unknown");
        r.w.cable.status = Some("unavailable".into());
        r.run(&mut a);
        r.wait(300);
        r.w.cable.status = Some("available".into());
        r.run(&mut a);
        r.w.cable.status = Some("unavailable".into());
        r.wait(1);
        r.run(&mut a);
        r.wait(599);
        assert!(r.run(&mut a).is_empty(), "the blip back restarted the 10 min");
        r.wait(1);
        assert!(!r.run(&mut a).is_empty());
    }

    /// Unavailable already at start (12:00:10): `last_changed` is the start, so the first
    /// `/30` pattern (12:30:00, 1790 s later) heals it.
    #[test]
    fn unavailable_at_start_is_healed_by_the_half_hour_pattern() {
        let mut r = Rig::new();
        let mut a = EvCableAutoheal::default();
        r.run(&mut a);
        r.wait(1789); // 12:29:59
        assert!(r.run(&mut a).is_empty());
        r.wait(1);
        assert_eq!(r.run(&mut a), healed("12:30"));
    }

    /// The pattern alone needs the condition: the cable up, or down for less than 600 s
    /// (12:25:00 → 12:30:00 is 300 s), does nothing; the 10 min hold heals it at 12:35:00.
    #[test]
    fn the_pattern_needs_600_s_of_unavailable() {
        let mut r = Rig::new();
        r.w.cable.status = Some("charging".into());
        let mut a = EvCableAutoheal::default();
        r.run(&mut a);
        r.wait(1490); // 12:25:00
        r.w.cable.status = Some("unavailable".into());
        assert!(r.run(&mut a).is_empty());
        r.wait(300); // 12:30:00
        assert!(r.run(&mut a).is_empty(), "300 s");
        r.wait(300); // 12:35:00
        assert_eq!(r.run(&mut a), healed("12:35"));
        let mut r = Rig::new();
        let mut a = EvCableAutoheal::default();
        r.w.cable.status = Some("charging".into());
        r.run(&mut a);
        r.wait(1790);
        assert!(r.run(&mut a).is_empty(), "12:30:00 with the cable up");
    }
}
