//! Off-device history in VictoriaMetrics.
//!
//! Every closed minute goes out as InfluxDB line protocol, so any database that accepts it can
//! receive the data; one measurement `energie` tagged `site`, four fields per series
//! (`<id>_mean`, `_min`, `_max`, `_last`), which VictoriaMetrics stores as `energie_<id>_mean` and
//! so on. Sending happens in its own task from a bounded queue in the store: the control loop
//! never waits for the network, and an outage only costs what no longer fits in the queue.
//!
//! The dashboard reads the same data back for what the local store doesn't have. Local values
//! always win, and a slow or absent database only leaves those buckets empty.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::join_all;
use serde::Deserialize;
use tokio::sync::watch;

use super::tier::{Agg, Pick};
use super::{QueryOut, SharedHistory};

const SEND_EVERY: Duration = Duration::from_secs(20);
const SEND_TIMEOUT: Duration = Duration::from_secs(15);
const READ_TIMEOUT: Duration = Duration::from_secs(4);
/// Rows in one request.
const BATCH: usize = 500;
/// The finest data sent is one minute, so finer charts stay local.
const MIN_READ_STEP: i64 = 60;

/// One closed minute: its start and the series that had samples.
pub type Point = (i64, Vec<(String, Agg)>);

#[derive(Debug, Clone)]
pub enum Auth {
    None,
    Basic(String, String),
    Bearer(String),
}

/// A connection to the database.
#[derive(Debug)]
pub struct Vm {
    client: reqwest::Client,
    url: String,
    site: String,
    auth: Auth,
    read: bool,
}

/// A field or metric name: what the store's ids can contain, and nothing else.
fn name(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

/// `points` as InfluxDB line protocol with second timestamps.
pub fn line_protocol(site: &str, points: &[Point]) -> String {
    let mut out = String::new();
    for (t, fields) in points {
        let mut body = String::new();
        for (id, a) in fields {
            let id = name(id);
            for (suffix, v) in [("mean", a.mean), ("min", a.min), ("max", a.max), ("last", a.last)] {
                if v.is_finite() {
                    let sep = if body.is_empty() { "" } else { "," };
                    body.push_str(&format!("{sep}{id}_{suffix}={v}"));
                }
            }
        }
        if !body.is_empty() {
            out.push_str(&format!("energie,site={site} {body} {t}\n"));
        }
    }
    out
}

/// The query for one chart series over buckets of `step` seconds.
fn query(site: &str, id: &str, pick: Pick, step: i64) -> String {
    let (func, field) = match pick {
        Pick::Mean => ("avg", "mean"),
        Pick::Min => ("min", "min"),
        Pick::Max => ("max", "max"),
        Pick::Last => ("last", "last"),
    };
    format!("{func}_over_time(energie_{}_{field}{{site=\"{site}\"}}[{step}s])", name(id))
}

#[derive(Deserialize)]
struct Range {
    data: RangeData,
}

#[derive(Deserialize)]
struct RangeData {
    result: Vec<RangeSeries>,
}

#[derive(Deserialize)]
struct RangeSeries {
    values: Vec<(f64, String)>,
}

impl Vm {
    pub fn new(url: &str, site: &str, auth: Auth, read: bool) -> crate::error::Result<Vm> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| crate::error::Error::new(format!("victoriametrics client: {e}")))?;
        Ok(Vm { client, url: url.trim_end_matches('/').to_string(), site: site.to_string(), auth, read })
    }

    fn authed(&self, r: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::None => r,
            Auth::Basic(u, p) => r.basic_auth(u, Some(p)),
            Auth::Bearer(t) => r.bearer_auth(t),
        }
    }

    /// Send `points`; any answer but success is an error, so they stay queued.
    pub async fn send(&self, points: &[Point]) -> Result<(), String> {
        let body = line_protocol(&self.site, points);
        if body.is_empty() {
            return Ok(());
        }
        let url = format!("{}/write?precision=s", self.url);
        let resp = self.authed(self.client.post(url).timeout(SEND_TIMEOUT).body(body)).send().await;
        // The request's own error can carry the URL; the status or kind is enough here.
        let resp =
            resp.map_err(|e| if e.is_timeout() { "timeout".to_string() } else { "not reachable".to_string() })?;
        if resp.status().is_success() { Ok(()) } else { Err(format!("HTTP {}", resp.status().as_u16())) }
    }

    /// One series over the given buckets: `(bucket start, value)`.
    async fn range(&self, id: &str, pick: Pick, first: i64, last: i64, step: i64) -> Result<Vec<(i64, f64)>, String> {
        // VictoriaMetrics looks back over (T - window, T]. Bucket t holds [t, t + step), so ask at
        // t + step - 1.
        let shift = step - 1;
        let params = [
            ("query", query(&self.site, id, pick, step)),
            ("start", (first + shift).to_string()),
            ("end", (last + shift).to_string()),
            ("step", format!("{step}s")),
            ("nocache", "1".to_string()),
        ];
        let url = format!("{}/api/v1/query_range", self.url);
        let resp = self.authed(self.client.get(url).timeout(READ_TIMEOUT).query(&params)).send().await;
        let resp =
            resp.map_err(|e| if e.is_timeout() { "timeout".to_string() } else { "not reachable".to_string() })?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        let parsed: Range = resp.json().await.map_err(|_| "unexpected answer".to_string())?;
        let Some(series) = parsed.data.result.into_iter().next() else { return Ok(vec![]) };
        Ok(series
            .values
            .into_iter()
            .filter_map(|(ts, v)| Some((ts as i64 - shift, v.parse::<f64>().ok().filter(|v| v.is_finite())?)))
            .collect())
    }

    /// Fill the empty buckets of `out` up to `now`. Whatever fails stays empty.
    pub async fn fill(&self, out: &mut QueryOut, picks: &[Pick], now: i64) {
        let (Some(&first), true) = (out.t.first(), self.read && out.step >= MIN_READ_STEP) else { return };
        let last = out.t.iter().copied().filter(|t| *t <= now).max().unwrap_or(first);
        if last < first {
            return;
        }
        let wanted: Vec<usize> = (0..out.series.len())
            .filter(|&s| out.t.iter().zip(&out.series[s].v).any(|(t, v)| *t <= now && v.is_none()))
            .collect();
        let step = out.step;
        let asks = wanted.iter().map(|&s| self.range(&out.series[s].id, picks[s], first, last, step));
        let answers = join_all(asks).await;
        for (s, answer) in wanted.into_iter().zip(answers) {
            let points = match answer {
                Ok(p) => p,
                Err(e) => {
                    tracing::debug!(error = %e, "victoriametrics: reading history failed");
                    continue;
                }
            };
            for (ts, v) in points {
                // Bucket starts aren't evenly spaced across a daylight-saving change.
                let Some(i) = out.t.iter().position(|t| (t - ts).abs() * 2 < step) else { continue };
                let slot = &mut out.series[s].v[i];
                if slot.is_none() {
                    *slot = Some((v * 1000.0).round() / 1000.0);
                }
            }
        }
    }
}

