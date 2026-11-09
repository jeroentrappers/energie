//! Day-ahead prices: energy-charts primary, ENTSO-E fallback, with the tariffs of
//! a Python price script and the statistics of the HA `entsoe` integration.
//!
//! * energy-charts: `GET /price?bzn=BE&start=YYYY-MM-DD&end=YYYY-MM-DD` →
//!   `{"unix_seconds": [..], "price": [..], "unit": "EUR / MWh"}`, 15-minute slots. `end` is
//!   inclusive, so `start=today&end=tomorrow` returns both days once tomorrow is published.
//! * ENTSO-E: `GET /api?documentType=A44&in_Domain=10YBE----------2&out_Domain=..&periodStart=..&periodEnd=..&securityToken=..`
//!   → `Publication_MarketDocument` XML, `curveType A03` (a missing position repeats the
//!   previous price), or an `Acknowledgement_MarketDocument` when there is no data.
//!
//! Tariff: `round((raw / 1000 × slope + offset) × (1 + VAT), 5)` — offtake slope 1.02, offset
//! 0.1215031, VAT 6 %; injection slope 0.98, offset −0.015, VAT 0 (both parsed by
//! `price_backup.py` from the `entsoe` config entries' `modifyer` templates).
//!
//! `afname_pct_in_range` is `sensor.jeroen_current_percentage_in_electricity_price_range` of the
//! HA `entsoe` integration (`calculation_mode: publish`): `round((now − min) / (max − min) × 100, 1)`
//! over every slot from today 00:00 on (today, plus tomorrow once published), 100 when all
//! prices are equal. `price_backup.py`'s `sensor.stroomprijs_positie` uses the same window
//! (since 2026-11-07 explicitly: its `prices_today` keeps only the day's own 96 rows and
//! laagste/hoogste/positie are taken over today + tomorrow) but answers 0 for a flat day. No
//! `price_backup.py` attribute is mirrored here: `afname_slots` follows the `entsoe` sensor's
//! `prices` (today + tomorrow).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, TimeZone, Utc};
use energie_core::command::Command;
use energie_core::time::{TZ, Ts};
use energie_core::world::{PriceSlot, Prices};
use quick_xml::events::Event;
use serde::Deserialize;

use crate::driver::{Actuator, Applied, Reading, Source};
use crate::error::{DriverError, Result};

/// Python's `round(x, n)`: correctly rounded, ties to even on the exact binary value.
pub fn py_round(x: f64, n: usize) -> f64 {
    format!("{x:.n$}").parse().expect("formatted float")
}

/// `(raw / 1000 × slope + offset) × (1 + vat)`, EUR/kWh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tariff {
    pub slope: f64,
    pub offset: f64,
    pub vat: f64,
}

impl Tariff {
    /// Ecopower offtake: `current_price * 1.02 + 0.1215031`, VAT 6 %.
    pub const OFFTAKE: Tariff = Tariff { slope: 1.02, offset: 0.1215031, vat: 0.06 };
    /// Injection: `current_price * 0.98 - 0.015`, no VAT.
    pub const INJECTION: Tariff = Tariff { slope: 0.98, offset: -0.015, vat: 0.0 };

    /// Price for a raw EUR/MWh value, rounded to 5 decimals.
    pub fn apply(&self, eur_mwh: f64) -> f64 {
        py_round((eur_mwh / 1000.0 * self.slope + self.offset) * (1.0 + self.vat), 5)
    }
}

/// `entsoe_qh`: raw EUR/MWh → EUR/kWh, `Decimal.quantize(0.0001, ROUND_HALF_UP)`.
pub fn epex_kwh(eur_mwh: f64) -> f64 {
    let hundredths = (eur_mwh * 100.0).round() as i64; // the feed has 2 decimals
    let q = (hundredths.abs() + 5) / 10 * hundredths.signum();
    q as f64 / 10_000.0
}

/// One raw 15-minute slot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawSlot {
    pub start: DateTime<Utc>,
    pub eur_mwh: f64,
}

#[derive(Deserialize)]
struct EnergyCharts {
    unix_seconds: Vec<i64>,
    price: Vec<Option<f64>>,
}

