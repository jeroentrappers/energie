//! Port of HA automation `smartevse_plug_latch` ("SmartEVSE: plug-status vasthouden"): keep the
//! last VALID SmartEVSE plug state in `input_text.smartevse_plug_laatst_bekend`
//! ([`Memory::smartevse_plug_laatst_bekend`](crate::memory::Memory)), so that consumers reading
//! [`cascade::buzz_plug_known`] see through the integration's dozens of daily `unavailable` blips.
//! A real unplug arrives as an explicit `Disconnected` and updates the latch.
//!
//! Triggers: a plug state change, `time_pattern minutes: /1`, and `homeassistant: start`.
//! Condition: the plug has a value and differs from the latch.
use super::{Automation, Cx};
use crate::cascade;

#[derive(Debug, Default)]
pub struct SmartevsePlugLatch;

impl Automation for SmartevsePlugLatch {
    fn id(&self) -> &'static str {
        "smartevse_plug_latch"
    }

    fn step(&mut self, cx: &mut Cx) {
        let fired = cx.started || cx.every(60) || cx.w.evse.plug != cx.pw.evse.plug;
        if !fired {
            return;
        }
        let Some(p) = cx.w.evse.plug.as_deref().filter(|p| !cascade::is_missing(Some(p))) else {
            return;
        };
        if p == cx.m.smartevse_plug_laatst_bekend {
            return;
        }
        cx.m.smartevse_plug_laatst_bekend = p.to_string();
        cx.mark(self.id());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;

    /// The latch follows a valid plug state at start and on a change.
    #[test]
    fn latches_valid_states() {
        let mut r = Rig::new();
        let mut a = SmartevsePlugLatch;
        r.w.evse.plug = Some("Connected".into());
        r.run(&mut a);
        assert_eq!(r.m.smartevse_plug_laatst_bekend, "Connected", "homeassistant: start");
        assert_eq!(r.m.last_triggered("smartevse_plug_latch"), Some(r.now));
        r.wait(5);
        r.w.evse.plug = Some("Disconnected".into());
        r.run(&mut a);
        assert_eq!(r.m.smartevse_plug_laatst_bekend, "Disconnected");
    }

    /// An unavailable blip never overwrites the latch.
    #[test]
    fn a_blip_keeps_the_latch() {
        let mut r = Rig::new();
        let mut a = SmartevsePlugLatch;
        r.m.smartevse_plug_laatst_bekend = "Connected".into();
        r.run(&mut a);
        for blip in [None, Some("unavailable"), Some("unknown"), Some("none"), Some("")] {
            r.wait(5);
            r.w.evse.plug = blip.map(str::to_string);
            r.run(&mut a);
            assert_eq!(r.m.smartevse_plug_laatst_bekend, "Connected", "{blip:?}");
        }
        assert_eq!(r.m.last_triggered("smartevse_plug_latch"), None, "condition never passed");
    }

    /// The /1 min pattern catches a latch that was out of date with no plug change (e.g. the
    /// latch edited by hand); an unchanged, equal state does not run.
    #[test]
    fn the_minute_pattern_resyncs() {
        let mut r = Rig::new();
        let mut a = SmartevsePlugLatch;
        r.w.evse.plug = Some("Connected".into());
        r.run(&mut a);
        r.wait(5);
        r.m.smartevse_plug_laatst_bekend = "Disconnected".into();
        r.run(&mut a);
        assert_eq!(r.m.smartevse_plug_laatst_bekend, "Disconnected", "no trigger at 12:00:15");
        r.wait(45); // 12:01:00
        r.run(&mut a);
        assert_eq!(r.m.smartevse_plug_laatst_bekend, "Connected");
        let t = r.now;
        r.wait(60);
        r.run(&mut a);
        assert_eq!(r.m.last_triggered("smartevse_plug_latch"), Some(t), "equal: condition fails");
    }
}
