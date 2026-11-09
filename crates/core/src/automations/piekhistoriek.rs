//! Port of HA automation `capaciteit_piekhistoriek_bijwerken`.
//!
//! Writes the month's P1 peak into `input_text.capaciteit_piekhistoriek` (`YYYY-MM:kW` with three
//! decimals, comma separated, sorted, the last 12 kept): the 12-month average Fluvius bills the
//! capacity tariff on. Two triggers: 23:59:50 on the last day of the month (`maandeinde`), and
//! the meter's own month-peak reset just after midnight on the 1st (`reset`), which carries the
//! final peak in its from-state. `mode: queued`: both runs happen, the later one replaces the
//! month's entry.

use chrono::{Datelike, Duration, Timelike};

use super::{Automation, Cx};

/// The input_text after writing `maand:kw`: entries of other months kept (anything with a
/// `:`), this month's replaced, sorted, the last 12.
fn bijwerken(hist: &str, maand: &str, kw: f64) -> String {
    let mut l: Vec<String> =
        hist.split(',').filter(|e| e.contains(':') && !e.starts_with(maand)).map(str::to_string).collect();
    // Python `'%.3f' % kw`: correctly rounded, like Rust's `{:.3}`.
    l.push(format!("{maand}:{kw:.3}"));
    l.sort();
    let skip = l.len().saturating_sub(12);
    l[skip..].join(",")
}

#[derive(Debug, Default)]
pub struct CapaciteitPiekhistoriek;

impl CapaciteitPiekhistoriek {
    /// The actions: `maand` is the month an hour ago (the closing month for both triggers).
    fn run(&self, cx: &mut Cx, peak_w: Option<f64>) {
        cx.mark(self.id());
        let maand = (cx.now.naive_local() - Duration::hours(1)).format("%Y-%m").to_string();
        let kw = peak_w.unwrap_or(0.0) / 1000.0;
        if kw > 0.0 {
            cx.m.capaciteit_piekhistoriek = bijwerken(&cx.m.capaciteit_piekhistoriek, &maand, kw);
        }
    }
}

