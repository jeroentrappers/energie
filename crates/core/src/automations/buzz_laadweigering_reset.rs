//! Port of HA automation `buzz_laadweigering_reset` ("Buzz: laadweigering-teller resetten"):
//! reset `counter.buzz_laadweigering` ([`Memory::buzz_laadweigering`](crate::memory::Memory)) once
//! the car really charges (pilot to `C`) or is unplugged (plug reaches `Disconnected`), so the
//! handshake recovery ([`super::buzz_handshake`]) may try its one mode cycle again next session.
//!
//! Triggers: `sensor.smartevse_1234_stateid` to `C`; `sensor.smartevse_1234_ev_plug_state` to
//! `Disconnected` (from anything). Condition: the counter is above 0.
use super::{Automation, Cx};

#[derive(Debug, Default)]
pub struct BuzzLaadweigeringReset;

/// `trigger: state, to: Disconnected`: the plug reached `Disconnected` from any other state
/// (also from `unavailable`); a `Connected → unavailable` blip does not fire.
pub fn plug_reached_disconnected(cx: &Cx) -> bool {
    cx.pw.evse.plug.as_deref() != Some("Disconnected") && cx.w.evse.plug.as_deref() == Some("Disconnected")
}

impl Automation for BuzzLaadweigeringReset {
    fn id(&self) -> &'static str {
        "buzz_laadweigering_reset"
    }

    fn step(&mut self, cx: &mut Cx) {
        let to_c = cx.pw.evse.state_id.as_deref() != Some("C") && cx.w.evse.state_id.as_deref() == Some("C");
        if !(to_c || plug_reached_disconnected(cx)) || cx.m.buzz_laadweigering == 0 {
            return;
        }
        cx.m.buzz_laadweigering = 0;
        cx.mark(self.id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;

    fn rig() -> Rig {
        let mut r = Rig::new();
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.state_id = Some("B".into());
        r.m.buzz_laadweigering = 1;
        r
    }

    #[test]
    fn resets_when_the_pilot_reaches_c() {
        let mut r = rig();
        let mut a = BuzzLaadweigeringReset;
        r.run(&mut a);
        assert_eq!(r.m.buzz_laadweigering, 1, "start");
        r.wait(5);
        r.w.evse.state_id = Some("C".into());
        r.run(&mut a);
        assert_eq!(r.m.buzz_laadweigering, 0);
        assert_eq!(r.m.last_triggered("buzz_laadweigering_reset"), Some(r.now));
    }

    #[test]
    fn resets_on_unplug() {
        let mut r = rig();
        let mut a = BuzzLaadweigeringReset;
        r.run(&mut a);
        r.wait(5);
        r.w.evse.plug = Some("Disconnected".into());
        r.run(&mut a);
        assert_eq!(r.m.buzz_laadweigering, 0);
    }

    /// `to: Disconnected`: a Connected → unavailable blip (and back) keeps the counter; a blip
    /// that comes back as Disconnected (unavailable → Disconnected) is an unplug and resets it.
    #[test]
    fn a_plug_blip_does_not_reset_but_a_return_as_disconnected_does() {
        let mut r = rig();
        let mut a = BuzzLaadweigeringReset;
        r.run(&mut a);
        for plug in [None, Some("unavailable"), Some("Connected"), None] {
            r.wait(5);
            r.w.evse.plug = plug.map(Into::into);
            r.run(&mut a);
            assert_eq!(r.m.buzz_laadweigering, 1, "{plug:?}");
        }
        r.wait(5);
        r.w.evse.plug = Some("Disconnected".into());
        r.run(&mut a);
        assert_eq!(r.m.buzz_laadweigering, 0, "unavailable → Disconnected");
    }

    /// Already 0: the condition fails, no run is recorded. Staying in C does not re-fire.
    #[test]
    fn needs_a_count_and_a_transition() {
        let mut r = rig();
        let mut a = BuzzLaadweigeringReset;
        r.m.buzz_laadweigering = 0;
        r.run(&mut a);
        r.wait(5);
        r.w.evse.state_id = Some("C".into());
        r.run(&mut a);
        assert_eq!(r.m.last_triggered("buzz_laadweigering_reset"), None);
        r.m.buzz_laadweigering = 1;
        r.wait(5);
        r.run(&mut a);
        assert_eq!(r.m.buzz_laadweigering, 1, "still C: no transition");
    }
}
