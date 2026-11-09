//! Port of HA automation `ev_laden_buzz_reset_op_uitplug` ("Buzz: override terug naar 6A bij
//! uitpluggen"): when the car disconnects (the plug reaches `Disconnected`), park the SmartEVSE
//! override at 6 A so the next session starts low and ramps up.
//!
//! Trigger: `sensor.smartevse_1234_ev_plug_state` to `Disconnected`. Conditions: the override
//! has a value and is not 6. Action: MQTT `CurrentOverride` `60` (tenths).
use super::buzz_laadweigering_reset::plug_reached_disconnected;
use super::{Automation, Cx};
use crate::cascade;
use crate::command::Command;

#[derive(Debug, Default)]
pub struct BuzzResetOpUitplug;

impl Automation for BuzzResetOpUitplug {
    fn id(&self) -> &'static str {
        "ev_laden_buzz_reset_op_uitplug"
    }

    fn step(&mut self, cx: &mut Cx) {
        // `to: Disconnected`, so a plug blip to `unavailable` no longer parks a charging Buzz.
        if !plug_reached_disconnected(cx) || !cascade::buzz_ctrl(cx.w) || cx.w.evse.override_a.unwrap_or(6.0) == 6.0 {
            return;
        }
        cx.mark(self.id());
        cx.send(Command::EvseCurrentDeciA(60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;

    fn rig() -> Rig {
        let mut r = Rig::new();
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.override_a = Some(14.5);
        r
    }

    #[test]
    fn unplug_parks_the_override_at_six() {
        let mut r = rig();
        let mut a = BuzzResetOpUitplug;
        assert!(r.run(&mut a).is_empty());
        r.wait(5);
        r.w.evse.plug = Some("Disconnected".into());
        assert_eq!(r.run(&mut a), vec![Command::EvseCurrentDeciA(60)]);
        assert_eq!(r.m.last_triggered("ev_laden_buzz_reset_op_uitplug"), Some(r.now));
        r.wait(5);
        assert!(r.run(&mut a).is_empty(), "no new transition");
    }

    /// Already at 6 A, or the override itself unavailable: nothing.
    #[test]
    fn conditions() {
        for ov in [Some(6.0), None] {
            let mut r = rig();
            let mut a = BuzzResetOpUitplug;
            r.run(&mut a);
            r.wait(5);
            r.w.evse.plug = Some("Disconnected".into());
            r.w.evse.override_a = ov;
            assert!(r.run(&mut a).is_empty(), "{ov:?}");
        }
    }

    /// `to: Disconnected`: a blip of the plug alone (override still readable, 2026-11-07
    /// 12:57:30) publishes nothing; the plug returning from `unavailable` as `Disconnected` is
    /// an unplug and parks the override.
    #[test]
    fn a_plug_blip_does_not_reset_but_a_return_as_disconnected_does() {
        let mut r = rig();
        let mut a = BuzzResetOpUitplug;
        r.run(&mut a);
        r.wait(5);
        r.w.evse.plug = None;
        assert!(r.run(&mut a).is_empty(), "Connected → unavailable");
        r.wait(5);
        r.w.evse.plug = Some("Disconnected".into());
        assert_eq!(r.run(&mut a), vec![Command::EvseCurrentDeciA(60)], "unavailable → Disconnected");
    }
}
