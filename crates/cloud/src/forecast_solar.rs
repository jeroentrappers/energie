//! forecast.solar public API (`forecast_solar` integration, `sensor.power_production_now`,
//! `sensor.energy_production_*`, `sensor.energy_*_hour`). Display only.
//!
//! `GET https://api.forecast.solar/estimate/{lat}/{lon}/{declination}/{azimuth}/{kwp}?time=utc&damping=0`
//! (with an API key: `/{key}/estimate/...`). No authentication on the public tier; the limit is
//! 12 calls per hour per IP, so the client polls hourly like HA does without a key. HA stores
//! the plane as declination / azimuth (0..360, 180 = south) / modules power (W) and converts to
//! the API's azimuth (-180..180, 0 = south) and kWp; [`ForecastSolarConfig::from_ha_plane`]
//! does the same. The home's entry: 30° / 180° / 8800 W at the HA location.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Days, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde::Deserialize;

use crate::error::{CloudError, Result};
use crate::http;
use crate::poll::{Gate, Poller};
use crate::source::{CloudReading, CloudSource, EntityDescriptor, EntityKind, EntityState, num};
use crate::store::Secret;

pub const BASE_URL: &str = "https://api.forecast.solar";

/// One plane at one place, in the API's conventions.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ForecastSolarConfig {
    pub latitude: f64,
    pub longitude: f64,
    /// Tilt, degrees from horizontal.
    pub declination: f64,
    /// API convention: -180..180, 0 = south, -90 = east.
    pub azimuth: f64,
    pub kwp: f64,
    #[serde(default)]
    pub damping: f64,
    /// A personal API key (none on the public tier).
    #[serde(default)]
    pub api_key: Option<Secret>,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Seconds between fetches (HA: 3600 without a key).
    #[serde(default = "default_poll")]
    pub poll_interval_s: u64,
}

fn default_base_url() -> String {
    BASE_URL.to_string()
}

fn default_poll() -> u64 {
    3600
}

impl ForecastSolarConfig {
    /// A plane as HA stores it: azimuth 0..360 (180 = south) and modules power in W.
    pub fn from_ha_plane(latitude: f64, longitude: f64, declination: f64, ha_azimuth: f64, modules_w: f64) -> Self {
        ForecastSolarConfig {
            latitude,
            longitude,
            declination,
            azimuth: ha_azimuth - 180.0,
            kwp: modules_w / 1000.0,
            damping: 0.0,
            api_key: None,
            base_url: default_base_url(),
            poll_interval_s: default_poll(),
        }
    }

    /// The defaults for keys left out: one plane at the configured site, 30° / south / 8.8 kWp.
    pub fn home() -> Self {
        let (latitude, longitude) = energie_core::sun::site();
        Self::from_ha_plane(latitude, longitude, 30.0, 180.0, 8800.0)
    }

    /// The estimate URL with its query.
    pub fn estimate_url(&self) -> Result<reqwest::Url> {
        let key = self.api_key.as_ref().map(|k| format!("/{}", k.expose())).unwrap_or_default();
        let base = self.base_url.trim_end_matches('/');
        let path = format!(
            "{base}{key}/estimate/{}/{}/{}/{}/{}",
            self.latitude, self.longitude, self.declination, self.azimuth, self.kwp
        );
        let mut url = http::parse_url(&path)?;
        url.query_pairs_mut().append_pair("time", "utc").append_pair("damping", &self.damping.to_string());
        Ok(url)
    }
}

/// The parsed `estimate` response.
#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    /// Power at each instant, W.
    pub watts: Vec<(DateTime<Utc>, f64)>,
    /// Energy per period ending at each instant, Wh.
    pub wh_period: Vec<(DateTime<Utc>, f64)>,
    /// Energy per day, Wh.
    pub wh_days: Vec<(NaiveDate, f64)>,
    /// `message.info.timezone`: the API's notion of "today".
    pub timezone: Tz,
    /// `message.ratelimit.limit`.
    pub rate_limit: Option<u32>,
}

