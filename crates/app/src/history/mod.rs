//! The history store behind the dashboard charts: per-series aggregates at four resolutions.
//!
//! | Tier | Bucket | Kept | Where | Series |
//! | --- | --- | --- | --- | --- |
//! | recent | 10 s | 2 h | memory | all sampled series |
//! | minute | 1 min | `minute_days` (14) | `minute/<YYYY-MM-DD>.jsonl` | all sampled series |
//! | quarter | 15 min | `quarter_days` (430) | `quarter/<YYYY-MM>.jsonl` | those marked `quarter` |
//! | day | local day | `day_days` (1100) | `day/<YYYY>.jsonl` | those marked `day` |
//!
//! Every bucket holds mean, min, max and last of each series ([`tier::Agg`]). Samples feed the
//! recent and minute accumulators; a closed minute is appended to its segment (one small write a
//! minute). Quarter and day rows are *derived from the minute rows* whenever a quarter closes, and
//! at start for every completed bucket that has minute rows but no row of its own, so a restart
//! (or a crash) never loses a quarter or a day that the minute tier still has. See [`tier`] for the
//! file format.
//!
//! The store is keyed by series id strings ([`Meta::id`]); [`series`] is the one place that says
//! which series exist and how each is read from a tick. On top of those, `[history]
//! extra_series` globs pick entities of the registry ([`History::extra_values`]): each matching
//! numeric entity becomes a series under its entity id when it first has a value, kept in the
//! recent, minute and day tiers. A query ([`History::query`]) re-buckets to any step, using the
//! finest tier that has data for each part of the range.

pub mod seed;
pub mod series;
pub mod tier;

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use energie_drivers::entity::{Entity, Value};

use crate::entities::glob;
use crate::error::{Error, Result};
use tier::{Acc, Agg, Loaded, Merge, Pick, Res, Row, Tier};

pub type SharedHistory = Arc<Mutex<History>>;

/// At most this many buckets per query.
pub const MAX_BUCKETS: usize = 5000;
const RECENT_KEEP_S: i64 = 2 * 3600;

/// What a series is, for the store and the API.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Meta {
    pub id: String,
    pub label: String,
    pub unit: String,
    /// Decimals written to disk and returned by queries.
    pub decimals: u32,
    /// Sampled every tick into the recent and minute tiers (false: only set from outside, like a
    /// forecast).
    pub sampled: bool,
    /// Kept in the quarter tier.
    pub quarter: bool,
    /// Kept in the day tier.
    pub day: bool,
}

