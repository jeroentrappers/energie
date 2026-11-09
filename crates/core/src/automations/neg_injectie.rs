//! Port of HA automations `neg_injectie_curtail` and `neg_injectie_herstel`.
//!
//! Negative injection price + full battery + exporting: set the Solis export limit (backflow)
//! to 0 so the hybrid curtails its own PV. Put it back to 5500 W once the price is ≥ 0 again.
//! A numeric_state on an unavailable entity never matches (`None` is false).

use super::{Automation, Cx};
use crate::battery::fmt_float_state;
use crate::command::Command;
use crate::trigger::Held;

fn below(x: Option<f64>, lim: f64) -> bool {
    x.is_some_and(|v| v < lim)
}

fn above(x: Option<f64>, lim: f64) -> bool {
    x.is_some_and(|v| v > lim)
}

#[derive(Debug, Default)]
pub struct NegInjectieCurtail {
    /// `injectie price below: 0 for: 00:03:00`.
    price_negative: Held,
    /// `battery soc above: 96 for: 00:02:00`.
    soc_full: Held,
}

impl Automation for NegInjectieCurtail {
    fn id(&self) -> &'static str {
        "neg_injectie_curtail"
    }

    fn step(&mut self, cx: &mut Cx) {
        let price = cx.w.prices.injectie_now;
        let soc = cx.w.solis.soc;
        let by_price = self.price_negative.update(below(price, 0.0), cx.now, 180.0);
        let by_soc = self.soc_full.update(above(soc, 96.0), cx.now, 120.0);
        let conditions = below(price, 0.0)
            && above(soc, 96.0)
            && below(cx.w.p1.power_w, -100.0)
            && above(cx.w.solis.backflow_w, 100.0);
        if !(by_price || by_soc) || !conditions {
            return;
        }
        cx.send(Command::SolisFeedInLimit(true));
        cx.send(Command::SolisBackflowW(0));
        cx.send(Command::notify(
            "⛔ PV-export begrensd",
            format!("Injectie negatief ({} EUR/kWh) + batterij vol → Solis-export op 0 gezet.", fmt_float_state(price)),
        ));
        let id = self.id();
        cx.mark(id);
    }
}

#[derive(Debug, Default)]
pub struct NegInjectieHerstel {
    /// `injectie price above: -0.0001 for: 00:03:00`.
    price_back: Held,
}