#[derive(Deserialize)]
struct RawEstimate {
    result: RawResult,
    message: RawMessage,
}

#[derive(Deserialize)]
struct RawResult {
    watts: serde_json::Map<String, serde_json::Value>,
    watt_hours_period: serde_json::Map<String, serde_json::Value>,
    watt_hours_day: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct RawMessage {
    info: RawInfo,
    #[serde(default)]
    ratelimit: Option<RawRatelimit>,
}

#[derive(Deserialize)]
struct RawInfo {
    timezone: String,
}

#[derive(Deserialize)]
struct RawRatelimit {
    limit: u32,
}

fn series(map: &serde_json::Map<String, serde_json::Value>) -> Result<Vec<(DateTime<Utc>, f64)>> {
    let mut out = map
        .iter()
        .map(|(k, v)| {
            let at = DateTime::parse_from_rfc3339(k).map_err(|e| CloudError::Malformed(format!("time {k:?}: {e}")))?;
            let v = v.as_f64().ok_or_else(|| CloudError::Malformed(format!("value at {k}")))?;
            Ok((at.with_timezone(&Utc), v))
        })
        .collect::<Result<Vec<_>>>()?;
    out.sort_by_key(|(t, _)| *t);
    Ok(out)
}

/// Parse an `estimate` body (requested with `time=utc`).
pub fn parse_estimate(body: &str) -> Result<Estimate> {
    let raw: RawEstimate = serde_json::from_str(body)?;
    let mut wh_days = raw
        .result
        .watt_hours_day
        .iter()
        .map(|(k, v)| {
            let day = k.get(..10).and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            match (day, v.as_f64()) {
                (Some(d), Some(v)) => Ok((d, v)),
                _ => Err(CloudError::Malformed(format!("day {k:?}"))),
            }
        })
        .collect::<Result<Vec<_>>>()?;
    wh_days.sort_by_key(|(d, _)| *d);
    Ok(Estimate {
        watts: series(&raw.result.watts)?,
        wh_period: series(&raw.result.watt_hours_period)?,
        wh_days,
        timezone: raw.message.info.timezone.parse().unwrap_or(Tz::UTC),
        rate_limit: raw.message.ratelimit.map(|r| r.limit),
    })
}

/// What the HA sensors show, at one instant, plus the series for charts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SolarForecast {
    /// `sensor.power_production_now`, W.
    pub power_now_w: Option<f64>,
    /// `sensor.energy_production_today`, kWh.
    pub energy_today_kwh: Option<f64>,
    /// `sensor.energy_production_today_remaining`, kWh.
    pub energy_today_remaining_kwh: Option<f64>,
    /// `sensor.energy_production_tomorrow`, kWh.
    pub energy_tomorrow_kwh: Option<f64>,
    /// `sensor.energy_current_hour`, kWh.
    pub energy_current_hour_kwh: Option<f64>,
    /// `sensor.energy_next_hour`, kWh.
    pub energy_next_hour_kwh: Option<f64>,
    /// Power series, W.
    pub watts: Vec<(DateTime<Utc>, f64)>,
    /// Energy per period, Wh.
    pub wh_period: Vec<(DateTime<Utc>, f64)>,
}

/// Sum of the values at instants in `[from, until)`.
fn sum_between(data: &[(DateTime<Utc>, f64)], from: DateTime<Utc>, until: DateTime<Utc>) -> f64 {
    data.iter().filter(|(t, _)| *t >= from && *t < until).map(|(_, v)| v).sum()
}

impl Estimate {
    /// The last value at or before `at`; 0 before the first and after the last point (the
    /// `forecast_solar` library's `power_production_at_time`).
    pub fn power_at(&self, at: DateTime<Utc>) -> f64 {
        let mut value = None;
        for (t, v) in &self.watts {
            if *t > at {
                return value.unwrap_or(0.0);
            }
            value = Some(*v);
        }
        0.0
    }

