//! Seed an empty history store from HA's recorder (`[history] seed_from_ha`), so the charts are
//! not empty on the first day. Read-only: one WebSocket session that sends three
//! `recorder/statistics_during_period` requests and closes.
//!
//! | Tier | HA statistics | Range |
//! | --- | --- | --- |
//! | day | `day` (long-term) | `day_days` back, up to last midnight (complete days only) |
//! | quarter | `hour` (long-term), at the hour's start | `quarter_days` back, up to where the 5-minute range starts |
//! | minute | `5minute` (short-term, HA keeps ~10 days) | `minute_days` (at most 10) back, up to now |
//!
//! Only series with an HA entity that has statistics (a `state_class`) get rows. Counters
//! (`total_increasing`) have only `state`, which becomes all four values. The minute rows then
//! derive their quarter and day rows like live ones; a seeded day row wins over a derived one.

use std::time::Duration;

use energie_core::meters::Cycle;
use energie_core::time::Ts;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::SharedHistory;
use super::series::SERIES;
use super::tier::{Agg, Res};
use crate::error::{Error, Result};
use crate::ha::ws::{self, ClientConfig};

/// HA purges short-term statistics after 10 days by default.
const SHORT_TERM_DAYS: i64 = 10;

/// A statistics request for one tier.
pub struct Request {
    pub res: Res,
    pub period: &'static str,
    pub from: i64,
    pub to: i64,
    /// (store index, HA entity) of the series asked for.
    pub series: Vec<(usize, &'static str)>,
}

fn iso(t: i64) -> String {
    super::tier::ts(t).to_rfc3339()
}

impl Request {
    pub fn body(&self, id: u64) -> Value {
        let ids: Vec<&str> = self.series.iter().map(|(_, e)| *e).collect();
        json!({
            "id": id,
            "type": "recorder/statistics_during_period",
            "start_time": iso(self.from),
            "end_time": iso(self.to),
            "statistic_ids": ids,
            "period": self.period,
            "types": ["mean", "min", "max", "state"],
        })
    }
}

/// The three requests for `now`, given each tier's retention (s).
pub fn requests(now: Ts, minute_keep_s: i64, quarter_keep_s: i64, day_keep_s: i64) -> Vec<Request> {
    let n = now.timestamp();
    let pick = |f: fn(&super::series::Series) -> bool| -> Vec<(usize, &'static str)> {
        SERIES
            .iter()
            .enumerate()
            .filter(|(_, s)| s.read.is_some() && f(s))
            .filter_map(|(i, s)| s.ha.map(|e| (i, e)))
            .collect()
    };
    let minute_from = n - minute_keep_s.min(SHORT_TERM_DAYS * 86_400);
    let midnight = Cycle::Daily.period_start(now).timestamp();
    vec![
        Request { res: Res::Day, period: "day", from: n - day_keep_s, to: midnight, series: pick(|s| s.day) },
        Request {
            res: Res::Quarter,
            period: "hour",
            from: n - quarter_keep_s,
            to: Res::Quarter.bucket(minute_from),
            series: pick(|s| s.quarter),
        },
        Request { res: Res::Minute, period: "5minute", from: minute_from, to: n, series: pick(|_| true) },
    ]
}

/// A statistics row's start: milliseconds (HA ≥ 2023.3), seconds, or an ISO string.
fn start_of(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => {
            let x = n.as_f64()?;
            Some(if x > 1e11 { (x / 1000.0) as i64 } else { x as i64 })
        }
        Value::String(s) => crate::ha::entity::parse_time(s).map(|t| t.timestamp()),
        _ => None,
    }
}

/// The rows of one statistics result (`{"sensor.x": [{start, mean, min, max, state}, …]}`).
pub fn rows_from(result: &Value, req: &Request) -> Vec<(i64, Vec<(usize, Agg)>)> {
    let mut by_t: std::collections::BTreeMap<i64, Vec<(usize, Agg)>> = Default::default();
    for (idx, entity) in &req.series {
        for p in result[*entity].as_array().map(Vec::as_slice).unwrap_or_default() {
            let num = |k: &str| p[k].as_f64();
            let Some(t) = start_of(&p["start"]) else { continue };
            let Some(mean) = num("mean").or(num("state")) else { continue };
            let a = Agg {
                mean: mean as f32,
                min: num("min").unwrap_or(mean) as f32,
                max: num("max").unwrap_or(mean) as f32,
                last: num("state").unwrap_or(mean) as f32,
            };
            by_t.entry(t).or_default().push((*idx, a));
        }
    }
    by_t.into_iter().collect()
}

