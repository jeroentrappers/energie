//! Port of HA automation `buzz_mode_stempel` ("Buzz: modewissel-stempel",
//! `automation.buzz_modewissel_stempel`): its `last_triggered` is the time of the last REAL
//! SmartEVSE mode change, which the charge-session protection measures from (LOGIC.md
//! "Charge-session protection": `MIN_PAUSE_S`, `MIN_CHARGE_S`; read through
//! [`super::ev_modulator::since_mode_change`]).
//!
//! Trigger: a state change of `select.smartevse_1234_mode`. Condition: the new state is valid
//! (`World.evse.mode` is `Some`) and differs from `input_text.smartevse_modus_laatst_bekend`
//! ([`Memory::smartevse_modus_laatst_bekend`](crate::memory::Memory)), the last valid mode.
//! Action: the helper takes the new mode. A blip that comes back to the same mode is not
//! stamped; a change made during a blip (Pause → unavailable → Normal) is.
use super::{Automation, Cx};

#[derive(Debug, Default)]
pub struct BuzzModeStempel;

impl Automation for BuzzModeStempel {
    fn id(&self) -> &'static str {
        "buzz_mode_stempel"
    }

    fn step(&mut self, cx: &mut Cx) {
        if cx.pw.evse.mode == cx.w.evse.mode {
            return;
        }
        if let Some(to) = cx.w.evse.mode
            && to.label() != cx.m.smartevse_modus_laatst_bekend
        {
            cx.mark(self.id());
            cx.m.smartevse_modus_laatst_bekend = to.label().to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;
    use crate::world::EvseMode;

    /// `buzz_mode_stempel`: stamped when the select changes to a valid mode other than the
    /// helper's; a blip back to the same mode is not, a change across a blip is, and the helper
    /// follows the stamped mode.
    #[test]
    fn stamps_a_real_mode_change_only() {
        let mut r = Rig::new();
        let mut a = BuzzModeStempel;
        r.w.evse.mode = Some(EvseMode::Normal);
        r.m.smartevse_modus_laatst_bekend = "Normal".into();
        r.run(&mut a);
        assert_eq!(r.m.last_triggered("buzz_mode_stempel"), None, "start is not a change");
        r.wait(5);
        r.w.evse.mode = Some(EvseMode::Pause);
        r.run(&mut a);
        let t = r.now;
        assert_eq!(r.m.last_triggered("buzz_mode_stempel"), Some(t));
        assert_eq!(r.m.smartevse_modus_laatst_bekend, "Pause");
        // Pause → unavailable → Pause: a blip, not stamped.
        r.wait(5);
        r.w.evse.mode = None;
        r.run(&mut a);
        assert_eq!(r.m.smartevse_modus_laatst_bekend, "Pause", "unavailable is not stored");
        r.wait(5);
        r.w.evse.mode = Some(EvseMode::Pause);
        r.run(&mut a);
        assert_eq!(r.m.last_triggered("buzz_mode_stempel"), Some(t));
        // Pause → unavailable → Normal: a real change, stamped on the return.
        r.wait(5);
        r.w.evse.mode = None;
        r.run(&mut a);
        r.wait(5);
        r.w.evse.mode = Some(EvseMode::Normal);
        r.run(&mut a);
        let t2 = r.now;
        assert_eq!(r.m.last_triggered("buzz_mode_stempel"), Some(t2));
        assert_eq!(r.m.smartevse_modus_laatst_bekend, "Normal");
        // No change at all: nothing.
        r.wait(5);
        r.run(&mut a);
        assert_eq!(r.m.last_triggered("buzz_mode_stempel"), Some(t2));
    }

    /// The helper, not the previous state, decides: a select change to the mode the helper
    /// already holds (e.g. Normal → Solar with the helper still at Solar after a restart)
    /// stamps nothing; without a trigger a differing helper is left alone.
    #[test]
    fn a_change_to_the_known_mode_is_not_stamped() {
        let mut r = Rig::new();
        let mut a = BuzzModeStempel;
        r.w.evse.mode = Some(EvseMode::Normal);
        r.m.smartevse_modus_laatst_bekend = "Solar".into();
        r.run(&mut a);
        r.wait(5);
        r.run(&mut a);
        assert_eq!(r.m.smartevse_modus_laatst_bekend, "Solar", "no state change, no trigger");
        r.wait(5);
        r.w.evse.mode = Some(EvseMode::Solar);
        r.run(&mut a);
        assert_eq!(r.m.last_triggered("buzz_mode_stempel"), None);
    }
}