/// Retention, s, and where the segments go.
#[derive(Debug, Clone, PartialEq)]
pub struct Retention {
    pub dir: Option<PathBuf>,
    pub minute_s: i64,
    pub quarter_s: i64,
    pub day_s: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Stats {
    pub samples: u64,
    pub rows_written: u64,
    pub bytes_written: u64,
    pub write_errors: u64,
    pub last_error: Option<String>,
    pub corrupt_lines: usize,
    pub torn_tails: usize,
    pub files_deleted: usize,
    pub rows_loaded: usize,
    pub rows_imported: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TierInfo {
    pub name: &'static str,
    pub step_s: i64,
    pub keep_s: i64,
    pub series: usize,
    pub rows: usize,
    pub first: Option<i64>,
    pub last: Option<i64>,
}

/// One series of a query result: a value (or `null`) per bucket.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesOut {
    pub id: String,
    pub label: String,
    pub unit: String,
    pub agg: &'static str,
    pub v: Vec<Option<f64>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryOut {
    pub from: i64,
    pub to: i64,
    pub step: i64,
    /// Bucket starts, Unix seconds.
    pub t: Vec<i64>,
    pub series: Vec<SeriesOut>,
}

pub struct History {
    meta: Vec<Meta>,
    ids: Vec<String>,
    decimals: Vec<u32>,
    index: HashMap<String, usize>,
    recent: Tier,
    minute: Tier,
    quarter: Tier,
    day: Tier,
    /// The open 10 s and minute buckets: start and an accumulator per minute-tier column.
    open_recent: Option<(i64, Vec<Acc>)>,
    open_minute: Option<(i64, Vec<Acc>)>,
    /// Quarter and day rows were derived at least once since start.
    caught_up: bool,
    /// Externally provided future points (e.g. a solar forecast), by series index.
    forecasts: HashMap<usize, Vec<(i64, f64)>>,
    /// The `extra_series` globs, and how many series precede the extra ones.
    extra: Vec<String>,
    base: usize,
    pub stats: Stats,
}

/// A series made from an entity: kept in the recent, minute and day tiers.
fn entity_meta(id: &str, label: &str, unit: &str) -> Meta {
    let decimals = match unit {
        "W" | "%" => 0,
        _ => 3,
    };
    Meta { id: id.into(), label: label.into(), unit: unit.into(), decimals, sampled: true, quarter: false, day: true }
}

/// An entity's value as a sample: a number, or a bool as 1/0.
fn sample_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

impl History {
    /// Open the store, loading what the segment files hold.
    pub fn open(meta: Vec<Meta>, ret: &Retention, now: i64) -> Result<History> {
        let n = meta.len();
        let ids: Vec<String> = meta.iter().map(|m| m.id.clone()).collect();
        let index = ids.iter().enumerate().map(|(i, id)| (id.clone(), i)).collect::<HashMap<_, _>>();
        if index.len() != n {
            return Err(Error::new("history: duplicate series id"));
        }
        let cols = |f: fn(&Meta) -> bool| meta.iter().enumerate().filter(|(_, m)| f(m)).map(|(i, _)| i).collect();
        let sub = |name: &str| ret.dir.as_ref().map(|d| d.join(name));
        let mut h = History {
            decimals: meta.iter().map(|m| m.decimals).collect(),
            recent: Tier::new(Res::Recent, RECENT_KEEP_S, cols(|m| m.sampled), n, None),
            minute: Tier::new(Res::Minute, ret.minute_s, cols(|m| m.sampled), n, sub("minute")),
            quarter: Tier::new(Res::Quarter, ret.quarter_s, cols(|m| m.quarter), n, sub("quarter")),
            day: Tier::new(Res::Day, ret.day_s, cols(|m| m.day), n, sub("day")),
            meta,
            ids,
            index,
            open_recent: None,
            open_minute: None,
            caught_up: false,
            forecasts: HashMap::new(),
            extra: Vec::new(),
            base: n,
            stats: Stats::default(),
        };
        for which in [Res::Minute, Res::Quarter, Res::Day] {
            let ids = h.ids.clone();
            let l: Loaded = h.tier_mut(which).load(&ids, now)?;
            h.stats.rows_loaded += l.rows;
            h.stats.corrupt_lines += l.corrupt_lines;
            h.stats.torn_tails += l.torn_tails;
            h.stats.files_deleted += l.files_deleted;
        }
        Ok(h)
    }

    /// [`History::open`] plus the entity series picked by the `extra` globs: those the segment
    /// files already hold are loaded with the rest (labelled by id until the entity shows up).
    pub fn open_with(meta: Vec<Meta>, extra: &[String], ret: &Retention, now: i64) -> Result<History> {
        let mut h = History::open(meta.clone(), ret, now)?;
        let on_disk: BTreeSet<String> = [Res::Minute, Res::Day]
            .into_iter()
            .flat_map(|r| h.tier(r).unknown.clone())
            .filter(|id| extra.iter().any(|g| glob(g, id)))
            .collect();
        if !on_disk.is_empty() {
            let base = meta.len();
            let mut all = meta;
            all.extend(on_disk.iter().map(|id| entity_meta(id, id, "")));
            h = History::open(all, ret, now)?;
            h.base = base;
        }
        h.extra = extra.to_vec();
        Ok(h)
    }

    pub fn meta(&self) -> &[Meta] {
        &self.meta
    }

    /// Add a series at the end of the registry; it starts empty.
    pub fn add_series(&mut self, m: Meta) -> Result<usize> {
        if self.index.contains_key(&m.id) {
            return Err(Error::new(format!("history: duplicate series id {}", m.id)));
        }
        let s = self.meta.len();
        self.recent.add_series(s, m.sampled);
        self.minute.add_series(s, m.sampled);
        self.quarter.add_series(s, m.quarter);
        self.day.add_series(s, m.day);
        for open in [&mut self.open_recent, &mut self.open_minute] {
            if let Some((_, accs)) = open.as_mut().filter(|_| m.sampled) {
                accs.push(Acc::default());
            }
        }
        self.index.insert(m.id.clone(), s);
        self.ids.push(m.id.clone());
        self.decimals.push(m.decimals);
        self.meta.push(m);
        Ok(s)
    }

    /// Fill `values` (registry order, grown to the registry's length) with the entities the
    /// `extra_series` globs pick, adding a series for each one the first time it has a value. A
    /// curated series is never overwritten by an entity of the same id.
    pub fn extra_values(&mut self, entities: &[Entity], values: &mut Vec<Option<f64>>) {
        let globs = std::mem::take(&mut self.extra);
        for e in entities.iter().filter(|e| globs.iter().any(|g| glob(g, &e.id))) {
            let v = sample_of(&e.value);
            let unit = e.unit.as_deref().unwrap_or("");
            let s = match self.index_of(&e.id) {
                Some(s) if s < self.base => continue,
                Some(s) => {
                    if self.meta[s].label == e.id && e.name != e.id {
                        let fresh = entity_meta(&e.id, &e.name, unit);
                        self.decimals[s] = fresh.decimals;
                        self.meta[s] = fresh;
                    }
                    s
                }
                None if v.is_some() => self.add_series(entity_meta(&e.id, &e.name, unit)).expect("a new id"),
                None => continue,
            };
            if values.len() <= s {
                values.resize(self.meta.len(), None);
            }
            values[s] = v;
        }
        self.extra = globs;
    }

    /// The registry index of a series id.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.index.get(id).copied()
    }

    /// No rows in any persisted tier (a first start).
    pub fn is_empty(&self) -> bool {
        self.minute.rows.is_empty() && self.quarter.rows.is_empty() && self.day.rows.is_empty()
    }

    fn tier(&self, r: Res) -> &Tier {
        match r {
            Res::Recent => &self.recent,
            Res::Minute => &self.minute,
            Res::Quarter => &self.quarter,
            Res::Day => &self.day,
        }
    }

    fn tier_mut(&mut self, r: Res) -> &mut Tier {
        match r {
            Res::Recent => &mut self.recent,
            Res::Minute => &mut self.minute,
            Res::Quarter => &mut self.quarter,
            Res::Day => &mut self.day,
        }
    }

    pub fn tiers(&self) -> Vec<TierInfo> {
        [Res::Recent, Res::Minute, Res::Quarter, Res::Day]
            .into_iter()
            .map(|r| {
                let t = self.tier(r);
                TierInfo {
                    name: r.name(),
                    step_s: r.step(),
                    keep_s: t.keep_s,
                    series: t.cols.len(),
                    rows: t.rows.len(),
                    first: t.first_t(),
                    last: t.last_t(),
                }
            })
            .collect()
    }

    /// One sample per series (registry order; `None` = no value this tick) at Unix second `t`.
    pub fn record(&mut self, t: i64, values: &[Option<f64>]) {
        self.stats.samples += 1;
        for res in [Res::Recent, Res::Minute] {
            let b = res.bucket(t);
            let open = if res == Res::Recent { &mut self.open_recent } else { &mut self.open_minute };
            if open.as_ref().is_some_and(|(ob, _)| *ob != b) {
                let (ob, accs) = open.take().expect("checked");
                self.close(res, ob, &accs, b);
            }
            let (cols, open) = if res == Res::Recent {
                (&self.recent.cols, &mut self.open_recent)
            } else {
                (&self.minute.cols, &mut self.open_minute)
            };
            let (_, accs) = open.get_or_insert_with(|| (b, vec![Acc::default(); cols.len()]));
            for (acc, &s) in accs.iter_mut().zip(cols) {
                if let Some(v) = values.get(s).copied().flatten().filter(|v| v.is_finite()) {
                    acc.add(v);
                }
            }
        }
    }

    /// [`History::record`] by series id; unknown ids are ignored.
    pub fn record_by_id(&mut self, t: i64, values: &[(&str, Option<f64>)]) {
        let mut row = vec![None; self.meta.len()];
        for (id, v) in values {
            if let Some(i) = self.index_of(id) {
                row[i] = *v;
            }
        }
        self.record(t, &row);
    }

    fn close(&mut self, res: Res, start: i64, accs: &[Acc], next: i64) {
        let row = Row { t: start, v: accs.iter().map(Acc::agg).collect() };
        if res == Res::Recent {
            self.recent.put(row);
            self.recent.prune(next);
            return;
        }
        self.write(Res::Minute, &[&row]);
        self.minute.put(row);
        self.minute.prune(next);
        if !self.caught_up || Res::Quarter.bucket(start) != Res::Quarter.bucket(next) {
            self.derive(next);
        }
    }

    fn write(&mut self, res: Res, rows: &[&Row]) {
        let (ids, dec) = (&self.ids, &self.decimals);
        let tier = match res {
            Res::Minute => &mut self.minute,
            Res::Quarter => &mut self.quarter,
            _ => &mut self.day,
        };
        match tier.append(rows, ids, dec) {
            Ok(n) => {
                self.stats.rows_written += rows.len() as u64;
                self.stats.bytes_written += n;
            }
            Err(e) => {
                if self.stats.write_errors == 0 {
                    tracing::error!(error = %e, "writing history failed");
                }
                self.stats.write_errors += 1;
                self.stats.last_error = Some(e.0);
            }
        }
    }

    /// Build the quarter and day rows of every bucket that ended at or before `upto`, has minute
    /// rows, and has no row of its own yet.
    fn derive(&mut self, upto: i64) {
        self.caught_up = true;
        for res in [Res::Quarter, Res::Day] {
            let target = self.tier(res);
            let mut built: Vec<Row> = vec![];
            let mut cur: Option<(i64, Vec<Merge>)> = None;
            for row in &self.minute.rows {
                let b = res.bucket(row.t);
                if cur.as_ref().is_none_or(|(cb, _)| *cb != b) {
                    if let Some((cb, m)) = cur.take() {
                        built.push(Row { t: cb, v: m.iter().map(Merge::agg).collect() });
                    }
                    if res.next(b) > upto || target.has(b) {
                        continue;
                    }
                    cur = Some((b, vec![Merge::default(); target.cols.len()]));
                }
                let (_, m) = cur.as_mut().expect("opened above");
                for (c, &s) in target.cols.iter().enumerate() {
                    if let Some(p) = self.minute.pos[s] {
                        m[c].add(row.v[p], 60.0);
                    }
                }
            }
            if let Some((cb, m)) = cur.take() {
                built.push(Row { t: cb, v: m.iter().map(Merge::agg).collect() });
            }
            if built.is_empty() {
                continue;
            }
            self.write(res, &built.iter().collect::<Vec<_>>());
            let tier = self.tier_mut(res);
            for row in built {
                tier.put(row);
            }
            tier.prune(upto);
        }
    }

    /// Close the open minute (at shutdown), so its samples are not lost.
    pub fn flush(&mut self) {
        if let Some((ob, accs)) = self.open_minute.take() {
            self.close(Res::Minute, ob, &accs, ob + 60);
        }
    }

    /// Rows from elsewhere (the HA recorder): `(t, [(series index, aggregate)])` for one tier.
    /// Values already present win; the rows are appended to the segments; quarter and day rows
    /// are then derived for imported minutes.
    pub fn import(&mut self, res: Res, rows: Vec<(i64, Vec<(usize, Agg)>)>, now: i64) {
        let mut add: Vec<Row> = vec![];
        let tier = self.tier(res);
        for (t, vals) in rows {
            let t = res.bucket(t);
            let mut row = tier.empty_row(t);
            for (s, a) in vals {
                if let Some(p) = tier.pos.get(s).copied().flatten() {
                    row.v[p] = a;
                }
            }
            if let Some(old) = tier.range(t, t + 1).next() {
                for (n, o) in row.v.iter_mut().zip(old.v.iter()) {
                    if !o.is_none() {
                        *n = *o;
                    }
                }
            }
            add.push(row);
        }
        add.sort_by_key(|r| r.t);
        self.stats.rows_imported += add.len();
        self.write(res, &add.iter().collect::<Vec<_>>());
        let tier = self.tier_mut(res);
        for row in add {
            tier.put(row);
        }
        tier.prune(now);
        if res == Res::Minute {
            self.derive(Res::Quarter.bucket(now));
        }
    }

    /// Replace the future points of a series that is set from outside (a forecast).
    pub fn set_forecast(&mut self, id: &str, points: Vec<(i64, f64)>) -> Result<()> {
        let i = self.index_of(id).ok_or_else(|| Error::new(format!("onbekende reeks: {id}")))?;
        self.forecasts.insert(i, points);
        Ok(())
    }

    /// The series re-bucketed to `step` seconds over `[from, to)`. A step that is a multiple of a
    /// day buckets by local days. Each part of the range comes from the finest tier (no coarser
    /// than `step`) that has data there.
    pub fn query(&self, want: &[(String, Pick)], from: i64, to: i64, step: i64) -> Result<QueryOut> {
        let step = step.max(Res::Recent.step());
        if to <= from {
            return Err(Error::new("'to' moet na 'from' liggen"));
        }
        let mut sel = vec![];
        for (id, p) in want {
            sel.push((self.index_of(id).ok_or_else(|| Error::new(format!("onbekende reeks: {id}")))?, *p));
        }
        let days = step % 86_400 == 0;
        let mut bs = vec![];
        let mut b = if days { Res::Day.bucket(from) } else { from - from.rem_euclid(step) };
        while b < to {
            if bs.len() == MAX_BUCKETS {
                return Err(Error::new(format!("te veel punten (max {MAX_BUCKETS}): kies een grotere stap")));
            }
            bs.push(b);
            b = if days { (0..step / 86_400).fold(b, |x, _| Res::Day.next(x)) } else { b + step };
        }
        let mut acc = vec![vec![Merge::default(); bs.len()]; sel.len()];
        let slot = |t: i64| (t >= bs[0] && t < to).then(|| bs.partition_point(|b| *b <= t) - 1);

        // Per cell (a bucket of the coarsest tier the step allows) and series, the tier whose rows
        // cover most of it wins; a tie goes to the finer tier. So a long range reads the coarse
        // tiers where the fine ones have been pruned, without counting any span twice.
        let tiers: Vec<Res> =
            [Res::Recent, Res::Minute, Res::Quarter, Res::Day].into_iter().filter(|r| r.step() <= step).collect();
        let cell_res = *tiers.last().expect("the recent tier is always eligible");
        let open = self.open_recent.as_ref().map(|(t, a)| Row { t: *t, v: a.iter().map(Acc::agg).collect() });
        let rows = |r: Res| {
            let tier = self.tier(r);
            let open = open.as_ref().filter(|_| r == Res::Recent);
            tier.range(bs[0], to).chain(open).map(move |row| (tier, row))
        };
        let mut best: HashMap<(i64, usize), (i64, usize)> = HashMap::new();
        for (k, &r) in tiers.iter().enumerate() {
            let mut cover: HashMap<(i64, usize), i64> = HashMap::new();
            for (tier, row) in rows(r) {
                let cell = cell_res.bucket(row.t);
                for (j, (s, _)) in sel.iter().enumerate() {
                    if tier.pos[*s].is_some_and(|p| !row.v[p].is_none()) {
                        *cover.entry((cell, j)).or_default() += r.step();
                    }
                }
            }
            for (key, c) in cover {
                let b = best.entry(key).or_insert((0, k));
                if c > b.0 {
                    *b = (c, k);
                }
            }
        }
        for (k, &r) in tiers.iter().enumerate() {
            for (tier, row) in rows(r) {
                let Some(i) = slot(row.t) else { continue };
                let cell = cell_res.bucket(row.t);
                for (j, (s, _)) in sel.iter().enumerate() {
                    if let Some(p) = tier.pos[*s]
                        && best.get(&(cell, j)).is_some_and(|b| b.1 == k)
                    {
                        acc[j][i].add(row.v[p], r.step() as f64);
                    }
                }
            }
        }
        for (k, (s, _)) in sel.iter().enumerate() {
            for &(t, v) in self.forecasts.get(s).map(Vec::as_slice).unwrap_or_default() {
                if let Some(i) = slot(t) {
                    acc[k][i].add(Agg::one(v), 1.0);
                }
            }
        }
        let series = sel
            .iter()
            .zip(acc)
            .map(|((s, p), m)| {
                let meta = &self.meta[*s];
                let scale = 10f64.powi(meta.decimals as i32 + 1);
                SeriesOut {
                    id: meta.id.clone(),
                    label: meta.label.clone(),
                    unit: meta.unit.clone(),
                    agg: p.label(),
                    v: m.iter().map(|x| x.get(*p).map(|v| (v * scale).round() / scale)).collect(),
                }
            })
            .collect();
        Ok(QueryOut { from, to, step, t: bs, series })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use energie_core::time::TZ;

    fn t(d: u32, h: u32, mi: u32, s: u32) -> i64 {
        TZ.with_ymd_and_hms(2026, 11, d, h, mi, s).unwrap().timestamp()
    }

    pub(crate) fn meta() -> Vec<Meta> {
        let m = |id: &str, quarter: bool, day: bool, sampled: bool| Meta {
            id: id.into(),
            label: id.to_uppercase(),
            unit: "W".into(),
            decimals: 1,
            sampled,
            quarter,
            day,
        };
        vec![
            m("p1", true, true, true),
            m("soc", false, true, true),
            m("ev", false, false, true),
            m("fc", false, false, false),
        ]
    }

    fn ret(dir: Option<PathBuf>) -> Retention {
        Retention { dir, minute_s: 14 * 86_400, quarter_s: 430 * 86_400, day_s: 1100 * 86_400 }
    }

    fn q(h: &History, ids: &[(&str, Pick)], from: i64, to: i64, step: i64) -> Vec<Vec<Option<f64>>> {
        let want: Vec<(String, Pick)> = ids.iter().map(|(i, p)| (i.to_string(), *p)).collect();
        h.query(&want, from, to, step).unwrap().series.into_iter().map(|s| s.v).collect()
    }

    /// Samples aggregate per 10 s and per minute; a closed minute is written; quarter and day
    /// rows follow from the minutes; queries re-bucket and pick mean/min/max/last.
    #[test]
    fn record_aggregate_and_query() {
        let d = tempfile::tempdir().unwrap();
        let t0 = t(8, 10, 0, 0);
        let mut h = History::open(meta(), &ret(Some(d.path().into())), t0).unwrap();
        assert!(h.is_empty());
        assert_eq!(h.meta().len(), 4);
        assert_eq!(h.tier_mut(Res::Recent).res, Res::Recent);
        // 20 minutes of p1 = minute index × 10, soc every other tick, ev never.
        for s in 0..20 * 60 {
            let soc = (s % 2 == 0).then_some(50.0 + f64::from(s / 60));
            h.record(t0 + i64::from(s), &[Some(f64::from(s / 60) * 10.0), soc, None, Some(f64::NAN)]);
        }
        h.record_by_id(t0 + 1200, &[("p1", Some(0.0)), ("weg", Some(1.0))]);
        assert_eq!(h.stats.samples, 1201);
        let tiers = h.tiers();
        assert_eq!(tiers[0].rows, 120, "10 s rows (the open one excluded)");
        assert_eq!(tiers[1].rows, 20);
        assert_eq!(tiers[2].rows, 1, "10:00–10:15 is complete, 10:15 is not");
        assert_eq!(tiers[3].rows, 0, "the day is not over");
        assert_eq!(h.stats.rows_written, 21);
        assert!(!h.is_empty());

        // The quarter row: mean of minutes 0..14 = 70, max 140, last 140.
        let quarter = h.query(&[("p1".into(), Pick::Mean)], t0, t0 + 900, 900).unwrap();
        assert_eq!(quarter.t, vec![t0]);
        assert_eq!(quarter.series[0].v, vec![Some(70.0)]);
        assert_eq!(quarter.series[0].label, "P1");
        assert_eq!(
            q(&h, &[("p1", Pick::Max), ("p1", Pick::Last), ("p1", Pick::Min)], t0, t0 + 900, 900),
            [vec![Some(140.0)], vec![Some(140.0)], vec![Some(0.0)]]
        );
        // 5-minute buckets, the series without samples is null.
        let v = q(&h, &[("p1", Pick::Mean), ("ev", Pick::Mean), ("soc", Pick::Last)], t0, t0 + 1200, 300);
        assert_eq!(v[0], vec![Some(20.0), Some(70.0), Some(120.0), Some(170.0)]);
        assert_eq!(v[1], vec![None; 4]);
        assert_eq!(v[2], vec![Some(54.0), Some(59.0), Some(64.0), Some(69.0)]);
        // 30 s buckets from the recent tier, including the open 10 s bucket.
        let v = q(&h, &[("p1", Pick::Mean)], t0 + 1140, t0 + 1230, 30);
        assert_eq!(v[0], vec![Some(190.0), Some(190.0), Some(0.0)]);

        // Nothing is lost across a restart: the minute rows load, and the day row follows once
        // the day is over.
        h.flush();
        h.flush();
        let next_day = t(9, 0, 0, 5);
        let mut h2 = History::open(meta(), &ret(Some(d.path().into())), next_day).unwrap();
        assert_eq!(h2.stats.rows_loaded, 21 + 1);
        h2.record(next_day, &[Some(1.0), None, None, None]);
        h2.record(next_day + 60, &[Some(1.0), None, None, None]);
        let tiers = h2.tiers();
        assert_eq!(tiers[2].rows, 2, "10:15–10:30 derived from the minutes after the restart");
        assert_eq!(tiers[3].rows, 1);
        let v = q(&h2, &[("p1", Pick::Max), ("soc", Pick::Last)], t(8, 0, 0, 0), t(10, 0, 0, 0), 86_400);
        assert_eq!(v, [vec![Some(190.0), Some(1.0)], vec![Some(69.0), None]]);
    }

    /// A long query reads the coarse tiers where the fine ones have no data.
    #[test]
    fn queries_stitch_tiers() {
        let now = t(20, 12, 0, 0);
        let mut h = History::open(meta(), &ret(None), now).unwrap();
        let old = t(1, 0, 0, 0);
        h.import(Res::Day, vec![(old, vec![(0, Agg::one(5.0)), (1, Agg::one(80.0)), (2, Agg::one(1.0))])], now);
        h.import(Res::Quarter, vec![(t(2, 10, 0, 0), vec![(0, Agg::one(7.0))])], now);
        for s in 0..120 {
            h.record(now - 120 + s, &[Some(3.0), Some(60.0), None, None]);
        }
        let v = q(&h, &[("p1", Pick::Max)], old, now + 86_400, 86_400);
        assert_eq!(v[0][0], Some(5.0), "day tier");
        assert_eq!(v[0][1], Some(7.0), "quarter tier");
        assert_eq!(v[0][19], Some(3.0), "minute and recent tiers");
        assert_eq!(v[0].iter().filter(|x| x.is_some()).count(), 3);
        // Two-day buckets.
        let v = q(&h, &[("p1", Pick::Max)], old, t(5, 0, 0, 0), 2 * 86_400);
        assert_eq!(v[0], vec![Some(7.0), None]);
        // Limits and bad requests.
        let all = [("p1".to_string(), Pick::Mean)];
        assert!(h.query(&all, now, now, 60).unwrap_err().0.contains("na"));
        assert!(h.query(&all, now - 86_400 * 30, now, 10).unwrap_err().0.contains("max"));
        assert!(h.query(&[("x".into(), Pick::Mean)], now - 60, now, 60).unwrap_err().0.contains("x"));
        assert_eq!(h.query(&all, now - 60, now, 1).unwrap().step, 10);
    }

    /// Forecast points show up in queries of their series; unknown ids are refused.
    #[test]
    fn forecasts() {
        let now = t(8, 12, 0, 0);
        let mut h = History::open(meta(), &ret(None), now).unwrap();
        h.set_forecast("fc", vec![(now + 600, 1000.0), (now + 1200, 2000.0), (now + 90_000, 5.0)]).unwrap();
        assert!(h.set_forecast("nee", vec![]).is_err());
        let v = q(&h, &[("fc", Pick::Mean), ("p1", Pick::Mean)], now, now + 3600, 900);
        assert_eq!(v[0], vec![Some(1000.0), Some(2000.0), None, None]);
        assert_eq!(v[1], vec![None; 4]);
        assert_eq!(h.tiers()[0].series, 3, "the forecast is not sampled");
    }

    /// Entities the `extra_series` globs pick become series once they have a value (a bool as
    /// 1/0), never overwrite a curated series, take the entity's label and unit, live in the
    /// recent, minute and day tiers, and load again after a restart (labelled by id until the
    /// entity shows up).
    #[test]
    fn entity_series_by_glob() {
        let d = tempfile::tempdir().unwrap();
        let t0 = t(8, 10, 0, 0);
        let globs = vec!["ev3_*".to_string(), "p1".to_string()];
        let mut h = History::open_with(meta(), &globs, &ret(Some(d.path().into())), t0).unwrap();
        let e = |id: &str, v: Value| Entity::sensor(id, format!("Naam {id}"), v);
        let ents = vec![
            e("ev3_soc", Value::Number(55.0)).with_unit("battery", "%"),
            e("ev3_plugged", Value::Bool(true)),
            e("ev3_note", Value::Text("x".into())),
            e("ev3_dc_limit", Value::Null),
            e("p1", Value::Number(-5.0)),
            e("buzz_soc", Value::Number(40.0)),
        ];
        // The entities show up in the second minute, after the first one was written.
        for s in 0..120 {
            let mut v = vec![Some(1.0), None, None, None];
            if s >= 70 {
                h.extra_values(&ents, &mut v);
            }
            h.record(t0 + s, &v);
        }
        let ids: Vec<_> = h.meta().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["p1", "soc", "ev", "fc", "ev3_soc", "ev3_plugged"]);
        assert_eq!(
            (h.meta()[4].label.as_str(), h.meta()[4].unit.as_str(), h.meta()[4].decimals),
            ("Naam ev3_soc", "%", 0)
        );
        assert_eq!(h.meta()[5].decimals, 3);
        let tiers = h.tiers();
        assert_eq!((tiers[1].series, tiers[2].series, tiers[3].series), (5, 1, 4), "not in the quarter tier");
        let v = q(&h, &[("p1", Pick::Mean), ("ev3_soc", Pick::Mean), ("ev3_plugged", Pick::Max)], t0, t0 + 120, 60);
        assert_eq!(v, [vec![Some(1.0); 2], vec![None, Some(55.0)], vec![None, Some(1.0)]], "the curated p1 is kept");
        assert!(h.add_series(meta()[0].clone()).unwrap_err().0.contains("duplicate"));
        h.flush();
        let minute = std::fs::read_to_string(d.path().join("minute/2026-11-08.jsonl")).unwrap();
        assert_eq!(minute.matches("{\"series\"").count(), 2, "a new header once the series were added");

        let mut h2 = History::open_with(meta(), &globs, &ret(Some(d.path().into())), t0 + 120).unwrap();
        let ids: Vec<_> = h2.meta().iter().map(|m| (m.id.as_str(), m.label.as_str())).collect();
        assert_eq!(ids[4..], [("ev3_plugged", "ev3_plugged"), ("ev3_soc", "ev3_soc")]);
        assert_eq!(q(&h2, &[("ev3_soc", Pick::Mean)], t0 + 60, t0 + 120, 60), [vec![Some(55.0)]]);
        let mut later = ents.clone();
        later[3].value = Value::Number(100.0);
        let mut v = vec![None; 4];
        h2.extra_values(&later, &mut v);
        assert_eq!(v, [None, None, None, None, Some(1.0), Some(55.0), Some(100.0)]);
        assert_eq!((h2.meta()[5].label.as_str(), h2.meta()[5].unit.as_str()), ("Naam ev3_soc", "%"));
        assert_eq!(h2.meta()[6].id, "ev3_dc_limit");
        let plain = History::open_with(meta(), &[], &ret(Some(d.path().into())), t0 + 120).unwrap();
        assert_eq!(plain.meta().len(), 4, "without globs the entity series stay on disk only");
    }

    /// Imported rows fill gaps but never overwrite what is there; imported minutes get their
    /// quarter rows.
    #[test]
    fn import_merges_and_derives() {
        let d = tempfile::tempdir().unwrap();
        let now = t(8, 12, 0, 0);
        let mut h = History::open(meta(), &ret(Some(d.path().into())), now).unwrap();
        for s in 0..61 {
            h.record(t(8, 11, 0, 0) + s, &[Some(10.0), None, None, None]);
        }
        let rows = (0..3).map(|k| (t(8, 11, 0, 0) + k * 300, vec![(0, Agg::one(99.0)), (1, Agg::one(40.0))])).collect();
        h.import(Res::Minute, rows, now);
        h.import(Res::Minute, vec![(now, vec![(9, Agg::one(1.0))])], now);
        assert_eq!(h.stats.rows_imported, 4);
        let v = q(&h, &[("p1", Pick::Mean), ("soc", Pick::Mean)], t(8, 11, 0, 0), t(8, 11, 15, 0), 300);
        assert_eq!(v[0], vec![Some(10.0), Some(99.0), Some(99.0)], "the live minute wins");
        assert_eq!(v[1], vec![Some(40.0); 3]);
        assert!(h.tier(Res::Quarter).has(t(8, 11, 0, 0)));
        let back = History::open(meta(), &ret(Some(d.path().into())), now).unwrap();
        assert_eq!(back.tier(Res::Minute).rows.len(), 4);
    }

    /// A write failure is counted and the store goes on in memory.
    #[test]
    fn write_errors_are_counted() {
        let d = tempfile::tempdir().unwrap();
        let now = t(8, 12, 0, 0);
        let mut h = History::open(meta(), &ret(Some(d.path().into())), now).unwrap();
        std::fs::remove_dir_all(d.path().join("minute")).unwrap();
        std::fs::write(d.path().join("minute"), "").unwrap();
        for s in 0..130 {
            h.record(now + s, &[Some(1.0), None, None, None]);
        }
        assert_eq!(h.stats.write_errors, 2);
        assert!(h.stats.last_error.as_deref().unwrap().contains("minute"));
        assert_eq!(h.tiers()[1].rows, 2);
    }

    /// Duplicate ids and unreadable directories fail at open.
    #[test]
    fn open_errors() {
        let mut m = meta();
        m.push(m[0].clone());
        assert!(History::open(m, &ret(None), 0).err().unwrap().0.contains("duplicate"));
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f"), "").unwrap();
        assert!(History::open(meta(), &ret(Some(d.path().join("f"))), 0).is_err());
    }
}