/// Run the requests against HA and import the rows. Returns the rows imported.
pub async fn seed(cfg: &ClientConfig, h: &SharedHistory, now: Ts) -> Result<usize> {
    let keep = |name: &str| {
        let h = h.lock().expect("history lock");
        h.tiers().into_iter().find(|t| t.name == name).map(|t| t.keep_s).expect("the store has every tier")
    };
    let reqs = requests(now, keep("minute"), keep("quarter"), keep("day"));
    let mut ws = ws::authenticate(cfg).await?;
    let mut total = 0;
    for (k, req) in reqs.iter().enumerate() {
        if req.series.is_empty() || req.to <= req.from {
            continue;
        }
        let id = k as u64 + 1;
        ws::send(&mut ws, req.body(id)).await?;
        let reply = loop {
            let m = ws::next_json(&mut ws).await?;
            if m["type"] == "result" && m["id"] == id {
                break m;
            }
        };
        if reply["success"] != true {
            return Err(Error::new(format!("recorder statistics ({}): {}", req.period, reply["error"])));
        }
        let rows = rows_from(&reply["result"], req);
        total += rows.len();
        h.lock().expect("history lock").import(req.res, rows, now.timestamp());
    }
    Ok(total)
}

/// [`seed`] in the background, logging the outcome; gives up after `limit` or at shutdown.
pub async fn run(cfg: ClientConfig, h: SharedHistory, now: Ts, limit: Duration, mut stop: watch::Receiver<bool>) {
    tokio::select! {
        r = tokio::time::timeout(limit, seed(&cfg, &h, now)) => match r {
            Ok(Ok(n)) => tracing::info!(rows = n, "history seeded from the home assistant recorder"),
            Ok(Err(e)) => tracing::warn!(error = %e, "seeding history from home assistant failed"),
            Err(_) => tracing::warn!("seeding history from home assistant timed out"),
        },
        _ = stop.wait_for(|s| *s) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::ws::tests::{ServerWs, push, recv};
    use crate::history::tier::Pick;
    use crate::history::{History, Retention, series};
    use crate::plug::tests::t0;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    type Reply = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

    /// A fake HA that authenticates `good-token` and answers each request with `reply`; it
    /// records the requests it saw.
    async fn fake_recorder(reply: Reply) -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws: ServerWs = tokio_tungstenite::accept_async(tcp).await.unwrap();
                push(&mut ws, json!({"type": "auth_required"})).await;
                let auth = recv(&mut ws).await.unwrap();
                if auth["access_token"] != "good-token" {
                    push(&mut ws, json!({"type": "auth_invalid", "message": "nee"})).await;
                    continue;
                }
                push(&mut ws, json!({"type": "auth_ok"})).await;
                while let Some(req) = recv(&mut ws).await {
                    log.lock().unwrap().push(req.clone());
                    push(&mut ws, json!({"id": 999, "type": "pong"})).await;
                    push(&mut ws, reply(&req)).await;
                }
            }
        });
        (url, seen)
    }

    fn store() -> SharedHistory {
        let ret = Retention { dir: None, minute_s: 14 * 86_400, quarter_s: 430 * 86_400, day_s: 1100 * 86_400 };
        Arc::new(Mutex::new(History::open(series::meta(), &ret, t0().timestamp()).unwrap()))
    }

    fn cfg(url: String, token: &str) -> ClientConfig {
        crate::ha::ws::tests::fast(url, token)
    }

    /// The requests are read-only statistics queries over the tiers' ranges.
    #[test]
    fn requests_cover_the_tiers() {
        let r = requests(t0(), 14 * 86_400, 430 * 86_400, 1100 * 86_400);
        assert_eq!(r.iter().map(|r| r.period).collect::<Vec<_>>(), ["day", "hour", "5minute"]);
        let n = t0().timestamp();
        assert_eq!(r[2].from, n - 10 * 86_400, "HA keeps 10 days of 5-minute statistics");
        assert_eq!(r[1].to, Res::Quarter.bucket(r[2].from));
        assert_eq!(r[0].to, Cycle::Daily.period_start(t0()).timestamp());
        assert!(r[2].series.iter().any(|(_, e)| *e == "sensor.p1_meter_vermogen"));
        assert!(r[0].series.iter().any(|(_, e)| *e == "sensor.pv_energie_dag"));
        assert!(r[0].series.iter().all(|(i, _)| SERIES[*i].day));
        let b = r[2].body(7);
        assert_eq!(b["type"], "recorder/statistics_during_period");
        assert_eq!(b["id"], 7);
        assert_eq!(b["start_time"], "2026-10-28T13:46:00+01:00");
        let short = requests(t0(), 3600, 86_400, 86_400);
        assert_eq!(short[2].from, n - 3600);
    }

    /// Rows: ms, s or ISO starts; mean/min/max/state, or a counter's state alone.
    #[test]
    fn rows_from_statistics() {
        let req = Request { res: Res::Minute, period: "5minute", from: 0, to: 1, series: vec![(0, "a"), (3, "b")] };
        let result = json!({
            "a": [{"start": 1_762_500_000_000.0_f64, "mean": 2.0, "min": 1.0, "max": 3.0, "state": 2.5},
                  {"start": "2025-11-07T08:20:00+00:00", "mean": 1.0},
                  {"start": true, "mean": 1.0},
                  {"start": 1_762_500_300, "max": 1.0}],
            "b": [{"start": 1_762_500_000, "state": 9.0}],
        });
        let rows = rows_from(&result, &req);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1_762_500_000);
        assert_eq!(rows[0].1, vec![(0, Agg { mean: 2.0, min: 1.0, max: 3.0, last: 2.5 }), (3, Agg::one(9.0))]);
        assert_eq!(rows[1], (1_762_503_600, vec![(0, Agg::one(1.0))]));
        assert!(rows_from(&json!({}), &req).is_empty());
    }

    /// A full seed against a fake recorder fills the three tiers; failures are errors.
    #[tokio::test]
    async fn seeds_the_store_from_a_fake_recorder() {
        let n = t0().timestamp();
        let reply: Reply = Arc::new(move |req: &Value| {
            let id = req["id"].clone();
            let rows =
                |t: i64| json!([{"start": t * 1000, "mean": 1000.0, "min": 500.0, "max": 2000.0, "state": 1500.0}]);
            let result = match req["period"].as_str().unwrap() {
                "day" => json!({"sensor.pv_energie_dag": [{"start": (n - 3 * 86_400) * 1000, "state": 12.5}]}),
                "hour" => json!({"sensor.p1_meter_vermogen": rows(n - 20 * 86_400)}),
                _ => {
                    json!({"sensor.p1_meter_vermogen": rows(n - 3600), "sensor.solis_s6_eh1p_battery_soc_2": rows(n - 3600)})
                }
            };
            json!({"id": id, "type": "result", "success": true, "result": result})
        });
        let (url, seen) = fake_recorder(reply).await;
        let h = store();
        let rows = seed(&cfg(url.clone(), "good-token"), &h, t0()).await.unwrap();
        assert_eq!(rows, 3);
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        assert!(seen.iter().all(|r| r["type"] == "recorder/statistics_during_period"));
        {
            let h = h.lock().unwrap();
            let day = h.query(&[("pv_dag_kwh".into(), Pick::Last)], n - 4 * 86_400, n, 86_400).unwrap();
            assert!(day.series[0].v.contains(&Some(12.5)));
            let p1 = h.query(&[("p1_w".into(), Pick::Max)], n - 21 * 86_400, n, 86_400).unwrap();
            assert_eq!(p1.series[0].v.iter().filter(|v| **v == Some(2000.0)).count(), 2);
            assert!(h.tiers()[2].rows >= 2, "the hour row and the quarter derived from the 5-minute row");
        }
        // A quarter retention shorter than the 5-minute range skips the hourly request.
        let ret = Retention { dir: None, minute_s: 14 * 86_400, quarter_s: 3600, day_s: 86_400 };
        let short: SharedHistory = Arc::new(Mutex::new(History::open(series::meta(), &ret, n).unwrap()));
        assert_eq!(seed(&cfg(url.clone(), "good-token"), &short, t0()).await.unwrap(), 2);

        // A request HA refuses and a bad token are errors.
        let refuse: Reply = Arc::new(
            |req: &Value| json!({"id": req["id"], "type": "result", "success": false, "error": {"code": "unknown_command"}}),
        );
        let (url2, _) = fake_recorder(refuse).await;
        assert!(seed(&cfg(url2, "good-token"), &store(), t0()).await.unwrap_err().0.contains("unknown_command"));
        assert!(seed(&cfg(url, "bad"), &store(), t0()).await.is_err());
    }

    /// The background runner logs and ends: on success, failure, timeout or shutdown.
    #[tokio::test]
    async fn runner_always_ends() {
        let ok: Reply =
            Arc::new(|req: &Value| json!({"id": req["id"], "type": "result", "success": true, "result": {}}));
        let (url, _) = fake_recorder(ok).await;
        let (_tx, stop) = watch::channel(false);
        run(cfg(url.clone(), "good-token"), store(), t0(), Duration::from_secs(5), stop.clone()).await;
        run(cfg(url.clone(), "bad"), store(), t0(), Duration::from_secs(5), stop.clone()).await;
        let silent: Reply = Arc::new(|_: &Value| json!({"type": "event"}));
        let (url3, _) = fake_recorder(silent).await;
        run(cfg(url3.clone(), "good-token"), store(), t0(), Duration::from_millis(50), stop).await;
        let (tx, stop) = watch::channel(false);
        tx.send(true).unwrap();
        run(cfg(url3, "good-token"), store(), t0(), Duration::from_secs(5), stop).await;
    }
}