impl Automation for NegInjectieHerstel {
    fn id(&self) -> &'static str {
        "neg_injectie_herstel"
    }

    fn step(&mut self, cx: &mut Cx) {
        let fired = self.price_back.update(above(cx.w.prices.injectie_now, -0.0001), cx.now, 180.0);
        if !fired || !below(cx.w.solis.backflow_w, 100.0) {
            return;
        }
        // Only the limit goes back to 5500 W; the feed-in limit switch stays on, which is its
        // normal state (docs/FINDINGS.md: not a config issue).
        cx.send(Command::SolisBackflowW(5500));
        cx.send(Command::notify("✅ PV-export hersteld", "Injectieprijs weer >= 0 → export-limiet terug op 5500 W."));
        let id = self.id();
        cx.mark(id);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::battery::rig::Rig;
    use crate::time::{Ts, at};

    fn t0() -> Ts {
        at(2026, 5, 10, 13, 0, 0)
    }

    fn secs(s: i64) -> Ts {
        t0() + Duration::seconds(s)
    }

    /// Started with a positive price, the battery at `soc`, exporting 2 kW, export limit 5500 W.
    fn curtail_rig_at(soc: f64) -> Rig<NegInjectieCurtail> {
        let mut r = Rig::new(NegInjectieCurtail::default());
        r.w.prices.injectie_now = Some(0.01);
        r.w.solis.soc = Some(soc);
        r.w.p1.power_w = Some(-2000.0);
        r.w.solis.backflow_w = Some(5500.0);
        r.tick(t0());
        r
    }

    fn curtail_rig() -> Rig<NegInjectieCurtail> {
        curtail_rig_at(90.0)
    }

    fn curtail_commands(price: &str) -> Vec<Command> {
        vec![
            Command::SolisFeedInLimit(true),
            Command::SolisBackflowW(0),
            Command::notify(
                "⛔ PV-export begrensd",
                format!("Injectie negatief ({price} EUR/kWh) + batterij vol → Solis-export op 0 gezet."),
            ),
        ]
    }

    /// Trigger `price below 0 for 3 min` with the battery already full (full since start, so its
    /// own trigger never fires).
    #[test]
    fn curtails_three_minutes_into_a_negative_price() {
        let mut r = curtail_rig_at(97.0);
        r.tick(secs(1));
        r.w.prices.injectie_now = Some(-0.0123);
        r.tick(secs(2));
        assert!(r.tick(secs(181)).is_empty(), "2:59");
        assert_eq!(r.tick(secs(182)), curtail_commands("-0.0123"));
    }

    /// Trigger `soc above 96 for 2 min` with the price already negative (its own trigger was
    /// blocked by the conditions while the battery could still absorb).
    #[test]
    fn curtails_two_minutes_after_the_battery_fills() {
        let mut r = curtail_rig();
        r.w.prices.injectie_now = Some(-0.05);
        r.tick(secs(1));
        assert!(r.tick(secs(181)).is_empty(), "price trigger fired, SoC 90 fails the condition");
        r.w.solis.soc = Some(96.0);
        r.tick(secs(182));
        r.w.solis.soc = Some(97.0);
        r.tick(secs(183));
        assert!(r.tick(secs(302)).is_empty(), "1:59");
        assert_eq!(r.tick(secs(303)), curtail_commands("-0.05"));
    }

    /// Each condition: exporting more than 100 W, export limit above 100 W, values available.
    #[test]
    fn curtail_conditions() {
        type Poke = fn(&mut Rig<NegInjectieCurtail>);
        let pokes: [Poke; 4] = [
            |r| r.w.p1.power_w = Some(-100.0),
            |r| r.w.p1.power_w = None,
            |r| r.w.solis.backflow_w = Some(100.0),
            |r| r.w.solis.backflow_w = None,
        ];
        for poke in pokes {
            let mut r = curtail_rig();
            r.w.solis.soc = Some(97.0);
            r.w.prices.injectie_now = Some(-0.01);
            poke(&mut r);
            r.tick(secs(1));
            assert!(r.tick(secs(181)).is_empty());
        }
        let mut r = curtail_rig();
        r.w.solis.soc = None;
        r.w.prices.injectie_now = Some(-0.01);
        r.tick(secs(1));
        assert!(r.tick(secs(181)).is_empty(), "unavailable SoC is not above 96");
    }

    fn herstel_rig() -> Rig<NegInjectieHerstel> {
        let mut r = Rig::new(NegInjectieHerstel::default());
        r.w.prices.injectie_now = Some(-0.01);
        r.w.solis.backflow_w = Some(0.0);
        r.tick(t0());
        r
    }

    /// `price above -0.0001 for 3 min` and `backflow below 100`: back to 5500 W. A price of
    /// exactly 0 counts as recovered.
    #[test]
    fn restores_the_export_limit_three_minutes_after_the_price_recovers() {
        let mut r = herstel_rig();
        r.w.prices.injectie_now = Some(0.0);
        r.tick(secs(1));
        assert!(r.tick(secs(180)).is_empty());
        assert_eq!(
            r.tick(secs(181)),
            vec![
                Command::SolisBackflowW(5500),
                Command::notify("✅ PV-export hersteld", "Injectieprijs weer >= 0 → export-limiet terug op 5500 W."),
            ]
        );
    }

    /// Not when the limit is already up; not on `-0.0001` itself; not when unavailable.
    #[test]
    fn herstel_conditions() {
        let mut r = herstel_rig();
        r.w.solis.backflow_w = Some(100.0);
        r.w.prices.injectie_now = Some(0.02);
        r.tick(secs(1));
        assert!(r.tick(secs(181)).is_empty());
        let mut r = herstel_rig();
        r.w.prices.injectie_now = Some(-0.0001);
        r.tick(secs(1));
        assert!(r.tick(secs(181)).is_empty());
        let mut r = herstel_rig();
        r.w.prices.injectie_now = None;
        r.tick(secs(1));
        assert!(r.tick(secs(181)).is_empty());
    }
}
