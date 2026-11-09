//! One resolution of the history store: rows of per-series aggregates in time order, and the
//! JSON-lines segment files they are kept in.
//!
//! A segment is one file per local day (`minute/2026-11-08.jsonl`), month (`quarter/2026-11.jsonl`)
//! or year (`day/2026.jsonl`). Lines are only ever appended; a segment is deleted whole once it is
//! past the retention. A header line `{"series":["p1_w",…]}` names the columns of the rows after
//! it (each process writes one before its first row in a segment, so the series list may change
//! between versions); a row is `[t,v,v,…]` with `t` the bucket start in Unix seconds and each `v`
//! `null` (no sample), one number (mean = min = max = last) or `[mean,min,max,last]`.
//!
//! Loading skips lines that do not parse and cuts a torn last line (no newline) off the file, so
//! a crash mid-write costs at most that line.

use std::collections::{BTreeSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::TimeZone;
use energie_core::meters::Cycle;
use energie_core::time::{TZ, Ts};
use serde_json::Value;

use crate::error::{Error, Result};

/// A series over one bucket: mean, min, max and the last sample. `NaN` = no sample.
#[derive(Debug, Clone, Copy)]
pub struct Agg {
    pub mean: f32,
    pub min: f32,
    pub max: f32,
    pub last: f32,
}

impl PartialEq for Agg {
    fn eq(&self, o: &Self) -> bool {
        let same = |a: f32, b: f32| a == b || (a.is_nan() && b.is_nan());
        same(self.mean, o.mean) && same(self.min, o.min) && same(self.max, o.max) && same(self.last, o.last)
    }
}

impl Agg {
    pub const NONE: Agg = Agg { mean: f32::NAN, min: f32::NAN, max: f32::NAN, last: f32::NAN };

    /// One value for all four.
    pub fn one(v: f64) -> Agg {
        let v = v as f32;
        Agg { mean: v, min: v, max: v, last: v }
    }

    pub fn is_none(&self) -> bool {
        self.mean.is_nan()
    }
}

/// Running aggregate of raw samples.
#[derive(Debug, Clone, Copy, Default)]
pub struct Acc {
    sum: f64,
    n: u32,
    min: f64,
    max: f64,
    last: f64,
}

impl Acc {
    pub fn add(&mut self, v: f64) {
        if self.n == 0 {
            (self.min, self.max) = (v, v);
        }
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += v;
        self.n += 1;
        self.last = v;
    }

    pub fn agg(&self) -> Agg {
        if self.n == 0 {
            return Agg::NONE;
        }
        let mean = self.sum / f64::from(self.n);
        Agg { mean: mean as f32, min: self.min as f32, max: self.max as f32, last: self.last as f32 }
    }
}

/// Which of the four values a query wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Mean,
    Min,
    Max,
    Last,
}

impl Pick {
    pub fn parse(s: &str) -> Option<Pick> {
        match s {
            "mean" | "avg" => Some(Pick::Mean),
            "min" => Some(Pick::Min),
            "max" => Some(Pick::Max),
            "last" => Some(Pick::Last),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Pick::Mean => "mean",
            Pick::Min => "min",
            Pick::Max => "max",
            Pick::Last => "last",
        }
    }
}

/// Aggregates of rows merged into a coarser bucket; the mean is weighted (by the rows' bucket
/// length), min/max are exact, `last` is the latest row's.
#[derive(Debug, Clone, Copy, Default)]
pub struct Merge {
    w: f64,
    sum: f64,
    min: f64,
    max: f64,
    last: f64,
}

impl Merge {
    pub fn add(&mut self, a: Agg, w: f64) {
        if a.is_none() {
            return;
        }
        if self.w == 0.0 {
            (self.min, self.max) = (f64::from(a.min), f64::from(a.max));
        }
        self.min = self.min.min(f64::from(a.min));
        self.max = self.max.max(f64::from(a.max));
        self.sum += f64::from(a.mean) * w;
        self.w += w;
        self.last = f64::from(a.last);
    }

    pub fn agg(&self) -> Agg {
        match self.get(Pick::Mean) {
            None => Agg::NONE,
            Some(mean) => Agg { mean: mean as f32, min: self.min as f32, max: self.max as f32, last: self.last as f32 },
        }
    }

    pub fn get(&self, p: Pick) -> Option<f64> {
        (self.w > 0.0).then(|| match p {
            Pick::Mean => self.sum / self.w,
            Pick::Min => self.min,
            Pick::Max => self.max,
            Pick::Last => self.last,
        })
    }
}