    /// Energy of the day `day` (in the API's timezone), Wh; 0 when absent.
    pub fn day_wh(&self, day: NaiveDate) -> f64 {
        self.wh_days.iter().find(|(d, _)| *d == day).map_or(0.0, |(_, v)| *v)
    }

    /// The sensor values at `now`.
    pub fn at(&self, now: DateTime<Utc>) -> SolarForecast {
        let local = now.with_timezone(&self.timezone);
        let today = local.date_naive();
        let tomorrow = today.checked_add_days(Days::new(1)).expect("date in range");
        let midnight = self.timezone.from_local_datetime(&tomorrow.and_hms_opt(0, 0, 0).expect("midnight")).earliest();
        let midnight = midnight.map_or(now + chrono::Duration::days(1), |m| m.with_timezone(&Utc));
        let hour = now.with_minute(0).and_then(|t| t.with_second(0)).and_then(|t| t.with_nanosecond(0)).expect("hour");
        let next_from = hour + chrono::Duration::minutes(59) + chrono::Duration::seconds(59);
        let next_from = next_from + chrono::Duration::microseconds(999);
        let kwh = |wh: f64| Some(wh / 1000.0);
        SolarForecast {
            power_now_w: Some(self.power_at(now)),
            energy_today_kwh: kwh(self.day_wh(today)),
            energy_today_remaining_kwh: kwh(sum_between(&self.wh_period, now, midnight)),
            energy_tomorrow_kwh: kwh(self.day_wh(tomorrow)),
            energy_current_hour_kwh: kwh(sum_between(&self.wh_period, hour, hour + chrono::Duration::hours(1))),
            energy_next_hour_kwh: kwh(sum_between(&self.wh_period, next_from, next_from + chrono::Duration::hours(1))),
            watts: self.watts.clone(),
            wh_period: self.wh_period.clone(),
        }
    }
}

const POWER_NOW: &str = "sensor.power_production_now";
const TODAY: &str = "sensor.energy_production_today";
const REMAINING: &str = "sensor.energy_production_today_remaining";
const TOMORROW: &str = "sensor.energy_production_tomorrow";
const CURRENT_HOUR: &str = "sensor.energy_current_hour";
const NEXT_HOUR: &str = "sensor.energy_next_hour";

impl SolarForecast {
    pub fn entity_states(&self) -> Vec<EntityState> {
        vec![
            num(POWER_NOW, self.power_now_w),
            num(TODAY, self.energy_today_kwh),
            num(REMAINING, self.energy_today_remaining_kwh),
            num(TOMORROW, self.energy_tomorrow_kwh),
            num(CURRENT_HOUR, self.energy_current_hour_kwh),
            num(NEXT_HOUR, self.energy_next_hour_kwh),
        ]
    }
}

/// The error message of an API error body (`{"message": "..."}` or `{"message": {"text": ...}}`).
fn api_message(body: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    match &v["message"] {
        serde_json::Value::String(s) => s.clone(),
        m if m["text"].is_string() => m["text"].as_str().unwrap_or_default().to_string(),
        _ => http::snippet(body, 200),
    }
}

/// Polling client for one plane.
pub struct ForecastSolar {
    cfg: ForecastSolarConfig,
    client: reqwest::Client,
    poller: Poller<Estimate>,
}

impl ForecastSolar {
    pub fn new(cfg: ForecastSolarConfig) -> Result<Self> {
        cfg.estimate_url()?;
        let interval = Duration::from_secs(cfg.poll_interval_s);
        Ok(ForecastSolar {
            client: http::client(http::TIMEOUT, None, true)?,
            poller: Poller::new(interval, Duration::from_secs(300), Duration::from_secs(3600)),
            cfg,
        })
    }