/// Send one batch; `false` when there is nothing more or it failed.
async fn send_batch(vm: &Vm, hist: &SharedHistory) -> bool {
    let batch = hist.lock().expect("history lock").export_batch(BATCH);
    let Some(last) = batch.last().map(|(t, _)| *t) else { return false };
    let full = batch.len() == BATCH;
    let result = vm.send(&batch).await;
    let mut h = hist.lock().expect("history lock");
    match result {
        Ok(()) => {
            h.export_done(last);
            full
        }
        Err(e) => {
            h.export_failed(e);
            false
        }
    }
}

/// Send what the store queues until shutdown, and once more then.
pub async fn run(vm: Arc<Vm>, hist: SharedHistory, mut stop: watch::Receiver<bool>) {
    let mut every = tokio::time::interval(SEND_EVERY);
    loop {
        tokio::select! {
            _ = every.tick() => {}
            _ = stop.wait_for(|s| *s) => break,
        }
        while send_batch(&vm, &hist).await {}
    }
    hist.lock().expect("history lock").flush();
    while send_batch(&vm, &hist).await {}
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::{get, post};
    use std::sync::Mutex;

    use super::*;
    use crate::history::SeriesOut;

    fn agg(v: f32) -> Agg {
        Agg { mean: v, min: v - 1.0, max: v + 1.0, last: v }
    }

    #[test]
    fn minutes_become_line_protocol() {
        let points = vec![
            (60, vec![("p1_w".to_string(), agg(5.5))]),
            (120, vec![("odd.id".to_string(), Agg { mean: 1.0, min: f32::NAN, max: f32::NAN, last: 2.0 })]),
            (180, vec![("gone".to_string(), Agg { mean: f32::NAN, min: f32::NAN, max: f32::NAN, last: f32::NAN })]),
        ];
        assert_eq!(
            line_protocol("home", &points),
            "energie,site=home p1_w_mean=5.5,p1_w_min=4.5,p1_w_max=6.5,p1_w_last=5.5 60\n\
             energie,site=home odd_id_mean=1,odd_id_last=2 120\n"
        );
    }

    #[test]
    fn each_pick_reads_its_own_field() {
        assert_eq!(query("home", "p1_w", Pick::Max, 900), "max_over_time(energie_p1_w_max{site=\"home\"}[900s])");
        assert_eq!(query("a", "soc", Pick::Last, 60), "last_over_time(energie_soc_last{site=\"a\"}[60s])");
    }

    async fn serve(app: Router) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        url
    }

    fn out(step: i64, t: Vec<i64>, v: Vec<Option<f64>>) -> QueryOut {
        let series = vec![SeriesOut { id: "p1_w".into(), label: "".into(), unit: "W".into(), agg: "mean", v }];
        QueryOut { from: t[0], to: *t.last().unwrap(), step, t, series }
    }

    /// Empty buckets are filled, local values are kept, the future isn't asked for.
    #[tokio::test]
    async fn the_database_fills_only_what_is_missing() {
        let asked = Arc::new(Mutex::new(String::new()));
        let seen = asked.clone();
        let app = Router::new().route(
            "/api/v1/query_range",
            get(move |q: axum::extract::RawQuery| async move {
                *seen.lock().unwrap() = q.0.unwrap();
                axum::Json(serde_json::json!({"data": {"result": [{"values": [[1799, "1.5"], [2399, "9"]]}]}}))
            }),
        );
        let vm = Vm::new(&serve(app).await, "home", Auth::None, true).unwrap();
        let mut o = out(600, vec![600, 1200, 1800, 2400], vec![None, None, Some(7.0), None]);
        vm.fill(&mut o, &[Pick::Mean], 1900).await;
        assert_eq!(o.series[0].v, vec![None, Some(1.5), Some(7.0), None]);
        let q = asked.lock().unwrap().clone();
        assert!(q.contains("start=1199") && q.contains("end=2399") && q.contains("nocache=1"), "{q}");
    }

    /// A database that is down or answers nonsense leaves the local result alone.
    #[tokio::test]
    async fn a_broken_database_changes_nothing() {
        let app = Router::new().route("/api/v1/query_range", get(|| async { "not json" }));
        let vm = Vm::new(&serve(app).await, "home", Auth::None, true).unwrap();
        let mut o = out(600, vec![600], vec![None]);
        vm.fill(&mut o, &[Pick::Mean], 9000).await;
        assert_eq!(o.series[0].v, vec![None]);
        let gone = Vm::new("http://127.0.0.1:1", "home", Auth::None, true).unwrap();
        gone.fill(&mut o, &[Pick::Mean], 9000).await;
        assert_eq!(o.series[0].v, vec![None]);
    }

    /// Charts finer than the exported minutes, and `read = false`, stay local.
    #[tokio::test]
    async fn fine_charts_never_ask() {
        let app = Router::new().route("/api/v1/query_range", get(|| async { panic!("asked") as &str }));
        let url = serve(app).await;
        let vm = Vm::new(&url, "home", Auth::None, true).unwrap();
        vm.fill(&mut out(30, vec![30], vec![None]), &[Pick::Mean], 900).await;
        let off = Vm::new(&url, "home", Auth::None, false).unwrap();
        off.fill(&mut out(600, vec![600], vec![None]), &[Pick::Mean], 900).await;
    }

    /// The credentials travel with both kinds of request, and a refusal is an error.
    #[tokio::test]
    async fn sends_with_credentials() {
        let got = Arc::new(Mutex::new(vec![]));
        let seen = got.clone();
        let app = Router::new().route(
            "/write",
            post(move |h: axum::http::HeaderMap, q: axum::extract::RawQuery, body: String| async move {
                seen.lock().unwrap().push((h["authorization"].to_str().unwrap().to_string(), q.0.unwrap(), body));
                axum::http::StatusCode::NO_CONTENT
            }),
        );
        let url = serve(app).await;
        let points = vec![(60, vec![("x".to_string(), agg(1.0))])];
        Vm::new(&url, "s", Auth::Bearer("tok".into()), true).unwrap().send(&points).await.unwrap();
        let got = got.lock().unwrap().clone();
        assert_eq!((got[0].0.as_str(), got[0].1.as_str()), ("Bearer tok", "precision=s"));
        assert!(got[0].2.starts_with("energie,site=s x_mean=1"));
        let refused = Router::new().route("/write", post(|| async { axum::http::StatusCode::UNAUTHORIZED }));
        let e = Vm::new(&serve(refused).await, "s", Auth::None, true).unwrap().send(&points).await.unwrap_err();
        assert_eq!(e, "HTTP 401");
    }
}