/// A tier's bucket length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Res {
    /// 10 s, memory only.
    Recent,
    Minute,
    Quarter,
    /// A local calendar day.
    Day,
}

impl Res {
    /// Nominal bucket length, s (a day is 86 400 even on DST days).
    pub fn step(self) -> i64 {
        match self {
            Res::Recent => 10,
            Res::Minute => 60,
            Res::Quarter => 900,
            Res::Day => 86_400,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Res::Recent => "recent",
            Res::Minute => "minute",
            Res::Quarter => "quarter",
            Res::Day => "day",
        }
    }

    /// Start of the bucket containing `t`. Brussels offsets are whole hours, so the sub-day
    /// buckets align on local time too.
    pub fn bucket(self, t: i64) -> i64 {
        match self {
            Res::Day => Cycle::Daily.period_start(ts(t)).timestamp(),
            _ => t - t.rem_euclid(self.step()),
        }
    }

    /// Start of the bucket after the one starting at `b`.
    pub fn next(self, b: i64) -> i64 {
        match self {
            Res::Day => Cycle::Daily.next_reset_after(ts(b)).timestamp(),
            _ => b + self.step(),
        }
    }

    /// The first bucket start at or after `t`.
    pub fn ceil(self, t: i64) -> i64 {
        let b = self.bucket(t);
        if b == t { b } else { self.next(b) }
    }

    /// The segment file a row at `t` goes in (local date: day, month or year); sorts like time.
    pub fn stem(self, t: i64) -> String {
        let fmt = match self {
            Res::Day => "%Y",
            Res::Quarter => "%Y-%m",
            _ => "%Y-%m-%d",
        };
        ts(t).format(fmt).to_string()
    }
}

/// Unix seconds as Brussels time.
pub fn ts(t: i64) -> Ts {
    TZ.timestamp_opt(t, 0).single().expect("a Unix second is one instant")
}

/// One bucket: an aggregate per column of the tier.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub t: i64,
    pub v: Box<[Agg]>,
}

impl Row {
    /// Fill the values this row lacks from `other` (same layout); `other` wins where both have one.
    fn merge_from(&mut self, other: &Row) {
        for (a, b) in self.v.iter_mut().zip(other.v.iter()) {
            if !b.is_none() {
                *a = *b;
            }
        }
    }
}

/// What loading a tier found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Loaded {
    pub rows: usize,
    pub corrupt_lines: usize,
    pub files_deleted: usize,
    pub torn_tails: usize,
}

pub struct Tier {
    pub res: Res,
    /// Retention, s.
    pub keep_s: i64,
    /// Registry index of each column.
    pub cols: Vec<usize>,
    /// Column of each registry index (`None`: this tier does not keep that series).
    pub pos: Vec<Option<usize>>,
    /// Sorted by `t`, one row per bucket.
    pub rows: VecDeque<Row>,
    /// Segment directory; `None` keeps the tier in memory only.
    dir: Option<PathBuf>,
    /// Segments this process has written a header to.
    headed: Vec<String>,
    /// Series ids found in segment headers that the registry does not have.
    pub unknown: BTreeSet<String>,
}

impl Tier {
    pub fn new(res: Res, keep_s: i64, cols: Vec<usize>, n_series: usize, dir: Option<PathBuf>) -> Self {
        let mut pos = vec![None; n_series];
        for (c, &s) in cols.iter().enumerate() {
            pos[s] = Some(c);
        }
        Tier { res, keep_s, cols, pos, rows: VecDeque::new(), dir, headed: vec![], unknown: BTreeSet::new() }
    }

    /// Make room for registry index `s` (the next one), as a column of this tier if `keep`.
    /// Rows get an empty value for it, and every segment gets a fresh header before the next
    /// row, so the files keep naming their columns.
    pub fn add_series(&mut self, s: usize, keep: bool) {
        self.pos.resize(s + 1, None);
        if !keep {
            return;
        }
        self.pos[s] = Some(self.cols.len());
        self.cols.push(s);
        for row in &mut self.rows {
            let mut v = std::mem::take(&mut row.v).into_vec();
            v.push(Agg::NONE);
            row.v = v.into_boxed_slice();
        }
        self.headed.clear();
    }

    pub fn empty_row(&self, t: i64) -> Row {
        Row { t, v: vec![Agg::NONE; self.cols.len()].into_boxed_slice() }
    }