    /// One `GET estimate`. A 429 is returned as [`CloudError::RateLimited`] with the
    /// `X-Ratelimit-Retry-At` instant, if the API sent one.
    pub async fn estimate(&self) -> std::result::Result<Estimate, (CloudError, Option<DateTime<Utc>>)> {
        let fetch = async {
            let resp = self.client.get(self.cfg.estimate_url()?).send().await?;
            let status = resp.status().as_u16();
            let retry_at = resp
                .headers()
                .get("X-Ratelimit-Retry-At")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                .map(|t| t.with_timezone(&Utc));
            let json = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("application/json"));
            let body = resp.text().await?;
            Ok::<_, CloudError>((status, retry_at, json, body))
        };
        let (status, retry_at, json, body) = fetch.await.map_err(|e| (e, None))?;
        let err = |e| Err((e, None));
        match status {
            200 if json => parse_estimate(&body).map_err(|e| (e, None)),
            200 => err(CloudError::Malformed("estimate is not JSON".into())),
            401 | 403 => err(CloudError::Unauthorized(api_message(&body))),
            400 | 404 | 422 => err(CloudError::Config(api_message(&body))),
            429 => Err((CloudError::RateLimited(api_message(&body)), retry_at)),
            s => err(CloudError::Status(s)),
        }
    }

    /// The forecast at `now`, fetching at most once per poll interval.
    pub async fn forecast_at(&mut self, now: DateTime<Utc>) -> Result<SolarForecast> {
        match self.poller.gate(Instant::now()) {
            Gate::Cached(e) => return Ok(e.at(now)),
            Gate::Wait(d) => return Err(CloudError::Backoff(d)),
            Gate::Fetch => {}
        }
        match self.estimate().await {
            Ok(e) => self.poller.record(Instant::now(), Ok(e)).map(|e| e.at(now)),
            Err((e, Some(at))) => {
                let wait = (at - Utc::now()).to_std().unwrap_or_default().max(Duration::from_secs(60));
                Err(self.poller.fail_for(Instant::now(), wait, e))
            }
            Err((e @ CloudError::RateLimited(_), None)) => {
                Err(self.poller.fail_for(Instant::now(), Duration::from_secs(3600), e))
            }
            Err((e, None)) => self.poller.record(Instant::now(), Err(e)).map(|e| e.at(now)),
        }
    }
}

