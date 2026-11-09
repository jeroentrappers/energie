//! Port of HA automation `entsoe_qh_kwartier_verversen` ("ENTSO-E kwartierprijs verversen",
//! added on 2026-11-07).
//!
//! Trigger: `time_pattern minutes: /15, seconds: 5`. Action:
//! `homeassistant.update_entity` on `sensor.entso_e_energy_prices_15_min`: the `entsoe_qh`
//! integration polls every 30 min and only then picks the current slot, so without it the
//! 15-min price lags a quarter half of the time.
//!
//! In the port that refresh has nothing to do: the native price driver
//! (`drivers::prices`) computes `World.prices.epex_now` from the slot that contains *now* on
//! every read, and its fetch schedule does not depend on the quarter. So the automation keeps
//! its HA id and its `last_triggered` (like HA's), and sends no command; sending
//! [`crate::command::Command::RefreshPrices`] would only re-download the day's prices every
//! 15 min.

use super::{Automation, Cx};
use crate::time;

/// `minutes: /15`.
pub const PERIOD_S: i64 = 900;
/// `seconds: 5`.
pub const OFFSET_S: i64 = 5;

#[derive(Debug, Default)]
pub struct EntsoeQhKwartierVerversen;

impl Automation for EntsoeQhKwartierVerversen {
    fn id(&self) -> &'static str {
        "entsoe_qh_kwartier_verversen"
    }

    fn step(&mut self, cx: &mut Cx) {
        let shift = chrono::Duration::seconds(OFFSET_S);
        if cx.started || !time::crossed_period(cx.prev_now - shift, cx.now - shift, PERIOD_S) {
            return;
        }
        cx.mark(self.id());
    }
}

#[cfg(test)]
mod tests {
    use super::super::dagbalans::rig::Rig;
    use super::*;
    use crate::time::at;

    /// `entsoe_qh_kwartier_verversen`: fires at hh:00:05, :15:05, :30:05, :45:05 (recorded as
    /// `last_triggered`), never on the start tick, and sends no command (the native driver
    /// already serves the current quarter).
    #[test]
    fn it_runs_five_seconds_past_each_quarter_and_sends_nothing() {
        for (m, s, fires) in [(15, 5, true), (0, 5, true), (45, 5, true), (15, 4, false), (15, 6, false), (7, 5, false)]
        {
            let mut r = Rig::new(at(2026, 11, 7, 14, m, s));
            assert!(r.step(&mut EntsoeQhKwartierVerversen).is_empty());
            assert_eq!(r.m.last_triggered("entsoe_qh_kwartier_verversen").is_some(), fires, "14:{m:02}:{s:02}");
        }
        let mut r = Rig::new(at(2026, 11, 7, 14, 30, 5));
        r.started = true;
        r.step(&mut EntsoeQhKwartierVerversen);
        assert_eq!(r.m.last_triggered("entsoe_qh_kwartier_verversen"), None, "start tick");
    }
}
