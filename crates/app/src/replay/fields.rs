//! Field parity over a replay: every `Derived` field against HA's recorded state of its
//! output entity. A Rust sample at `t` agrees when it matches (within the field's shadow
//! tolerance) any value HA showed in `[t − tol, t + tol]`: HA's template sensors update when
//! a source changes, a few seconds before or after Rust's tick.
//!
//! HA's values come from the recorder rows as they are applied (with their own timestamps);
//! a sample is judged once the replay has applied every row up to `t + tol`.

use std::collections::{HashMap, VecDeque};

use energie_core::derived::Derived;
use serde::Serialize;

use super::{Episode, fmt_ts};
use crate::ha::entity::EntityState;
use crate::ha::outputs::{Output, outputs};
use crate::shadow::Val;

/// Worst episodes kept per field.
pub const KEEP_WORST: usize = 5;
/// Traced samples kept (`--trace`).
pub const KEEP_TRACE: usize = 20_000;

/// A disagreeing sample of the traced field: Rust's value and every HA value in the window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceLine {
    pub at: String,
    pub rust: Val,
    pub ha: Vec<Val>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldReport {
    pub field: &'static str,
    pub entity: &'static str,
    /// Compared samples (HA had the entity, outside gaps and settle windows).
    pub samples: u64,
    pub matches: u64,
    pub agreement_pct: Option<f64>,
    /// Samples where HA never had the entity (not exported / does not exist).
    pub missing: u64,
    /// Samples in a gap's settle window.
    pub skipped: u64,
    /// Mean of the smallest |rust − ha| in the window over the numeric samples.
    pub mean_abs_err: Option<f64>,
    pub max_abs_err: Option<f64>,
    pub max_err_at: Option<String>,
    /// Runs of consecutive disagreeing samples.
    pub mismatch_episodes: u64,
    /// The worst episodes: largest error, or longest for non-numeric fields.
    pub worst: Vec<Episode>,
}

#[derive(Default)]
struct Acc {
    samples: u64,
    matches: u64,
    missing: u64,
    skipped: u64,
    err_sum: f64,
    err_n: u64,
    max_err: Option<(f64, f64)>,
    episodes: u64,
    cur: Option<Episode>,
    worst: Vec<Episode>,
}

impl Acc {
    fn close(&mut self) {
        if let Some(e) = self.cur.take() {
            self.worst.push(e);
            self.worst.sort_by(|a, b| b.score().total_cmp(&a.score()));
            self.worst.truncate(KEEP_WORST);
        }
    }
}

struct Track {
    out: Output,
    /// HA's values: the one in force at the window start, then every change after it.
    ha: VecDeque<(f64, Val)>,
    acc: Acc,
}

pub struct FieldComparator {
    tracks: Vec<Track>,
    by_entity: HashMap<&'static str, Vec<usize>>,
    tol_s: f64,
    /// The field whose disagreeing samples are traced, and the trace.
    trace_field: Option<String>,
    pub trace: Vec<TraceLine>,
    /// Rust samples not judged yet: `(t, values, compared)`.
    pending: VecDeque<(f64, Vec<Val>, bool)>,
}

impl FieldComparator {
    pub fn new(tol_s: f64, trace_field: Option<String>) -> Self {
        let tracks: Vec<Track> =
            outputs().into_iter().map(|out| Track { out, ha: VecDeque::new(), acc: Acc::default() }).collect();
        let mut by_entity: HashMap<&'static str, Vec<usize>> = HashMap::new();
        for (i, t) in tracks.iter().enumerate() {
            by_entity.entry(t.out.entity).or_default().push(i);
        }
        FieldComparator { tracks, by_entity, tol_s, trace_field, trace: Vec::new(), pending: VecDeque::new() }
    }