/// Parse an energy-charts `/price` body. Slots with a `null` price are skipped.
pub fn parse_energy_charts(body: &str) -> Result<Vec<RawSlot>> {
    let ec: EnergyCharts = serde_json::from_str(body)?;
    if ec.unix_seconds.len() != ec.price.len() {
        return Err(DriverError::Malformed("energy-charts: unix_seconds and price differ in length".into()));
    }
    Ok(ec
        .unix_seconds
        .iter()
        .zip(&ec.price)
        .filter_map(|(&t, p)| Some(RawSlot { start: Utc.timestamp_opt(t, 0).single()?, eur_mwh: (*p)? }))
        .collect())
}

#[derive(Default)]
struct Period {
    start: Option<DateTime<Utc>>,
    minutes: Option<i64>,
    points: Vec<(i64, f64)>,
}

impl Period {
    /// `process_points`: fill from the first to the last position, repeating the previous price
    /// for a missing position (A03). Hourly periods are split into four quarters.
    fn slots(&self) -> Vec<RawSlot> {
        let (Some(start), Some(minutes)) = (self.start, self.minutes) else { return Vec::new() };
        let mut points = self.points.clone();
        points.sort_by_key(|p| p.0);
        let (Some(first), Some(last)) = (points.first().map(|p| p.0), points.last().map(|p| p.0)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut it = points.iter().peekable();
        let mut price = 0.0;
        for pos in first..=last {
            if let Some(&&(_, p)) = it.peek().filter(|p| p.0 == pos) {
                price = p;
                it.next();
            }
            let at = start + ChronoDuration::minutes((pos - 1) * minutes);
            for q in 0..minutes / 15 {
                out.push(RawSlot { start: at + ChronoDuration::minutes(q * 15), eur_mwh: price });
            }
        }
        out
    }
}

fn parse_time(s: &str) -> Result<DateTime<Utc>> {
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%MZ")
        .map_err(|e| DriverError::Malformed(format!("ENTSO-E time {s:?}: {e}")))?;
    Ok(Utc.from_utc_datetime(&naive))
}

/// Parse an ENTSO-E A44 document. Periods with a resolution other than PT15M/PT60M are skipped,
/// as are periods starting where an earlier one did (`We found a duplicate period`).
pub fn parse_entsoe(xml: &str) -> Result<Vec<RawSlot>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut path: Vec<String> = Vec::new();
    let mut period = Period::default();
    let mut point: (Option<i64>, Option<f64>) = (None, None);
    let mut starts = Vec::new();
    let mut out = Vec::new();
    let mut reason = String::new();
    loop {
        match reader.read_event().map_err(|e| DriverError::Malformed(format!("ENTSO-E XML: {e}")))? {
            Event::Start(e) => path.push(String::from_utf8_lossy(e.local_name().as_ref()).into_owned()),
            Event::End(e) => {
                match e.local_name().as_ref() {
                    b"Point" => {
                        if let (Some(pos), Some(price)) = std::mem::take(&mut point) {
                            period.points.push((pos, price));
                        }
                    }
                    b"Period" => {
                        let p = std::mem::take(&mut period);
                        if p.start.is_some_and(|s| !starts.contains(&s)) {
                            starts.push(p.start.expect("start"));
                            out.extend(p.slots());
                        }
                    }
                    _ => {}
                }
                path.pop();
            }
            Event::Text(t) => {
                let text = t.decode().map_err(|e| DriverError::Malformed(e.to_string()))?.into_owned();
                let tail: Vec<&str> = path.iter().rev().take(3).map(String::as_str).collect();
                match tail.as_slice() {
                    ["start", "timeInterval", "Period", ..] => period.start = Some(parse_time(&text)?),
                    ["resolution", "Period", ..] => {
                        period.minutes = match text.as_str() {
                            "PT15M" => Some(15),
                            "PT60M" | "PT1H" => Some(60),
                            _ => None,
                        }
                    }
                    ["position", "Point", ..] => point.0 = text.parse().ok(),
                    ["price.amount", "Point", ..] => point.1 = text.parse().ok(),
                    ["text", "Reason", ..] => reason = text,
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if out.is_empty() {
        let why = if reason.is_empty() { "no price periods".to_string() } else { reason };
        return Err(DriverError::Malformed(format!("ENTSO-E: {why}")));
    }
    out.sort_by_key(|s| s.start);
    Ok(out)
}

/// Clock the feed reads "now" from (tests use a fixed one).
pub type Clock = Arc<dyn Fn() -> Ts + Send + Sync>;

/// Where to fetch from and how often.
#[derive(Clone)]
pub struct PricesConfig {
    /// `https://api.energy-charts.info`
    pub energy_charts_url: String,
    /// `https://web-api.tp.entsoe.eu/api`
    pub entsoe_url: String,
    /// ENTSO-E security token (config entry `entsoe_qh`; never commit it). `None` disables the
    /// fallback.
    pub entsoe_token: Option<String>,
    pub bzn: String,
    pub area: String,
    pub offtake: Tariff,
    pub injection: Tariff,
    pub timeout: Duration,
    /// Re-fetch at least this often.
    pub refresh_every: ChronoDuration,
    /// While tomorrow is missing after 13:00, re-fetch this often.
    pub tomorrow_retry: ChronoDuration,
    pub clock: Clock,
}

impl PricesConfig {
    /// The production endpoints for Belgium.
    pub fn belgium(entsoe_token: Option<String>) -> Self {
        PricesConfig {
            energy_charts_url: "https://api.energy-charts.info".into(),
            entsoe_url: "https://web-api.tp.entsoe.eu/api".into(),
            entsoe_token,
            bzn: "BE".into(),
            area: "10YBE----------2".into(),
            offtake: Tariff::OFFTAKE,
            injection: Tariff::INJECTION,
            timeout: Duration::from_secs(25),
            refresh_every: ChronoDuration::hours(1),
            tomorrow_retry: ChronoDuration::minutes(15),
            clock: Arc::new(|| Utc::now().with_timezone(&TZ)),
        }
    }
}

/// Which feed the cached prices came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    EnergyCharts,
    Entsoe,
}

struct Cache {
    day: NaiveDate,
    fetched: Ts,
    slots: Vec<RawSlot>,
    source: PriceSource,
}

/// The price feed: fetches, caches and turns raw slots into [`Prices`].
pub struct PriceFeed {
    cfg: PricesConfig,
    client: reqwest::Client,
    cache: Option<Cache>,
    refresh: bool,
}

fn midnight(day: NaiveDate) -> Ts {
    let naive = day.and_hms_opt(0, 0, 0).expect("midnight");
    TZ.from_local_datetime(&naive).earliest().expect("midnight exists in Brussels")
}

impl PriceFeed {
    pub fn new(cfg: PricesConfig) -> Result<Self> {
        let client = reqwest::Client::builder().use_rustls_tls().timeout(cfg.timeout).build()?;
        Ok(PriceFeed { cfg, client, cache: None, refresh: false })
    }

    /// The feed the cached prices came from.
    pub fn source(&self) -> Option<PriceSource> {
        self.cache.as_ref().map(|c| c.source)
    }

    async fn get(&self, req: reqwest::RequestBuilder) -> Result<String> {
        let resp = req.send().await?;
        match resp.status().as_u16() {
            200 => Ok(resp.text().await?),
            401 => Err(DriverError::Unauthorized),
            s => Err(DriverError::Status(s)),
        }
    }

    async fn energy_charts(&self, today: NaiveDate) -> Result<Vec<RawSlot>> {
        let url = format!("{}/price", self.cfg.energy_charts_url);
        let q = [
            ("bzn", self.cfg.bzn.clone()),
            ("start", today.to_string()),
            ("end", today.succ_opt().expect("day").to_string()),
        ];
        parse_energy_charts(&self.get(self.client.get(url).query(&q)).await?)
    }

    async fn entsoe(&self, today: NaiveDate, token: &str) -> Result<Vec<RawSlot>> {
        let fmt = |t: Ts| t.with_timezone(&Utc).format("%Y%m%d%H%M").to_string();
        let start = midnight(today);
        let end = midnight(today + ChronoDuration::days(2));
        let q = [
            ("securityToken", token.to_string()),
            ("documentType", "A44".into()),
            ("contract_MarketAgreement.type", "A01".into()),
            ("in_Domain", self.cfg.area.clone()),
            ("out_Domain", self.cfg.area.clone()),
            ("periodStart", fmt(start)),
            ("periodEnd", fmt(end)),
        ];
        parse_entsoe(&self.get(self.client.get(&self.cfg.entsoe_url).query(&q)).await?)
    }

    /// Fetch today (and tomorrow if published): energy-charts first, ENTSO-E when that fails or
    /// has nothing for today.
    pub async fn fetch(&mut self) -> Result<PriceSource> {
        let now = (self.cfg.clock)();
        let today = now.date_naive();
        let covers_today = |s: &[RawSlot]| s.iter().any(|r| r.start.with_timezone(&TZ).date_naive() == today);
        let (slots, source) = match self.energy_charts(today).await {
            Ok(s) if covers_today(&s) => (s, PriceSource::EnergyCharts),
            primary => {
                let token = self.cfg.entsoe_token.clone();
                let Some(token) = token else {
                    return Err(primary
                        .err()
                        .unwrap_or(DriverError::Malformed("energy-charts: no prices for today".into())));
                };
                (self.entsoe(today, &token).await?, PriceSource::Entsoe)
            }
        };
        self.cache = Some(Cache { day: today, fetched: now, slots, source });
        self.refresh = false;
        Ok(source)
    }

    fn needs_fetch(&self, now: Ts) -> bool {
        let Some(c) = &self.cache else { return true };
        let since = now - c.fetched;
        let tomorrow = now.date_naive().succ_opt().expect("day");
        let has_tomorrow = c.slots.iter().any(|r| r.start.with_timezone(&TZ).date_naive() == tomorrow);
        self.refresh
            || c.day != now.date_naive()
            || self.prices_at(now).is_err()
            || since >= self.cfg.refresh_every
            || (!has_tomorrow
                && now.time() >= chrono::NaiveTime::from_hms_opt(13, 0, 0).expect("13:00")
                && since >= self.cfg.tomorrow_retry)
    }

    /// The [`Prices`] at `now` from the cache.
    pub fn prices_at(&self, now: Ts) -> Result<Prices> {
        let c = self.cache.as_ref().ok_or(DriverError::NotConnected)?;
        let from = midnight(now.date_naive());
        let tomorrow = now.date_naive().succ_opt().expect("day");
        let slots: Vec<&RawSlot> = c.slots.iter().filter(|r| r.start.with_timezone(&TZ) >= from).collect();
        let cur = slots
            .iter()
            .find(|r| r.start <= now && now < r.start + ChronoDuration::minutes(15))
            .ok_or_else(|| DriverError::Malformed(format!("no price slot for {now}")))?;
        let afname: Vec<f64> = slots.iter().map(|r| self.cfg.offtake.apply(r.eur_mwh)).collect();
        let now_price = self.cfg.offtake.apply(cur.eur_mwh);
        let (lo, hi) = afname.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &p| (lo.min(p), hi.max(p)));
        let pct = if hi - lo == 0.0 { 100.0 } else { py_round((now_price - lo) / (hi - lo) * 100.0, 1) };
        let has_tomorrow = slots.iter().any(|r| r.start.with_timezone(&TZ).date_naive() == tomorrow);
        Ok(Prices {
            afname_now: Some(now_price),
            afname_pct_in_range: Some(pct),
            afname_slots: slots
                .iter()
                .zip(&afname)
                .map(|(r, &price)| PriceSlot { start: r.start.with_timezone(&TZ), price })
                .collect(),
            afname_has_tomorrow: has_tomorrow,
            injectie_now: Some(self.cfg.injection.apply(cur.eur_mwh)),
            injectie_has_tomorrow: has_tomorrow,
            epex_now: Some(epex_kwh(cur.eur_mwh)),
            backup_now: Some(now_price),
        })
    }
}

#[async_trait]
impl Source for PriceFeed {
    fn name(&self) -> &'static str {
        "prices"
    }

    /// Fetch when due (new day, hourly, every 15 min after 13:00 while tomorrow is missing, or
    /// after `RefreshPrices`), then compute the prices for now. A failed fetch keeps using the
    /// cache as long as it has a slot for now, like the HA integrations keep their data.
    async fn read(&mut self) -> Result<Reading> {
        let now = (self.cfg.clock)();
        if self.needs_fetch(now)
            && let Err(e) = self.fetch().await
            && self.prices_at(now).is_err()
        {
            return Err(e);
        }
        Ok(Reading::Prices(self.prices_at(now)?))
    }

    fn unavailable(&self) -> Reading {
        Reading::Prices(Prices::default())
    }
}

#[async_trait]
impl Actuator for PriceFeed {
    /// `RefreshPrices` fetches now (HA: `homeassistant.update_entity` on the price feeds).
    async fn apply(&mut self, cmd: &Command) -> Result<Applied> {
        if *cmd != Command::RefreshPrices {
            return Ok(Applied::NotMine);
        }
        self.refresh = true;
        self.fetch().await?;
        Ok(Applied::Done)
    }
}

#[cfg(test)]
mod tests;