#[async_trait]
impl CloudSource for ForecastSolar {
    fn name(&self) -> &'static str {
        "forecast_solar"
    }

    fn entities(&self) -> Vec<EntityDescriptor> {
        let kwh = |id| EntityDescriptor::new(id, EntityKind::Sensor, Some("kWh"));
        vec![
            EntityDescriptor::new(POWER_NOW, EntityKind::Sensor, Some("W")),
            kwh(TODAY),
            kwh(REMAINING),
            kwh(TOMORROW),
            kwh(CURRENT_HOUR),
            kwh(NEXT_HOUR),
        ]
    }

    async fn read(&mut self) -> Result<CloudReading> {
        Ok(CloudReading::Forecast(self.forecast_at(Utc::now()).await?))
    }

    fn unavailable(&self) -> CloudReading {
        CloudReading::Forecast(SolarForecast::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::{Log, Mock, Reply, Script};

    const FIXTURE: &str = include_str!("../fixtures/forecast_solar_estimate.json");

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// The HA plane converts to the API's conventions and builds the documented URL.
    #[test]
    fn home_plane_and_url() {
        let cfg = ForecastSolarConfig::home();
        assert_eq!((cfg.declination, cfg.azimuth, cfg.kwp), (30.0, 0.0, 8.8));
        assert_eq!(
            cfg.estimate_url().unwrap().as_str(),
            "https://api.forecast.solar/estimate/50.8467/4.3525/30/0/8.8?time=utc&damping=0"
        );
        let keyed = ForecastSolarConfig { api_key: Some(Secret::new("k3y")), damping: 0.25, ..cfg.clone() };
        assert!(keyed.estimate_url().unwrap().as_str().starts_with("https://api.forecast.solar/k3y/estimate/"));
        assert!(keyed.estimate_url().unwrap().as_str().ends_with("damping=0.25"));
        let toml_like: ForecastSolarConfig =
            serde_json::from_str(r#"{"latitude":1,"longitude":2,"declination":3,"azimuth":-90,"kwp":4}"#).unwrap();
        assert_eq!((toml_like.base_url.as_str(), toml_like.poll_interval_s), (BASE_URL, 3600));
        let bad = ForecastSolarConfig { base_url: "nope".into(), ..cfg };
        assert!(matches!(ForecastSolar::new(bad), Err(CloudError::Config(_))));
    }

    /// The values worked out by hand from the fixture at 13:30 local (12:30 UTC).
    #[test]
    fn sensor_values_at_an_instant() {
        let e = parse_estimate(FIXTURE).unwrap();
        assert_eq!(e.timezone, chrono_tz::Europe::Brussels);
        assert_eq!(e.rate_limit, Some(12));
        let f = e.at(t("2026-11-07T12:30:00Z"));
        assert_eq!(f.power_now_w, Some(1156.0));
        assert_eq!(f.energy_today_kwh, Some(6.998));
        assert_eq!(f.energy_tomorrow_kwh, Some(7.232));
        // 13:00 1031 + 14:00 713 + 15:00 315 + 15:50 46
        assert_eq!(f.energy_today_remaining_kwh, Some(2.105));
        // [12:00, 13:00): the 12:00 period
        assert_eq!(f.energy_current_hour_kwh, Some(1.233));
        // [12:59:59.999, 13:59:59.999): the 13:00 period
        assert_eq!(f.energy_next_hour_kwh, Some(1.031));
        assert_eq!(f.watts.len(), 14);
        assert_eq!(
            f.entity_states()[0],
            ("sensor.power_production_now".into(), Some(crate::EntityValue::Number(1156.0)))
        );
    }

    /// Before the first and after the last point the power is 0; unknown days are 0 kWh.
    #[test]
    fn edges_are_zero() {
        let e = parse_estimate(FIXTURE).unwrap();
        assert_eq!(e.power_at(t("2026-11-07T05:00:00Z")), 0.0);
        assert_eq!(e.power_at(t("2026-11-08T16:00:00Z")), 0.0);
        let f = e.at(t("2026-11-09T12:00:00Z"));
        assert_eq!((f.energy_today_kwh, f.energy_tomorrow_kwh), (Some(0.0), Some(0.0)));
    }

    /// Broken bodies are parse errors; an unknown timezone falls back to UTC.
    #[test]
    fn malformed_estimates() {
        assert!(matches!(parse_estimate("{}"), Err(CloudError::Malformed(_))));
        let mut v: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        v["message"]["info"]["timezone"] = "Mars/Olympus".into();
        v["message"].as_object_mut().unwrap().remove("ratelimit");
        let e = parse_estimate(&v.to_string()).unwrap();
        assert_eq!((e.timezone, e.rate_limit), (Tz::UTC, None));
        let mut bad = v.clone();
        bad["result"]["watts"]["yesterday"] = 1.into();
        assert!(matches!(parse_estimate(&bad.to_string()), Err(CloudError::Malformed(_))));
        let mut bad = v.clone();
        bad["result"]["watts"]["2026-11-07T06:45:00+00:00"] = "x".into();
        assert!(matches!(parse_estimate(&bad.to_string()), Err(CloudError::Malformed(_))));
        let mut bad = v;
        bad["result"]["watt_hours_day"]["someday"] = 1.into();
        assert!(matches!(parse_estimate(&bad.to_string()), Err(CloudError::Malformed(_))));
    }

    #[test]
    fn api_messages() {
        assert_eq!(api_message(r#"{"message":"bad plane"}"#), "bad plane");
        assert_eq!(api_message(r#"{"message":{"text":"too many"}}"#), "too many");
        assert_eq!(api_message("oops"), "oops");
    }

    const KEY: &str = "GET /estimate/50.8467/4.3525/30/0/8.8";

    async fn serve() -> (Mock, Script, Log) {
        let (m, s, l) = Mock::scripted().await;
        s.set(KEY, Reply::json(200, FIXTURE));
        (m, s, l)
    }

    fn client(m: &Mock, poll: u64) -> ForecastSolar {
        ForecastSolar::new(ForecastSolarConfig {
            base_url: m.base.clone(),
            poll_interval_s: poll,
            ..ForecastSolarConfig::home()
        })
        .unwrap()
    }

    /// End to end: one GET per poll interval, the reading computed per read.
    #[tokio::test]
    async fn reads_and_caches() {
        let (m, _, log) = serve().await;
        let mut fs = client(&m, 3600);
        assert_eq!(fs.name(), "forecast_solar");
        assert_eq!(fs.entities().len(), 6);
        assert_eq!(fs.entities()[1].unit, Some("kWh"));
        let f = fs.forecast_at(t("2026-11-07T12:30:00Z")).await.unwrap();
        assert_eq!(f.power_now_w, Some(1156.0));
        let CloudReading::Forecast(_) = fs.read().await.unwrap() else { panic!() };
        assert_eq!(log.count("GET /estimate/"), 1);
        assert_eq!(log.paths()[0], format!("{KEY}?time=utc&damping=0"));
        assert_eq!(fs.unavailable(), CloudReading::Forecast(SolarForecast::default()));
    }

    /// Errors map to their kind; a 429 waits for the API's retry time, then an hour.
    #[tokio::test]
    async fn errors_and_rate_limits() {
        let (m, s, _) = serve().await;
        let now = t("2026-11-07T12:30:00Z");
        s.set(KEY, Reply::html("<html>"));
        assert!(matches!(client(&m, 0).forecast_at(now).await, Err(CloudError::Malformed(_))));
        s.set(KEY, Reply::json(401, r#"{"message":"key"}"#));
        assert!(matches!(client(&m, 0).forecast_at(now).await, Err(CloudError::Unauthorized(_))));
        s.set(KEY, Reply::json(422, r#"{"message":"plane"}"#));
        assert!(matches!(client(&m, 0).forecast_at(now).await, Err(CloudError::Config(_))));
        s.set(KEY, Reply::json(503, ""));
        let mut fs = client(&m, 0);
        assert!(matches!(fs.forecast_at(now).await, Err(CloudError::Status(503))));
        assert!(matches!(fs.forecast_at(now).await, Err(CloudError::Backoff(d)) if d <= Duration::from_secs(300)));
        s.set(KEY, Reply::json(429, r#"{"message":{"text":"limit"}}"#));
        let mut fs = client(&m, 0);
        assert!(matches!(fs.forecast_at(now).await, Err(CloudError::RateLimited(_))));
        assert!(matches!(fs.forecast_at(now).await, Err(CloudError::Backoff(d)) if d > Duration::from_secs(3000)));
        s.set(KEY, Reply::json(429, "{}").header("X-Ratelimit-Retry-At", "2099-01-01T00:00:00+00:00"));
        let mut fs = client(&m, 0);
        assert!(matches!(fs.forecast_at(now).await, Err(CloudError::RateLimited(_))));
        assert!(matches!(fs.forecast_at(now).await, Err(CloudError::Backoff(d)) if d > Duration::from_secs(86400)));
        drop(m);
        let (m, _, _) = serve().await;
        let mut fs = client(&m, 0);
        drop(m);
        assert!(matches!(fs.forecast_at(now).await, Err(CloudError::Http(_))));
    }
}
