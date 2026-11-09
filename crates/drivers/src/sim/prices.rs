//! Day-ahead price servers: energy-charts' `/price` JSON and ENTSO-E's A44 XML, both answering
//! from the same per-day raw prices (EUR/MWh, one per quarter hour from local midnight).
//!
//! ENTSO-E answers in `curveType A03`: a position whose price equals the previous one is left
//! out, as the live API does. A bad `securityToken` gets 401; a period without data gets the
//! `Acknowledgement_MarketDocument` with status 200, like the live API.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use energie_core::time::TZ;

use super::http::{HttpServer, serve};

/// The live energy-charts answer for BE, `start=2026-11-07&end=2026-11-08` (both days).
pub const ENERGY_CHARTS_FIXTURE: &str = include_str!("../../fixtures/energy_charts_be_2026-11-07.json");
/// The live ENTSO-E A44 answer for BE, 2026-11-07 and 2026-11-08 (one position compressed out).
pub const ENTSOE_FIXTURE: &str = include_str!("../../fixtures/entsoe_a44_be_2026-11-07.xml");

struct Inner {
    days: BTreeMap<NaiveDate, Vec<f64>>,
    token: String,
    energy_charts_status: u16,
    entsoe_status: u16,
    hits: HashMap<&'static str, usize>,
}

type Shared = Arc<Mutex<Inner>>;

/// A running price server.
pub struct PriceSim {
    state: Shared,
    server: HttpServer,
}

fn midnight_utc(day: NaiveDate) -> DateTime<Utc> {
    TZ.from_local_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight"))
        .earliest()
        .expect("midnight")
        .with_timezone(&Utc)
}

/// The two live days of the fixture, split by local date.
fn fixture_days() -> BTreeMap<NaiveDate, Vec<f64>> {
    let v: serde_json::Value = serde_json::from_str(ENERGY_CHARTS_FIXTURE).expect("fixture");
    let mut days: BTreeMap<NaiveDate, Vec<f64>> = BTreeMap::new();
    for (t, p) in v["unix_seconds"].as_array().expect("times").iter().zip(v["price"].as_array().expect("prices")) {
        let day = Utc.timestamp_opt(t.as_i64().expect("t"), 0).single().expect("ts").with_timezone(&TZ).date_naive();
        days.entry(day).or_default().push(p.as_f64().expect("price"));
    }
    days
}

fn slots(days: &BTreeMap<NaiveDate, Vec<f64>>, from: NaiveDate, to: NaiveDate) -> Vec<(DateTime<Utc>, f64)> {
    days.range(from..=to)
        .flat_map(|(&d, prices)| {
            let start = midnight_utc(d);
            prices.iter().enumerate().map(move |(i, &p)| (start + Duration::minutes(15 * i as i64), p))
        })
        .collect()
}

async fn energy_charts(State(st): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mut s = st.lock().unwrap();
    *s.hits.entry("energy-charts").or_default() += 1;
    if s.energy_charts_status != 200 {
        return StatusCode::from_u16(s.energy_charts_status).expect("status").into_response();
    }
    let day = |k: &str| q.get(k).and_then(|v| NaiveDate::parse_from_str(v, "%Y-%m-%d").ok());
    let (Some(from), Some(to)) = (day("start"), day("end")) else { return StatusCode::BAD_REQUEST.into_response() };
    let rows = slots(&s.days, from, to);
    if rows.is_empty() {
        return (StatusCode::NOT_FOUND, axum::Json(serde_json::json!({"detail": "No data for request"})))
            .into_response();
    }
    let body = serde_json::json!({
        "license_info": "simulated",
        "unix_seconds": rows.iter().map(|r| r.0.timestamp()).collect::<Vec<_>>(),
        "price": rows.iter().map(|r| r.1).collect::<Vec<_>>(),
        "unit": "EUR / MWh",
        "deprecated": false,
    });
    axum::Json(body).into_response()
}