    /// The entities this comparator reads.
    pub fn entities(&self) -> impl Iterator<Item = &&'static str> {
        self.by_entity.keys()
    }

    /// HA wrote `e` at `ts`.
    pub fn ha_row(&mut self, ts: f64, e: &EntityState) {
        let Some(idx) = self.by_entity.get(e.entity_id.as_str()) else { return };
        for &i in idx {
            let t = &mut self.tracks[i];
            // The recorder does not store some attributes (`next_reset`): no HA value at all.
            if t.out.attr.is_some_and(|a| e.attr(a).is_none()) {
                continue;
            }
            let v = t.out.ha_val(Some(e)).expect("entity given");
            if t.ha.back().is_none_or(|(_, last)| *last != v) {
                t.ha.push_back((ts, v));
            }
        }
    }

    /// Rust's derived values at `t`; `compared` is false inside a settle window. Samples
    /// up to `t − tol` are judged (every HA row up to `t` has been applied).
    pub fn rust(&mut self, t: f64, d: &Derived, compared: bool) {
        let json = serde_json::to_value(d).expect("Derived serialises");
        let vals = self.tracks.iter().map(|tr| tr.out.rust_val(json.get(tr.out.field))).collect();
        self.pending.push_back((t, vals, compared));
        while self.pending.front().is_some_and(|(at, _, _)| *at <= t - self.tol_s + 1e-9) {
            let s = self.pending.pop_front().expect("front exists");
            self.judge(s);
        }
    }

    /// Judge what is left (end of the replay).
    pub fn finish(&mut self) {
        while let Some(s) = self.pending.pop_front() {
            self.judge(s);
        }
        for t in &mut self.tracks {
            t.acc.close();
        }
    }

    fn judge(&mut self, (t, vals, compared): (f64, Vec<Val>, bool)) {
        let (lo, hi) = (t - self.tol_s, t + self.tol_s);
        for (tr, rust) in self.tracks.iter_mut().zip(vals) {
            // Keep only the value in force at `lo` and what came after.
            while tr.ha.len() > 1 && tr.ha[1].0 <= lo {
                tr.ha.pop_front();
            }
            let acc = &mut tr.acc;
            let window: Vec<&Val> = tr.ha.iter().filter(|(ts, _)| *ts <= hi).map(|(_, v)| v).collect();
            if window.is_empty() {
                acc.missing += 1;
                continue;
            }
            if !compared {
                acc.skipped += 1;
                acc.close();
                continue;
            }
            acc.samples += 1;
            let best = window.iter().filter_map(|h| rust.err(h)).reduce(f64::min);
            if let Some(e) = best {
                acc.err_sum += e;
                acc.err_n += 1;
                if acc.max_err.is_none_or(|(m, _)| e > m) {
                    acc.max_err = Some((e, t));
                }
            }
            if window.iter().any(|h| rust.matches(h, tr.out.tol)) {
                acc.matches += 1;
                acc.close();
                continue;
            }
            // The HA value closest to Rust's (or the one in force at `t`).
            let ha = window
                .iter()
                .min_by(|a, b| rust.err(a).unwrap_or(f64::MAX).total_cmp(&rust.err(b).unwrap_or(f64::MAX)))
                .map(|v| (*v).clone())
                .expect("window not empty");
            if self.trace_field.as_deref() == Some(tr.out.field) && self.trace.len() < KEEP_TRACE {
                let ha = window.iter().map(|v| (*v).clone()).collect();
                self.trace.push(TraceLine { at: fmt_ts(t), rust: rust.clone(), ha });
            }
            match &mut acc.cur {
                Some(ep) => ep.extend(t, rust, ha, best),
                None => {
                    acc.episodes += 1;
                    acc.cur = Some(Episode::new(t, rust, ha, best));
                }
            }
        }
    }

    /// Per-field results, worst agreement first (never compared last).
    pub fn report(&self) -> Vec<FieldReport> {
        let mut v: Vec<FieldReport> = self
            .tracks
            .iter()
            .map(|t| {
                let a = &t.acc;
                FieldReport {
                    field: t.out.field,
                    entity: t.out.entity,
                    samples: a.samples,
                    matches: a.matches,
                    agreement_pct: crate::shadow::pct(a.matches, a.samples),
                    missing: a.missing,
                    skipped: a.skipped,
                    mean_abs_err: (a.err_n > 0).then(|| a.err_sum / a.err_n as f64),
                    max_abs_err: a.max_err.map(|(e, _)| e),
                    max_err_at: a.max_err.map(|(_, t)| fmt_ts(t)),
                    mismatch_episodes: a.episodes,
                    worst: a.worst.clone(),
                }
            })
            .collect();
        v.sort_by(|a, b| {
            let k = |r: &FieldReport| r.agreement_pct.unwrap_or(f64::INFINITY);
            k(a).total_cmp(&k(b)).then(a.field.cmp(b.field))
        });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setpoint(v: &str) -> EntityState {
        EntityState::new("sensor.batterij_setpoint", v)
    }

    fn d(w: f64) -> Derived {
        Derived { batterij_setpoint_w: w, ..Default::default() }
    }

    fn field<'a>(r: &'a [FieldReport], f: &str) -> &'a FieldReport {
        r.iter().find(|x| x.field == f).unwrap()
    }

    /// HA changing 3 s after Rust still agrees (tolerance 5 s); 8 s after does not, and the
    /// run of disagreeing samples is one episode with its worst error.
    #[test]
    fn time_tolerance_and_episodes() {
        let mut c = FieldComparator::new(5.0, None);
        c.ha_row(0.0, &setpoint("0"));
        // Rust goes to −1000 at t=10; HA follows at t=13.
        for t in 0..30 {
            if t == 13 {
                c.ha_row(13.2, &setpoint("-1000"));
            }
            c.rust(f64::from(t), &d(if t >= 10 { -1000.0 } else { 0.0 }), true);
        }
        // Rust goes to 500 at t=40; HA follows at t=48.
        for t in 30..60 {
            if t == 48 {
                c.ha_row(48.0, &setpoint("500"));
            }
            c.rust(f64::from(t), &d(if t >= 40 { 500.0 } else { -1000.0 }), true);
        }
        c.finish();
        let r = c.report();
        let f = field(&r, "batterij_setpoint_w");
        assert_eq!((f.samples, f.missing, f.skipped), (60, 0, 0));
        // 40, 41, 42 see only −1000 within ±5 s (HA's 500 arrives at 48).
        assert_eq!(f.matches, 57);
        assert_eq!(f.mismatch_episodes, 1);
        assert_eq!(f.worst.len(), 1);
        assert_eq!(f.worst[0].duration_s, 2.0);
        assert_eq!(f.worst[0].err, Some(1500.0));
        assert_eq!(f.max_abs_err, Some(1500.0));
        assert_eq!(f.max_err_at.as_deref(), Some("1970-01-01 01:00:40"));
        assert!((f.mean_abs_err.unwrap() - 4500.0 / 60.0).abs() < 1e-9);
        // Fields HA never showed are missing, not mismatches; they sort last.
        let k = field(&r, "kwartier_index");
        assert_eq!((k.samples, k.missing, k.agreement_pct), (0, 60, None));
        assert_eq!(r.last().unwrap().agreement_pct, None);
        assert_eq!(r[0].field, "batterij_setpoint_w");
    }

    /// Samples in a settle window are skipped and close an open episode; non-numeric fields
    /// keep the longest episodes; an unchanged HA row adds no value.
    #[test]
    fn skipped_samples_and_text_fields() {
        let mut c = FieldComparator::new(0.0, None);
        let car = |s: &str| EntityState::new("sensor.ev_actieve_laadauto", s);
        c.ha_row(0.0, &car("EV3"));
        c.ha_row(0.5, &car("EV3"));
        let buzz = Derived::default(); // ev_actieve_laadauto defaults to Buzz
        c.rust(0.0, &buzz, true);
        c.rust(1.0, &buzz, true);
        c.rust(2.0, &buzz, false);
        c.rust(3.0, &buzz, true);
        c.finish();
        let r = c.report();
        let f = field(&r, "ev_actieve_laadauto");
        assert_eq!((f.samples, f.matches, f.skipped, f.mismatch_episodes), (3, 0, 1, 2));
        assert_eq!(f.worst[0].duration_s, 1.0);
        assert_eq!(f.worst[0].ha, Val::Text("ev3".into()));
        assert_eq!(f.mean_abs_err, None);
        assert_eq!(c.tracks.iter().find(|t| t.out.field == "ev_actieve_laadauto").unwrap().ha.len(), 1);
        assert!(c.entities().any(|e| *e == "sensor.batterij_setpoint"));
        // Unknown entities are ignored.
        c.ha_row(4.0, &EntityState::new("sensor.nope", "1"));
    }

    /// More than five episodes keep the five worst.
    #[test]
    fn keeps_the_worst_episodes() {
        let mut c = FieldComparator::new(0.0, None);
        c.ha_row(0.0, &setpoint("0"));
        for i in 0..8 {
            c.rust(f64::from(2 * i), &d(f64::from(100 * (i + 1))), true);
            c.rust(f64::from(2 * i + 1), &d(0.0), true);
        }
        c.finish();
        let r = c.report();
        let f = field(&r, "batterij_setpoint_w");
        assert_eq!(f.mismatch_episodes, 8);
        let errs: Vec<_> = f.worst.iter().map(|e| e.err.unwrap()).collect();
        assert_eq!(errs, vec![800.0, 700.0, 600.0, 500.0, 400.0]);
    }
}
