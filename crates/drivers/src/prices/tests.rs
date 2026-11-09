use std::sync::Mutex;

use super::*;
use crate::sim::prices::{ENERGY_CHARTS_FIXTURE, ENTSOE_FIXTURE, PriceSim};

/// A made-up ENTSO-E token.
const TOKEN: &str = "00000000-test-token";

fn at(d: u32, h: u32, m: u32) -> Ts {
    TZ.with_ymd_and_hms(2026, 11, d, h, m, 0).unwrap()
}

fn day(d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
}

struct Rig {
    sim: PriceSim,
    now: Arc<Mutex<Ts>>,
    feed: PriceFeed,
}

impl Rig {
    async fn new(token: Option<&str>) -> Rig {
        let sim = PriceSim::start(TOKEN).await;
        let now = Arc::new(Mutex::new(at(7, 13, 47)));
        let n = now.clone();
        let cfg = PricesConfig {
            energy_charts_url: sim.energy_charts_url(),
            entsoe_url: sim.entsoe_url(),
            entsoe_token: token.map(str::to_string),
            clock: Arc::new(move || *n.lock().unwrap()),
            ..PricesConfig::belgium(None)
        };
        Rig { feed: PriceFeed::new(cfg).unwrap(), sim, now }
    }

    fn set_now(&self, t: Ts) {
        *self.now.lock().unwrap() = t;
    }

    async fn prices(&mut self) -> Result<Prices> {
        match self.feed.read().await? {
            Reading::Prices(p) => Ok(p),
            _ => unreachable!(),
        }
    }
}

/// `price_backup.py` tariffs on the 13:45 slot of 2026-11-07 (229.49 EUR/MWh) give exactly what
/// HA showed: offtake 0.37692 (`sensor.jeroen_current_electricity_market_price`,
/// `input_number.prijs_backup_nu`), injection 0.2099.
#[test]
fn tariffs_match_live_ha_states() {
    assert_eq!(Tariff::OFFTAKE.apply(229.49), 0.37692);
    assert_eq!(Tariff::INJECTION.apply(229.49), 0.2099);
}

/// `round(x, 5)` is Python's: correctly rounded on the binary value, exact ties to even.
#[test]
fn python_rounding() {
    assert_eq!(py_round(2.675, 2), 2.67);
    assert_eq!(py_round(0.125, 2), 0.12);
    assert_eq!(py_round(0.375, 2), 0.38);
    assert_eq!(py_round(18.55, 1), 18.6);
    // Values where scaling first (`(x * 1e5).round_ties_even() / 1e5`) would differ from Python.
    assert_eq!(py_round(-0.557275, 5), -0.55727);
    assert_eq!(py_round(1.649535, 5), 1.64953);
    assert_eq!(py_round(-1.016005, 5), -1.01601);
}

/// `entsoe_qh` rounds the raw price to 4 decimals of EUR/kWh, half away from zero.
#[test]
fn epex_rounding_is_half_up() {
    assert_eq!(epex_kwh(229.49), 0.2295);
    assert_eq!(epex_kwh(226.65), 0.2267);
    assert_eq!(epex_kwh(226.64), 0.2266);
    assert_eq!(epex_kwh(-12.35), -0.0124);
}