fn a44(days: &BTreeMap<NaiveDate, Vec<f64>>, from: DateTime<Utc>, to: DateTime<Utc>) -> Option<String> {
    let fmt = |t: DateTime<Utc>| t.format("%Y-%m-%dT%H:%MZ").to_string();
    let mut periods = String::new();
    for (&d, prices) in days {
        let start = midnight_utc(d);
        if start < from || start >= to {
            continue;
        }
        let end = start + Duration::minutes(15 * prices.len() as i64);
        let mut points = String::new();
        for (i, p) in prices.iter().enumerate() {
            if i == 0 || prices[i - 1] != *p {
                points += &format!("<Point><position>{}</position><price.amount>{p}</price.amount></Point>", i + 1);
            }
        }
        periods += &format!(
            "<Period><timeInterval><start>{}</start><end>{}</end></timeInterval><resolution>PT15M</resolution>{points}</Period>",
            fmt(start),
            fmt(end)
        );
    }
    (!periods.is_empty()).then(|| {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?><Publication_MarketDocument xmlns="urn:iec62325.351:tc57wg16:451-3:publicationdocument:7:3"><type>A44</type><period.timeInterval><start>{}</start><end>{}</end></period.timeInterval><TimeSeries><mRID>1</mRID><curveType>A03</curveType>{periods}</TimeSeries></Publication_MarketDocument>"#,
            fmt(from),
            fmt(to)
        )
    })
}

const NO_DATA: &str = r#"<?xml version="1.0" encoding="utf-8"?><Acknowledgement_MarketDocument xmlns="urn:iec62325.351:tc57wg16:451-1:acknowledgementdocument:7:0"><Reason><code>999</code><text>No matching data found for Data item Energy Prices [12.1.D]</text></Reason></Acknowledgement_MarketDocument>"#;

async fn entsoe(State(st): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    let mut s = st.lock().unwrap();
    *s.hits.entry("entsoe").or_default() += 1;
    if q.get("securityToken") != Some(&s.token) {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    if s.entsoe_status != 200 {
        return StatusCode::from_u16(s.entsoe_status).expect("status").into_response();
    }
    let t = |k: &str| {
        q.get(k).and_then(|v| NaiveDateTime::parse_from_str(v, "%Y%m%d%H%M").ok()).map(|n| Utc.from_utc_datetime(&n))
    };
    let (Some(from), Some(to)) = (t("periodStart"), t("periodEnd")) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    a44(&s.days, from, to).unwrap_or_else(|| NO_DATA.to_string()).into_response()
}

impl PriceSim {
    /// Start with the live days 2026-11-07 and 2026-11-08; ENTSO-E accepts `token`.
    pub async fn start(token: &str) -> PriceSim {
        let state = Arc::new(Mutex::new(Inner {
            days: fixture_days(),
            token: token.into(),
            energy_charts_status: 200,
            entsoe_status: 200,
            hits: HashMap::new(),
        }));
        let router =
            Router::new().route("/price", get(energy_charts)).route("/api", get(entsoe)).with_state(state.clone());
        PriceSim { state, server: serve(router).await }
    }

    /// `http://127.0.0.1:<port>` (energy-charts base).
    pub fn energy_charts_url(&self) -> String {
        format!("http://{}", self.server.addr)
    }

    /// `http://127.0.0.1:<port>/api` (ENTSO-E endpoint).
    pub fn entsoe_url(&self) -> String {
        format!("http://{}/api", self.server.addr)
    }

    /// Set a day's quarter-hour prices (EUR/MWh).
    pub fn set_day(&self, day: NaiveDate, prices: Vec<f64>) {
        self.state.lock().unwrap().days.insert(day, prices);
    }

    /// Withdraw a day (e.g. tomorrow before publication).
    pub fn remove_day(&self, day: NaiveDate) {
        self.state.lock().unwrap().days.remove(&day);
    }

    /// Make energy-charts answer with this status (200 = normal).
    pub fn set_energy_charts_status(&self, status: u16) {
        self.state.lock().unwrap().energy_charts_status = status;
    }

    /// Make ENTSO-E answer with this status (200 = normal), e.g. its 2026-08 run of 503s.
    pub fn set_entsoe_status(&self, status: u16) {
        self.state.lock().unwrap().entsoe_status = status;
    }

    /// Requests served per feed (`"energy-charts"`, `"entsoe"`).
    pub fn hits(&self, feed: &str) -> usize {
        self.state.lock().unwrap().hits.get(feed).copied().unwrap_or(0)
    }
}
