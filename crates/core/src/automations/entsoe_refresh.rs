//! Port of HA automation `entsoe_dayahead_morgen_ophalen`.
//!
//! Every 2 min (and at start) inside the publication window 13:00–23:59, while either price
//! feed still lacks tomorrow's prices, force a re-fetch of both. Stops by itself once both have
//! tomorrow.

use chrono::NaiveTime;

use super::{Automation, Cx};
use crate::command::Command;

#[derive(Debug, Default)]
pub struct EntsoeDayaheadMorgen;

impl Automation for EntsoeDayaheadMorgen {
    fn id(&self) -> &'static str {
        "entsoe_dayahead_morgen_ophalen"
    }

    fn step(&mut self, cx: &mut Cx) {
        if !(cx.every(120) || cx.started) {
            return;
        }
        let t = cx.now.time();
        // `after: 13:00:00` inclusive, `before: 23:59:00` exclusive.
        let in_window = NaiveTime::from_hms_opt(13, 0, 0).expect("valid") <= t
            && t < NaiveTime::from_hms_opt(23, 59, 0).expect("valid");
        let p = &cx.w.prices;
        // `prices_tomorrow|count == 0` on either feed.
        if !(in_window && (!p.afname_has_tomorrow || !p.injectie_has_tomorrow)) {
            return;
        }
        cx.mark(self.id());
        cx.send(Command::RefreshPrices);
    }
}

#[cfg(test)]
mod tests {
    use super::super::dagbalans::rig::Rig;
    use super::*;
    use crate::time::at;

    /// Run once at `now`, reached from one second earlier.
    fn at_time(h: u32, m: u32, s: u32, afname: bool, injectie: bool) -> Vec<Command> {
        let mut r = Rig::new(at(2026, 11, 7, h, m, s));
        r.w.prices.afname_has_tomorrow = afname;
        r.w.prices.injectie_has_tomorrow = injectie;
        r.step(&mut EntsoeDayaheadMorgen)
    }

    /// `entsoe_dayahead_morgen_ophalen`: on the even minute inside 13:00–23:59, refresh while
    /// either feed lacks tomorrow; stop once both have it.
    #[test]
    fn prices_are_refreshed_every_2_min_until_both_feeds_have_tomorrow() {
        assert_eq!(at_time(13, 0, 0, false, false), vec![Command::RefreshPrices], "13:00 inclusive");
        assert_eq!(at_time(14, 2, 0, true, false), vec![Command::RefreshPrices], "injection missing");
        assert_eq!(at_time(14, 2, 0, false, true), vec![Command::RefreshPrices], "offtake missing");
        assert!(at_time(14, 2, 0, true, true).is_empty(), "both have tomorrow");
        assert!(at_time(14, 3, 0, false, false).is_empty(), "odd minute: no trigger");
        assert!(at_time(12, 58, 0, false, false).is_empty(), "before the window");
        assert_eq!(at_time(23, 58, 0, false, false).len(), 1, "23:58 is inside");
    }

    /// The `homeassistant: start` trigger: a restart in the window refreshes at once; one at
    /// 23:59:00 or later is past `before: 23:59:00` (HA's `time` condition: exclusive).
    #[test]
    fn a_start_inside_the_window_refreshes_at_once() {
        let mut r = Rig::new(at(2026, 11, 7, 15, 7, 13));
        r.started = true;
        assert_eq!(r.step(&mut EntsoeDayaheadMorgen), vec![Command::RefreshPrices]);
        assert_eq!(r.m.last_triggered("entsoe_dayahead_morgen_ophalen"), Some(r.now));
        let mut r = Rig::new(at(2026, 11, 7, 23, 58, 59));
        r.started = true;
        assert_eq!(r.step(&mut EntsoeDayaheadMorgen), vec![Command::RefreshPrices], "23:58:59 is inside");
        let mut r = Rig::new(at(2026, 11, 7, 23, 59, 0));
        r.started = true;
        assert!(r.step(&mut EntsoeDayaheadMorgen).is_empty(), "`before: 23:59:00` is exclusive");
        let mut r = Rig::new(at(2026, 11, 7, 23, 59, 30));
        r.started = true;
        assert!(r.step(&mut EntsoeDayaheadMorgen).is_empty());
    }
}