    pub fn first_t(&self) -> Option<i64> {
        self.rows.front().map(|r| r.t)
    }

    pub fn last_t(&self) -> Option<i64> {
        self.rows.back().map(|r| r.t)
    }

    fn find(&self, t: i64) -> std::result::Result<usize, usize> {
        let i = self.rows.partition_point(|r| r.t < t);
        if self.rows.get(i).is_some_and(|r| r.t == t) { Ok(i) } else { Err(i) }
    }

    pub fn has(&self, t: i64) -> bool {
        self.find(t).is_ok()
    }

    /// Add a row in time order; a row for a bucket that is already there merges into it (the new
    /// values win).
    pub fn put(&mut self, row: Row) {
        match self.find(row.t) {
            Ok(i) => self.rows[i].merge_from(&row),
            Err(i) => self.rows.insert(i, row),
        }
    }

    /// The rows with `from <= t < to`.
    pub fn range(&self, from: i64, to: i64) -> impl Iterator<Item = &Row> {
        let i = self.rows.partition_point(|r| r.t < from);
        self.rows.range(i..).take_while(move |r| r.t < to)
    }

    /// Drop rows older than the retention.
    pub fn prune(&mut self, now: i64) {
        let cutoff = now - self.keep_s;
        while self.rows.front().is_some_and(|r| r.t < cutoff) {
            self.rows.pop_front();
        }
    }

    /// Append rows to their segment files, one write per segment. Returns the bytes written.
    pub fn append(&mut self, rows: &[&Row], ids: &[String], decimals: &[u32]) -> Result<u64> {
        let Some(dir) = self.dir.clone() else { return Ok(0) };
        let mut bytes = 0;
        let mut i = 0;
        while i < rows.len() {
            let stem = self.res.stem(rows[i].t);
            let mut text = String::new();
            if !self.headed.contains(&stem) {
                let names: Vec<&str> = self.cols.iter().map(|&s| ids[s].as_str()).collect();
                text.push_str(&serde_json::json!({ "series": names }).to_string());
                text.push('\n');
            }
            while i < rows.len() && self.res.stem(rows[i].t) == stem {
                text.push_str(&self.encode(rows[i], decimals));
                text.push('\n');
                i += 1;
            }
            let path = dir.join(format!("{stem}.jsonl"));
            let named = |e: std::io::Error| Error::new(format!("{}: {e}", path.display()));
            let mut f = OpenOptions::new().create(true).append(true).open(&path).map_err(named)?;
            f.write_all(text.as_bytes()).map_err(named)?;
            if !self.headed.contains(&stem) {
                self.headed.push(stem);
            }
            bytes += text.len() as u64;
        }
        Ok(bytes)
    }

    fn encode(&self, row: &Row, decimals: &[u32]) -> String {
        let mut s = format!("[{}", row.t);
        for (c, a) in row.v.iter().enumerate() {
            s.push(',');
            s.push_str(&encode_agg(a, decimals[self.cols[c]]));
        }
        s.push(']');
        s
    }