/// The captured energy-charts answer holds both days (192 quarters); `null` prices are skipped,
/// and mismatched arrays or garbage are errors.
#[test]
fn energy_charts_parsing() {
    let slots = parse_energy_charts(ENERGY_CHARTS_FIXTURE).unwrap();
    assert_eq!(slots.len(), 192);
    assert_eq!(slots[0], RawSlot { start: at(7, 0, 0).with_timezone(&Utc), eur_mwh: 263.94 });
    let with_null = r#"{"unix_seconds":[1794006000,1794006900],"price":[null,1.5]}"#;
    assert_eq!(parse_energy_charts(with_null).unwrap().len(), 1);
    assert!(matches!(parse_energy_charts(r#"{"unix_seconds":[1],"price":[]}"#), Err(DriverError::Malformed(_))));
    assert!(matches!(parse_energy_charts("<html/>"), Err(DriverError::Malformed(_))));
}

/// The captured ENTSO-E document (191 points, one position left out by A03 compression)
/// decodes to exactly the energy-charts series: the missing quarter repeats the previous price.
#[test]
fn entsoe_fixture_matches_energy_charts() {
    assert_eq!(parse_entsoe(ENTSOE_FIXTURE).unwrap(), parse_energy_charts(ENERGY_CHARTS_FIXTURE).unwrap());
}

fn doc(periods: &str) -> String {
    format!(
        r#"<Publication_MarketDocument xmlns="urn:x"><TimeSeries>{periods}</TimeSeries></Publication_MarketDocument>"#
    )
}

fn period(start: &str, res: &str, points: &[(i64, f64)]) -> String {
    let pts: String = points
        .iter()
        .map(|(p, v)| format!("<Point><position>{p}</position><price.amount>{v}</price.amount></Point>"))
        .collect();
    format!(
        "<Period><timeInterval><start>{start}</start><end>x</end></timeInterval><resolution>{res}</resolution>{pts}</Period>"
    )
}

/// Hourly periods become four quarters; other resolutions, empty periods and a second period
/// with an already-seen start are skipped.
#[test]
fn entsoe_period_handling() {
    let xml = doc(&[
        period("2026-11-06T23:00Z", "PT60M", &[(1, 10.0), (2, 20.0)]),
        period("2026-11-06T23:00Z", "PT15M", &[(1, 99.0)]),
        period("2026-11-07T01:00Z", "PT30M", &[(1, 99.0)]),
        period("2026-11-07T02:00Z", "PT15M", &[]),
    ]
    .concat());
    let slots = parse_entsoe(&xml).unwrap();
    assert_eq!(slots.len(), 8);
    assert_eq!(slots[3].eur_mwh, 10.0);
    assert_eq!(slots[4].eur_mwh, 20.0);
    assert_eq!(slots[7].start, Utc.with_ymd_and_hms(2026, 11, 7, 0, 45, 0).unwrap());
    let half_point = period("2026-11-06T23:00Z", "PT15M", &[(1, 5.0)])
        .replace("</Period>", "<Point><position>2</position></Point></Period>");
    assert_eq!(parse_entsoe(&doc(&half_point)).unwrap().len(), 1, "a point without a price is skipped");
}

/// No data is an `Acknowledgement_MarketDocument`; its reason is the error. Broken XML and bad
/// timestamps are errors too.
#[test]
fn entsoe_errors() {
    let ack = r#"<Acknowledgement_MarketDocument><Reason><code>999</code><text>No matching data found</text></Reason></Acknowledgement_MarketDocument>"#;
    assert!(matches!(parse_entsoe(ack), Err(DriverError::Malformed(m)) if m.contains("No matching data")));
    assert!(matches!(parse_entsoe(&doc("")), Err(DriverError::Malformed(m)) if m.contains("no price periods")));
    assert!(parse_entsoe("<a><b></a>").is_err());
    assert!(parse_entsoe(&doc(&period("yesterday", "PT15M", &[(1, 1.0)]))).is_err());
    assert!(parse_entsoe("<a>&bogus;</a>").is_err());
}

/// At 13:47 on 2026-11-07 the feed gives HA's numbers: offtake 0.37692, injection 0.2099, the
/// raw slot 229.49 → 0.2295, both days (192 slots, tomorrow published) and the price's position
/// in today+tomorrow's range, 18.6 % (`sensor.stroomprijs_positie` at that moment).
#[tokio::test]
async fn prices_now_from_energy_charts() {
    let mut rig = Rig::new(None).await;
    assert_eq!(rig.feed.name(), "prices");
    let p = rig.prices().await.unwrap();
    assert_eq!(p.afname_now, Some(0.37692));
    assert_eq!(p.backup_now, Some(0.37692));
    assert_eq!(p.injectie_now, Some(0.2099));
    assert_eq!(p.epex_now, Some(0.2295));
    assert_eq!(p.afname_pct_in_range, Some(18.6));
    assert_eq!(p.afname_slots.len(), 192);
    assert_eq!(p.afname_slots[0], PriceSlot { start: at(7, 0, 0), price: 0.41417 });
    assert!(p.afname_has_tomorrow && p.injectie_has_tomorrow);
    assert_eq!(rig.feed.source(), Some(PriceSource::EnergyCharts));
    assert_eq!(rig.feed.unavailable(), Reading::Prices(Prices::default()));
}

/// Before tomorrow is published the range is today's only: 8.5 %, which is what HA's `entsoe`
/// sensor showed at 13:47 (it had not fetched tomorrow yet).
#[tokio::test]
async fn range_is_today_only_before_publication() {
    let mut rig = Rig::new(None).await;
    rig.sim.remove_day(day(8));
    let p = rig.prices().await.unwrap();
    assert_eq!(p.afname_pct_in_range, Some(8.5));
    assert!(!p.afname_has_tomorrow);
    assert_eq!(p.afname_slots.len(), 96);
}

/// A flat day sits at 100 % of its (empty) range, as the `entsoe` integration answers.
#[tokio::test]
async fn flat_prices_are_100_percent() {
    let mut rig = Rig::new(None).await;
    rig.sim.remove_day(day(8));
    rig.sim.set_day(day(7), vec![100.0; 96]);
    assert_eq!(rig.prices().await.unwrap().afname_pct_in_range, Some(100.0));
}

/// When energy-charts fails, ENTSO-E supplies the same prices; without a token the failure
/// stands.
#[tokio::test]
async fn falls_back_to_entsoe() {
    let mut rig = Rig::new(Some(TOKEN)).await;
    rig.sim.set_energy_charts_status(503);
    let p = rig.prices().await.unwrap();
    assert_eq!(p.afname_now, Some(0.37692));
    assert_eq!(p.afname_slots.len(), 192);
    assert_eq!(rig.feed.source(), Some(PriceSource::Entsoe));
    assert_eq!(rig.sim.hits("entsoe"), 1);
    let mut rig = Rig::new(None).await;
    rig.sim.set_energy_charts_status(503);
    assert!(matches!(rig.prices().await, Err(DriverError::Status(503))));
}

/// An energy-charts answer without today's prices (only tomorrow) also sends the feed to
/// ENTSO-E; with no data anywhere ENTSO-E's acknowledgement is the error, and without a token
/// the energy-charts 404 is.
#[tokio::test]
async fn energy_charts_without_today_falls_back() {
    let mut rig = Rig::new(Some(TOKEN)).await;
    rig.set_now(at(12, 10, 0));
    rig.sim.set_day(day(13), vec![60.0; 96]);
    assert!(matches!(rig.prices().await, Err(DriverError::Malformed(m)) if m.contains("no price slot")));
    assert_eq!(rig.sim.hits("entsoe"), 1);
    rig.sim.remove_day(day(13));
    assert!(matches!(rig.prices().await, Err(DriverError::Malformed(m)) if m.contains("No matching data")));
    let mut rig = Rig::new(None).await;
    rig.set_now(at(12, 10, 0));
    assert!(matches!(rig.prices().await, Err(DriverError::Status(404))));
    rig.sim.set_day(day(13), vec![60.0; 96]);
    assert!(matches!(rig.prices().await, Err(DriverError::Malformed(m)) if m.contains("energy-charts: no prices")));
}

/// A wrong ENTSO-E token is `Unauthorized`; an ENTSO-E outage is its status.
#[tokio::test]
async fn entsoe_auth_and_outage() {
    let mut rig = Rig::new(Some("wrong")).await;
    rig.sim.set_energy_charts_status(500);
    assert!(matches!(rig.prices().await, Err(DriverError::Unauthorized)));
    let mut rig = Rig::new(Some(TOKEN)).await;
    rig.sim.set_energy_charts_status(500);
    rig.sim.set_entsoe_status(503);
    assert!(matches!(rig.prices().await, Err(DriverError::Status(503))));
}

/// Fetch policy: one fetch serves the hour; after an hour, on a new day, every 15 minutes after
/// 13:00 while tomorrow is missing, and on `RefreshPrices` it fetches again.
#[tokio::test]
async fn fetch_policy() {
    let mut rig = Rig::new(None).await;
    rig.set_now(at(7, 10, 0));
    rig.prices().await.unwrap();
    rig.set_now(at(7, 10, 59));
    rig.prices().await.unwrap();
    assert_eq!(rig.sim.hits("energy-charts"), 1);
    rig.set_now(at(7, 11, 0));
    rig.prices().await.unwrap();
    assert_eq!(rig.sim.hits("energy-charts"), 2);
    rig.sim.remove_day(day(8));
    rig.set_now(at(7, 13, 0));
    rig.prices().await.unwrap();
    rig.set_now(at(7, 13, 14));
    rig.prices().await.unwrap();
    assert_eq!(rig.sim.hits("energy-charts"), 3);
    rig.set_now(at(7, 13, 15));
    rig.prices().await.unwrap();
    assert_eq!(rig.sim.hits("energy-charts"), 4);
    assert_eq!(rig.feed.apply(&Command::RefreshPrices).await.unwrap(), Applied::Done);
    assert_eq!(rig.sim.hits("energy-charts"), 5);
    rig.sim.set_day(day(8), vec![80.0; 96]);
    rig.set_now(at(8, 0, 5));
    assert!(rig.prices().await.unwrap().afname_slots.iter().all(|s| s.start >= at(8, 0, 0)));
    assert_eq!(rig.sim.hits("energy-charts"), 6);
    assert_eq!(rig.feed.apply(&Command::CableStop).await.unwrap(), Applied::NotMine);
}

/// A failed re-fetch keeps serving the cache while it has a slot for now; once it has none the
/// read fails. A failed `RefreshPrices` reports the error.
#[tokio::test]
async fn failed_refetch_keeps_cache() {
    let mut rig = Rig::new(None).await;
    rig.prices().await.unwrap();
    rig.sim.set_energy_charts_status(500);
    rig.set_now(at(7, 15, 0));
    assert_eq!(rig.prices().await.unwrap().afname_slots.len(), 192);
    assert!(rig.feed.apply(&Command::RefreshPrices).await.is_err());
    rig.set_now(at(9, 1, 0));
    assert!(matches!(rig.prices().await, Err(DriverError::Status(500))));
}

/// The cache knows no "now" outside its slots, and nothing before the first fetch.
#[tokio::test]
async fn no_slot_for_now() {
    let rig = Rig::new(None).await;
    assert!(matches!(rig.feed.prices_at(at(7, 12, 0)), Err(DriverError::NotConnected)));
    let mut rig = Rig::new(None).await;
    rig.prices().await.unwrap();
    assert!(matches!(rig.feed.prices_at(at(10, 12, 0)), Err(DriverError::Malformed(_))));
}

/// The simulator answers malformed queries with 400, like any HTTP API would.
#[tokio::test]
async fn simulator_rejects_bad_queries() {
    let sim = PriceSim::start(TOKEN).await;
    let c = reqwest::Client::new();
    let s = c.get(format!("{}/price", sim.energy_charts_url())).send().await.unwrap().status();
    assert_eq!(s, 400);
    let s = c.get(sim.entsoe_url()).query(&[("securityToken", TOKEN)]).send().await.unwrap().status();
    assert_eq!(s, 400);
}

/// Production endpoints and tariffs.
#[test]
fn belgium_defaults() {
    let c = PricesConfig::belgium(None);
    assert_eq!(c.area, "10YBE----------2");
    assert_eq!((c.offtake, c.injection), (Tariff::OFFTAKE, Tariff::INJECTION));
    assert!((c.clock)().year() >= 2026);
}

use chrono::Datelike;