impl Automation for CapaciteitPiekhistoriek {
    fn id(&self) -> &'static str {
        "capaciteit_piekhistoriek_bijwerken"
    }

    fn step(&mut self, cx: &mut Cx) {
        let now = cx.now.naive_local();
        if cx.daily(23, 59, 50) && (now + Duration::days(1)).day() == 1 {
            self.run(cx, cx.w.p1.month_peak_w);
        }
        let (from, to) = (cx.pw.p1.month_peak_w, cx.w.p1.month_peak_w);
        if from != to {
            // `from|float(-1) > 0 and to|float(-1) >= 0 and to|float(0) < from|float(0)`, at
            // 00:xx on the 1st.
            let reset = now.day() == 1
                && now.hour() == 0
                && from.unwrap_or(-1.0) > 0.0
                && to.unwrap_or(-1.0) >= 0.0
                && to.unwrap_or(0.0) < from.unwrap_or(0.0);
            if reset {
                self.run(cx, from);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::dagbalans::rig::Rig;
    use super::*;
    use crate::time::at;

    /// The live value of 2026-11-07.
    const LIVE: &str = "2025-11:6.572,2025-12:8.011,2026-01:6.748,2026-02:6.299,2026-03:7.046,2026-04:6.180,\
                        2026-05:8.791,2026-06:5.929,2026-07:3.400,2026-08:2.940,2026-09:3.910,2026-10:4.916";

    /// `capaciteit_piekhistoriek_bijwerken`: adding November drops the oldest month (2025-11)
    /// so 12 remain, sorted; writing November again replaces it instead of adding a 13th.
    #[test]
    fn piekhistoriek_keeps_12_months_sorted_and_replaces_the_current_month() {
        let h = bijwerken(LIVE, "2026-11", 3.892);
        assert!(h.starts_with("2025-12:8.011,"));
        assert!(h.ends_with(",2026-10:4.916,2026-11:3.892"));
        assert_eq!(h.split(',').count(), 12);
        let h2 = bijwerken(&h, "2026-11", 4.2);
        assert_eq!(h2, h.replace("2026-11:3.892", "2026-11:4.200"));
        // Out-of-order and junk entries: sorted, junk without `:` dropped.
        assert_eq!(
            bijwerken("2026-02:1.000,junk,,2025-12:2.5", "2026-01", 3.0),
            "2025-12:2.5,2026-01:3.000,2026-02:1.000"
        );
        assert_eq!(bijwerken("", "2026-01", 2.0625), "2026-01:2.062", "'%.3f' rounds the exact binary value");
    }

    /// `maandeinde` at 23:59:50 on the last day of the month writes the meter's current peak
    /// (3892 W → 3.892); on other days nothing happens.
    #[test]
    fn the_month_end_writes_the_current_peak() {
        let mut r = Rig::new(at(2026, 11, 30, 23, 59, 49));
        r.m.capaciteit_piekhistoriek = LIVE.into();
        r.w.p1.month_peak_w = Some(3892.0);
        r.pw = r.w.clone();
        let mut a = CapaciteitPiekhistoriek;
        r.step(&mut a);
        assert_eq!(r.m.capaciteit_piekhistoriek, LIVE);
        r.tick(&mut a, 1);
        assert!(r.m.capaciteit_piekhistoriek.ends_with(",2026-11:3.892"));
        assert_eq!(r.m.last_triggered("capaciteit_piekhistoriek_bijwerken"), Some(r.now));

        let mut r = Rig::new(at(2026, 11, 29, 23, 59, 50));
        r.w.p1.month_peak_w = Some(3892.0);
        r.pw = r.w.clone();
        r.step(&mut a);
        assert_eq!(r.m.capaciteit_piekhistoriek, "", "the 30th follows");
    }

    /// `reset`: the P1 month peak drops to 0 at 00:00:05 on the 1st; the from-state is the
    /// final peak of the closing month (a peak in the last 10 s is caught here), stored under
    /// that month (now − 1 h = 31 October).
    #[test]
    fn the_meter_reset_on_the_1st_records_the_final_peak() {
        let mut r = Rig::new(at(2026, 11, 1, 0, 0, 4));
        r.m.capaciteit_piekhistoriek = "2026-10:4.800".into();
        r.w.p1.month_peak_w = Some(4916.0);
        let mut a = CapaciteitPiekhistoriek;
        r.advance(1);
        r.w.p1.month_peak_w = Some(0.0);
        r.step(&mut a);
        assert_eq!(r.m.capaciteit_piekhistoriek, "2026-10:4.916");
    }

    /// `reset` conditions: only at 00:xx on the 1st, only a decrease from a positive value to
    /// a valid one. A rise, a drop to unavailable, a drop at 01:00, or from unavailable: no.
    #[test]
    fn only_a_real_reset_counts() {
        let cases = [
            ("rise", at(2026, 11, 1, 0, 5, 0), Some(100.0), Some(200.0)),
            ("to unavailable", at(2026, 11, 1, 0, 5, 0), Some(4916.0), None),
            ("from unavailable", at(2026, 11, 1, 0, 5, 0), None, Some(0.0)),
            ("from zero", at(2026, 11, 1, 0, 5, 0), Some(0.0), Some(-1.0)),
            ("01:00", at(2026, 11, 1, 1, 0, 0), Some(4916.0), Some(0.0)),
            ("the 2nd", at(2026, 11, 2, 0, 5, 0), Some(4916.0), Some(0.0)),
        ];
        for (name, now, from, to) in cases {
            let mut r = Rig::new(now);
            r.pw.p1.month_peak_w = from;
            r.w.p1.month_peak_w = to;
            r.step(&mut CapaciteitPiekhistoriek);
            assert_eq!(r.m.capaciteit_piekhistoriek, "", "{name}");
            assert_eq!(r.m.last_triggered("capaciteit_piekhistoriek_bijwerken"), None, "{name}");
        }
    }

    /// The action-level `kw > 0` condition: an unavailable peak at month end runs the
    /// automation (last_triggered moves) but writes nothing.
    #[test]
    fn an_unavailable_peak_at_month_end_writes_nothing() {
        let mut r = Rig::new(at(2026, 12, 31, 23, 59, 50));
        r.m.capaciteit_piekhistoriek = LIVE.into();
        r.step(&mut CapaciteitPiekhistoriek);
        assert_eq!(r.m.capaciteit_piekhistoriek, LIVE);
        assert_eq!(r.m.last_triggered("capaciteit_piekhistoriek_bijwerken"), Some(r.now));
    }
}