    /// Load the segment files (deleting those past the retention). `ids` is the registry.
    pub fn load(&mut self, ids: &[String], now: i64) -> Result<Loaded> {
        let mut out = Loaded::default();
        let Some(dir) = self.dir.clone() else { return Ok(out) };
        std::fs::create_dir_all(&dir).map_err(|e| Error::new(format!("{}: {e}", dir.display())))?;
        let cutoff = self.res.stem(now - self.keep_s);
        let mut files: Vec<(String, PathBuf)> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let p = e.path();
                let stem = p.file_name()?.to_str()?.strip_suffix(".jsonl")?.to_string();
                Some((stem, p))
            })
            .collect();
        files.sort();
        let mut rows = Vec::new();
        let mut out_unknown = BTreeSet::new();
        for (stem, path) in files {
            if stem < cutoff {
                std::fs::remove_file(&path)?;
                out.files_deleted += 1;
                continue;
            }
            let named = |e: std::io::Error| Error::new(format!("{}: {e}", path.display()));
            let bytes = std::fs::read(&path).map_err(named)?;
            let keep = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            if keep < bytes.len() {
                OpenOptions::new().write(true).open(&path).and_then(|f| f.set_len(keep as u64)).map_err(named)?;
                out.torn_tails += 1;
            }
            let text = String::from_utf8_lossy(&bytes[..keep]);
            let mut map: Option<Vec<Option<usize>>> = None;
            for line in text.lines().filter(|l| !l.trim().is_empty()) {
                match self.parse_line(line, ids, &mut map, &mut out_unknown) {
                    Some(Some(row)) => rows.push(row),
                    Some(None) => {}
                    None => out.corrupt_lines += 1,
                }
            }
        }
        self.unknown = out_unknown;
        rows.sort_by_key(|r| r.t);
        out.rows = rows.len();
        for row in rows {
            self.put(row);
        }
        self.prune(now);
        Ok(out)
    }

    /// `Some(Some(row))`, `Some(None)` for a header (which updates `map` and adds the ids the
    /// registry lacks to `unknown`), `None` if corrupt.
    fn parse_line(
        &self,
        line: &str,
        ids: &[String],
        map: &mut Option<Vec<Option<usize>>>,
        unknown: &mut BTreeSet<String>,
    ) -> Option<Option<Row>> {
        let v: Value = serde_json::from_str(line).ok()?;
        if let Some(names) = v.get("series") {
            let names = names.as_array()?;
            for n in names.iter().filter_map(Value::as_str).filter(|n| !ids.iter().any(|i| i == n)) {
                unknown.insert(n.to_string());
            }
            let cols = names
                .iter()
                .map(|n| n.as_str().and_then(|n| ids.iter().position(|i| i == n)).and_then(|s| self.pos[s]))
                .collect();
            *map = Some(cols);
            return Some(None);
        }
        let arr = v.as_array()?;
        let map = map.as_ref()?;
        let t = arr.first()?.as_i64()?;
        if arr.len() != map.len() + 1 {
            return None;
        }
        let mut row = self.empty_row(t);
        for (val, col) in arr[1..].iter().zip(map) {
            let a = decode_agg(val)?;
            if let Some(c) = col {
                row.v[*c] = a;
            }
        }
        Some(Some(row))
    }

    /// The directory of the segment files, if any.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

/// A number with at most `d` decimals, without trailing zeros.
fn fmt_num(x: f32, d: u32) -> String {
    let s = format!("{:.*}", d as usize, x);
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" { "0".to_string() } else { s }
}

fn encode_agg(a: &Agg, d: u32) -> String {
    if a.is_none() {
        return "null".to_string();
    }
    let f = [a.mean, a.min, a.max, a.last].map(|x| fmt_num(x, d));
    if f.iter().all(|x| *x == f[0]) { f[0].clone() } else { format!("[{},{},{},{}]", f[0], f[1], f[2], f[3]) }
}

