//! The day-ahead curves for the price charts (`GET /api/prices`): the offtake slots as the feed
//! has them (today and, once published, tomorrow), the injection slots recomputed from them, and
//! the cheapest and dearest quarter over the same window (as `price_backup.py`'s
//! `stroomprijs_laagste` / `_hoogste` take them since 2026-11-07).

use energie_core::world::Prices;
use energie_drivers::prices::Tariff;
use serde::Serialize;

/// One slot: start (Unix seconds) and EUR/kWh.
pub type Point = (i64, f64);

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Extreme {
    pub t: i64,
    pub price: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Curve {
    pub afname: Vec<Point>,
    pub injectie: Vec<Point>,
    pub laagste: Option<Extreme>,
    pub hoogste: Option<Extreme>,
    pub afname_has_tomorrow: bool,
}

/// The injection price of the slot whose offtake price is `afname`: the offtake tariff undone to
/// the raw EPEX price, then the injection tariff applied (both round to 5 decimals, as the feed).
pub fn injectie_from_afname(afname: f64) -> f64 {
    let o = Tariff::OFFTAKE;
    let eur_mwh = (afname / (1.0 + o.vat) - o.offset) / o.slope * 1000.0;
    Tariff::INJECTION.apply(eur_mwh)
}

pub fn curve(p: &Prices) -> Curve {
    let afname: Vec<Point> = p.afname_slots.iter().map(|s| (s.start.timestamp(), s.price)).collect();
    let injectie = afname.iter().map(|&(t, v)| (t, injectie_from_afname(v))).collect();
    let pick = |better: fn(f64, f64) -> bool| {
        afname.iter().fold(None, |best: Option<Extreme>, &(t, price)| match best {
            Some(b) if !better(price, b.price) => Some(b),
            _ => Some(Extreme { t, price }),
        })
    };
    Curve {
        laagste: pick(|a, b| a < b),
        hoogste: pick(|a, b| a > b),
        afname,
        injectie,
        afname_has_tomorrow: p.afname_has_tomorrow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use energie_core::time::{TZ, Ts};
    use energie_core::world::PriceSlot;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Ts {
        TZ.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()
    }

    /// The live 13:45 slot of 2026-11-07: offtake 0.37692 gives the injection feed's 0.2099.
    #[test]
    fn injection_follows_from_the_offtake_price() {
        assert_eq!(injectie_from_afname(0.37692), 0.2099);
        assert_eq!(injectie_from_afname(Tariff::OFFTAKE.apply(-12.5)), Tariff::INJECTION.apply(-12.5));
    }

    /// Slots, injection slots and the first lowest / highest quarter.
    #[test]
    fn curve_of_the_slots() {
        let mut p = Prices::default();
        assert_eq!(curve(&p).laagste, None);
        p.afname_slots = [(0, 0.30), (15, 0.25), (30, 0.40), (45, 0.25), (60, 0.40)]
            .iter()
            .map(|&(m, price)| PriceSlot { start: at(2026, 11, 8, 0, 0, 0) + chrono::Duration::minutes(m), price })
            .collect();
        p.afname_has_tomorrow = true;
        let c = curve(&p);
        let t0 = at(2026, 11, 8, 0, 0, 0).timestamp();
        assert_eq!(c.afname.len(), 5);
        assert_eq!(c.injectie[0], (t0, injectie_from_afname(0.30)));
        assert_eq!(c.laagste, Some(Extreme { t: t0 + 900, price: 0.25 }));
        assert_eq!(c.hoogste, Some(Extreme { t: t0 + 1800, price: 0.40 }));
        assert!(c.afname_has_tomorrow);
    }
}