fn decode_agg(v: &Value) -> Option<Agg> {
    match v {
        Value::Null => Some(Agg::NONE),
        Value::Number(n) => Some(Agg::one(n.as_f64()?)),
        Value::Array(a) if a.len() == 4 => {
            let f: Vec<f32> = a.iter().map(|x| x.as_f64().map(|x| x as f32)).collect::<Option<_>>()?;
            Some(Agg { mean: f[0], min: f[1], max: f[2], last: f[3] })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        TZ.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp()
    }

    fn ids() -> Vec<String> {
        vec!["a".into(), "b".into(), "c".into()]
    }

    /// Accumulators: mean, min, max and last of the samples; nothing is `NONE`.
    #[test]
    fn acc_and_merge() {
        let mut a = Acc::default();
        assert!(a.agg().is_none());
        for v in [3.0, 1.0, 2.0] {
            a.add(v);
        }
        assert_eq!(a.agg(), Agg { mean: 2.0, min: 1.0, max: 3.0, last: 2.0 });
        let mut m = Merge::default();
        assert!(m.agg().is_none());
        assert_eq!(m.get(Pick::Max), None);
        m.add(Agg::NONE, 60.0);
        m.add(Agg { mean: 10.0, min: 5.0, max: 20.0, last: 7.0 }, 60.0);
        m.add(Agg::one(1.0), 180.0);
        assert_eq!(m.get(Pick::Mean), Some(3.25));
        assert_eq!(m.get(Pick::Min), Some(1.0));
        assert_eq!(m.get(Pick::Max), Some(20.0));
        assert_eq!(m.get(Pick::Last), Some(1.0));
        assert_eq!(m.agg(), Agg { mean: 3.25, min: 1.0, max: 20.0, last: 1.0 });
        assert_ne!(Agg::one(1.0), Agg::NONE);
        assert_eq!(Agg::NONE, Agg::NONE);
        for p in [Pick::Mean, Pick::Min, Pick::Max, Pick::Last] {
            assert_eq!(Pick::parse(p.label()), Some(p));
        }
        assert_eq!(Pick::parse("avg"), Some(Pick::Mean));
        assert_eq!(Pick::parse("median"), None);
    }

    /// Buckets align on local quarters and local midnight, also across the DST change.
    #[test]
    fn buckets_and_segments() {
        let x = t(2026, 10, 25, 13, 47) + 12;
        assert_eq!(Res::Recent.bucket(x), x - 2);
        assert_eq!(Res::Minute.bucket(x), t(2026, 10, 25, 13, 47));
        assert_eq!(Res::Quarter.bucket(x), t(2026, 10, 25, 13, 45));
        assert_eq!(Res::Quarter.next(Res::Quarter.bucket(x)), t(2026, 10, 25, 14, 0));
        assert_eq!(Res::Day.bucket(x), t(2026, 10, 25, 0, 0));
        // 25 October 2026 has 25 hours in Brussels.
        assert_eq!(Res::Day.next(t(2026, 10, 25, 0, 0)) - t(2026, 10, 25, 0, 0), 25 * 3600);
        assert_eq!(Res::Day.ceil(t(2026, 10, 25, 0, 0)), t(2026, 10, 25, 0, 0));
        assert_eq!(Res::Day.ceil(x), t(2026, 10, 26, 0, 0));
        assert_eq!(Res::Minute.stem(x), "2026-10-25");
        assert_eq!(Res::Quarter.stem(x), "2026-10");
        assert_eq!(Res::Day.stem(x), "2026");
        assert_eq!(Res::Recent.stem(x), "2026-10-25");
        let names: Vec<_> = [Res::Recent, Res::Minute, Res::Quarter, Res::Day].map(Res::name).into();
        assert_eq!(names, ["recent", "minute", "quarter", "day"]);
        assert_eq!(ts(0).timestamp(), 0);
    }

    /// Values are written with the series' decimals, compactly when all four agree.
    #[test]
    fn encoding() {
        assert_eq!(fmt_num(3066.25, 0), "3066");
        assert_eq!(fmt_num(0.5, 3), "0.5");
        assert_eq!(fmt_num(2.0, 3), "2");
        assert_eq!(fmt_num(-0.0001, 2), "0");
        assert_eq!(encode_agg(&Agg::NONE, 0), "null");
        assert_eq!(encode_agg(&Agg::one(12.0), 1), "12");
        assert_eq!(encode_agg(&Agg { mean: 1.5, min: 1.0, max: 2.0, last: 2.0 }, 1), "[1.5,1,2,2]");
        assert_eq!(decode_agg(&serde_json::json!(null)), Some(Agg::NONE));
        assert_eq!(decode_agg(&serde_json::json!(4)), Some(Agg::one(4.0)));
        assert_eq!(
            decode_agg(&serde_json::json!([1, 2, 3, 4])),
            Some(Agg { mean: 1.0, min: 2.0, max: 3.0, last: 4.0 })
        );
        assert_eq!(decode_agg(&serde_json::json!([1, 2])), None);
        assert_eq!(decode_agg(&serde_json::json!([1, 2, "x", 4])), None);
        assert_eq!(decode_agg(&serde_json::json!("x")), None);
    }

    /// Rows go in time order; a second row for the same bucket merges.
    #[test]
    fn put_range_prune() {
        let mut tier = Tier::new(Res::Minute, 120, vec![0, 2], 3, None);
        assert_eq!((tier.first_t(), tier.last_t()), (None, None));
        let mut r = tier.empty_row(120);
        r.v[0] = Agg::one(1.0);
        tier.put(r);
        let mut r = tier.empty_row(60);
        r.v[1] = Agg::one(2.0);
        tier.put(r);
        let mut r = tier.empty_row(120);
        r.v[1] = Agg::one(3.0);
        tier.put(r);
        assert_eq!((tier.first_t(), tier.last_t()), (Some(60), Some(120)));
        assert_eq!(tier.rows[1].v[..], [Agg::one(1.0), Agg::one(3.0)]);
        assert!(tier.has(60) && !tier.has(0));
        assert_eq!(tier.range(61, 1000).count(), 1);
        assert_eq!(tier.range(0, 120).count(), 1);
        assert_eq!(tier.pos, vec![Some(0), None, Some(1)]);
        tier.prune(200);
        assert_eq!(tier.first_t(), Some(120));
        assert_eq!(tier.append(&[&tier.rows[0].clone()], &ids(), &[0, 0, 0]).unwrap(), 0, "memory only");
        assert!(tier.dir().is_none());
    }

    /// Written rows load back; a header per process and segment; corrupt lines are skipped, a
    /// torn tail is cut off, segments past the retention are deleted.
    #[test]
    fn files_roundtrip_and_survive_damage() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("minute");
        let day1 = t(2026, 11, 7, 23, 59);
        let day2 = t(2026, 11, 8, 0, 0);
        let mut tier = Tier::new(Res::Minute, 14 * 86_400, vec![0, 1], 3, Some(dir.clone()));
        assert_eq!(tier.load(&ids(), day2).unwrap(), Loaded::default());
        let mut r1 = tier.empty_row(day1);
        r1.v[0] = Agg { mean: 1.25, min: 1.0, max: 2.0, last: 1.5 };
        let mut r2 = tier.empty_row(day2);
        r2.v[1] = Agg::one(7.0);
        let n = tier.append(&[&r1, &r2], &ids(), &[2, 0, 0]).unwrap();
        assert!(n > 0);
        let r3 = Row { t: day2 + 60, v: r2.v.clone() };
        tier.append(&[&r3], &ids(), &[2, 0, 0]).unwrap();
        let text = std::fs::read_to_string(dir.join("2026-11-08.jsonl")).unwrap();
        assert_eq!(text.lines().filter(|l| l.contains("series")).count(), 1, "one header per segment: {text}");
        assert!(std::fs::read_to_string(dir.join("2026-11-07.jsonl")).unwrap().contains("[1.25,1,2,1.5],null]"));

        // Damage: a corrupt line, a row before any header, a torn tail, an expired segment, a
        // header with an unknown series, a non-jsonl file.
        let mut f = OpenOptions::new().append(true).open(dir.join("2026-11-08.jsonl")).unwrap();
        f.write_all(b"kapot\n[1,2]\n\n{\"series\":[\"c\",\"zz\",\"b\"]}\n[").unwrap();
        let line = format!("{},5,6,8]\n", day2 + 120);
        f.write_all(line.as_bytes()).unwrap();
        f.write_all(b"[999").unwrap();
        std::fs::write(dir.join("2026-10-01.jsonl"), "{\"series\":[\"a\"]}\n[1,1]\n").unwrap();
        std::fs::write(dir.join("2026-11-06.jsonl"), "[1,1]\n{\"series\":\"a\"}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let mut back = Tier::new(Res::Minute, 14 * 86_400, vec![0, 1], 3, Some(dir.clone()));
        let l = back.load(&ids(), day2).unwrap();
        assert_eq!(l, Loaded { rows: 4, corrupt_lines: 4, files_deleted: 1, torn_tails: 1 });
        assert_eq!(back.rows.len(), 4);
        assert_eq!(back.rows[0], r1);
        assert_eq!(back.rows[1], r2);
        assert_eq!(back.rows[3].v[..], [Agg::NONE, Agg::one(8.0)], "c is not kept here, zz unknown");
        assert!(!dir.join("2026-10-01.jsonl").exists());
        assert!(std::fs::read_to_string(dir.join("2026-11-08.jsonl")).unwrap().ends_with("8]\n"));
        // A new process writes its own header before appending.
        back.append(&[&r3], &ids(), &[2, 0, 0]).unwrap();
        let text = std::fs::read_to_string(dir.join("2026-11-08.jsonl")).unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with("{\"series\":[\"a\",\"b\"]")).count(), 2);
    }

    /// I/O failures name the path.
    #[test]
    fn io_errors() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("f");
        std::fs::write(&file, "").unwrap();
        let mut tier = Tier::new(Res::Day, 86_400, vec![0], 1, Some(file.join("sub")));
        assert!(tier.load(&ids(), 0).unwrap_err().0.contains("sub"));
        let row = tier.empty_row(0);
        assert!(tier.append(&[&row], &ids(), &[0]).unwrap_err().0.contains("sub"));
        // A segment that is a directory cannot be read.
        let dir = d.path().join("day");
        std::fs::create_dir_all(dir.join("2026.jsonl")).unwrap();
        let mut tier = Tier::new(Res::Day, 86_400, vec![0], 1, Some(dir));
        assert!(tier.load(&ids(), t(2026, 11, 8, 0, 0)).unwrap_err().0.contains("2026.jsonl"));
    }
}
