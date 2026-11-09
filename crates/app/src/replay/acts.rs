//! Actuation parity over a replay, per actuator key (`evse.override_a`, `solis.rc_mode`,
//! `latch.batterij_net_laden`, …; see `shadow::commands`).
//!
//! HA's side is the **value timeline** of the output entity (recorded state changes;
//! `unavailable` is a gap, not a value). HA's automations also send redundant `set_value`
//! calls that leave no trace, so nothing is matched on HA's *calls*.
//!
//! Rust's side:
//! - devices: its commands. Its **intended value** is the last value it commanded; an HA
//!   change it did not ask for becomes its intent after the match window when it sends nothing
//!   else in that window (in lockstep it sees HA's value, and staying silent for a whole
//!   window — every controller runs at least every 30 s — means it is content with it);
//! - latches (Memory helpers): its after-tick value each step, and its flips as acts.
//!
//! Reported per key:
//! - **timeline agreement**: % of seconds (both sides known, outside gaps/settle) where
//!   Rust's intended value matches a value HA showed within ±`tol_s`;
//! - **HA changes** matched by a Rust act with the same value within ±window;
//! - **Rust acts** matched by HA showing that value within ±window, and the subset that are
//!   **edges** (a value different from Rust's intent just before; re-sends are not edges);
//! - the unmatched ones as examples (Rust re-sends of one unmatched value are one episode).

use std::collections::BTreeMap;

use serde::Serialize;

use super::fields::TraceLine;
use super::{Spans, fmt_ts};
use crate::config::Tolerance;
use crate::shadow::Val;
use crate::shadow::commands::Act;

/// Examples kept per key and side.
pub const KEEP_EXAMPLES: usize = 8;

/// Value tolerance per key: tenths of an amp for the override, whole amps for the cable,
/// watts for the Solis powers and the bias.
pub fn key_tol(key: &str) -> Tolerance {
    let abs = match key {
        "evse.override_a" => 0.05,
        "cable.current_a" | "solis.rc_timeout_min" | "latch.batterij_setpoint_bias" => 0.5,
        k if k.ends_with("_w") => 5.0,
        _ => 0.0,
    };
    Tolerance { abs, rel: 0.0 }
}

/// Keys whose Rust side is Memory, not a command.
pub fn is_latch(key: &str) -> bool {
    key.starts_with("latch.")
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActExample {
    pub at: String,
    /// The last act of a run of identical unmatched acts (Rust re-sends).
    pub until: String,
    pub count: u64,
    pub val: Val,
    /// The other side at `at`: for a Rust act HA's value, for an HA change Rust's intended
    /// value (`null`: unknown).
    pub other: Val,
    #[serde(skip)]
    until_ts: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActReport {
    pub key: &'static str,
    pub ha_changes: u64,
    pub ha_changes_matched: u64,
    pub rust_acts: u64,
    pub rust_acts_matched: u64,
    pub rust_edges: u64,
    pub rust_edges_matched: u64,
    /// Seconds where both sides had a value.
    pub timeline_s: u64,
    pub timeline_agree_s: u64,
    pub timeline_agreement_pct: Option<f64>,
    /// Edge agreement: matched (HA changes + Rust edges) over all of them.
    pub edge_agreement_pct: Option<f64>,
    /// HA changes Rust never asked for.
    pub ha_only: Vec<ActExample>,
    pub ha_only_total: u64,
    /// Rust acts HA never showed (runs of re-sends folded).
    pub rust_only: Vec<ActExample>,
    pub rust_only_total: u64,
}

/// A step function: `(from, value)`, `None` = unknown.
type Timeline = Vec<(f64, Option<Val>)>;

/// The value in force at `t`.
fn at(tl: &Timeline, t: f64) -> Option<&Val> {
    let i = tl.partition_point(|(ts, _)| *ts <= t);
    i.checked_sub(1).and_then(|i| tl[i].1.as_ref())
}

/// Every known value in force at some moment of `[a, b]`.
fn during(tl: &Timeline, a: f64, b: f64) -> impl Iterator<Item = &Val> {
    let i = tl.partition_point(|(ts, _)| *ts <= a).saturating_sub(1);
    tl[i..].iter().take_while(move |(ts, _)| *ts <= b).filter_map(|(_, v)| v.as_ref())
}

fn push_tl(tl: &mut Timeline, t: f64, v: Option<Val>) {
    if tl.last().is_none_or(|(_, last)| *last != v) {
        tl.push((t, v));
    }
}

#[derive(Default)]
struct Key {
    ha: Timeline,
    /// HA's changes from one known value to another: `(t, value)`.
    ha_changes: Vec<(f64, Val)>,
    rust: Vec<(f64, Val)>,
    /// Latches: Rust's after-tick value.
    rust_tl: Timeline,
}

pub struct ActComparator {
    keys: BTreeMap<&'static str, Key>,
    window_s: f64,
    tol_s: f64,
}

impl ActComparator {
    pub fn new(window_s: f64, tol_s: f64) -> Self {
        ActComparator { keys: BTreeMap::new(), window_s, tol_s }
    }

    /// HA's entity now reads `act` (`None` with the key when it became unreadable).
    pub fn ha_state(&mut self, ts: f64, key: &'static str, act: Option<Val>) {
        let k = self.keys.entry(key).or_default();
        let prev = k.ha.last().and_then(|(_, v)| v.clone());
        if let (Some(p), Some(v)) = (&prev, &act)
            && p != v
        {
            k.ha_changes.push((ts, v.clone()));
        }
        push_tl(&mut k.ha, ts, act);
    }

    /// A Rust command or latch flip at `ts`.
    pub fn rust_act(&mut self, ts: f64, act: Act) {
        self.keys.entry(act.key).or_default().rust.push((ts, act.val));
    }

    /// A latch's after-tick value at `ts`.
    pub fn rust_latch(&mut self, ts: f64, key: &'static str, v: Val) {
        push_tl(&mut self.keys.entry(key).or_default().rust_tl, ts, Some(v));
    }

    /// Rust's intended value of a device key (see the module docs).
    fn device_intent(&self, k: &Key) -> Timeline {
        let mut ev: Vec<(f64, bool, &Val)> = k.rust.iter().map(|(t, v)| (*t, true, v)).collect();
        for (t, v) in &k.ha_changes {
            let adopt = t + self.window_s;
            if !k.rust.iter().any(|(rt, _)| *rt > *t && *rt <= adopt) {
                ev.push((adopt, false, v));
            }
        }
        ev.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut tl = Timeline::new();
        for (t, _, v) in ev {
            push_tl(&mut tl, t, Some(v.clone()));
        }
        tl
    }

    /// Every Rust act and HA change of `key`, in time order (`--trace <key>`): a Rust act has
    /// its value in `rust`, an HA change in `ha`.
    pub fn trace(&self, key: &str) -> Vec<TraceLine> {
        let Some(k) = self.keys.get(key) else { return Vec::new() };
        let mut v: Vec<(f64, TraceLine)> = k
            .rust
            .iter()
            .map(|(t, val)| (*t, TraceLine { at: fmt_ts(*t), rust: val.clone(), ha: vec![] }))
            .chain(k.ha.iter().map(|(t, val)| {
                (*t, TraceLine { at: fmt_ts(*t), rust: Val::Null, ha: vec![val.clone().unwrap_or(Val::Null)] })
            }))
            .collect();
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        v.into_iter().map(|(_, l)| l).collect()
    }

    /// Results per key, over `[start, end]` minus the `skip` spans; worst timeline first.
    pub fn report(&self, start: f64, end: f64, skip: &Spans) -> Vec<ActReport> {
        let w = self.window_s;
        let mut out = Vec::new();
        for (&key, k) in &self.keys {
            let tol = key_tol(key);
            let intent = if is_latch(key) { k.rust_tl.clone() } else { self.device_intent(k) };
            let mut r = ActReport {
                key,
                ha_changes: 0,
                ha_changes_matched: 0,
                rust_acts: 0,
                rust_acts_matched: 0,
                rust_edges: 0,
                rust_edges_matched: 0,
                timeline_s: 0,
                timeline_agree_s: 0,
                timeline_agreement_pct: None,
                edge_agreement_pct: None,
                ha_only: Vec::new(),
                ha_only_total: 0,
                rust_only: Vec::new(),
                rust_only_total: 0,
            };
            let ha_now = |t: f64| at(&k.ha, t).cloned().unwrap_or(Val::Null);
            for (t, v) in k.ha_changes.iter().filter(|(t, _)| !skip.contains(*t)) {
                r.ha_changes += 1;
                if k.rust.iter().any(|(rt, rv)| (rt - t).abs() <= w && rv.matches(v, tol)) {
                    r.ha_changes_matched += 1;
                } else {
                    let rust = at(&intent, *t).cloned().unwrap_or(Val::Null);
                    fold(&mut r.ha_only, &mut r.ha_only_total, *t, v, rust, w);
                }
            }
            for (t, v) in k.rust.iter().filter(|(t, _)| !skip.contains(*t)) {
                r.rust_acts += 1;
                let edge = !at(&intent, t - 1e-6).is_some_and(|p| p.matches(v, tol));
                let shown = during(&k.ha, t - w, t + w).any(|h| v.matches(h, tol));
                r.rust_edges += u64::from(edge);
                if shown {
                    r.rust_acts_matched += 1;
                    r.rust_edges_matched += u64::from(edge);
                } else {
                    fold(&mut r.rust_only, &mut r.rust_only_total, *t, v, ha_now(*t), w);
                }
            }
            let mut t = start.ceil();
            while t <= end {
                if !skip.contains(t)
                    && let Some(rv) = at(&intent, t)
                {
                    let mut ha = during(&k.ha, t - self.tol_s, t + self.tol_s).peekable();
                    if ha.peek().is_some() {
                        r.timeline_s += 1;
                        r.timeline_agree_s += u64::from(ha.any(|h| rv.matches(h, tol)));
                    }
                }
                t += 1.0;
            }
            r.timeline_agreement_pct = crate::shadow::pct(r.timeline_agree_s, r.timeline_s);
            r.edge_agreement_pct =
                crate::shadow::pct(r.ha_changes_matched + r.rust_edges_matched, r.ha_changes + r.rust_edges);
            out.push(r);
        }
        out.sort_by(|a, b| {
            let k = |r: &ActReport| r.timeline_agreement_pct.or(r.edge_agreement_pct).unwrap_or(f64::INFINITY);
            k(a).total_cmp(&k(b)).then(a.key.cmp(b.key))
        });
        out
    }
}

/// Add an unmatched act to the examples, folding it into the previous one when it repeats
/// the same value within the window.
fn fold(ex: &mut Vec<ActExample>, total: &mut u64, t: f64, v: &Val, other: Val, w: f64) {
    *total += 1;
    if let Some(last) = ex.last_mut()
        && last.val == *v
        && t - last.until_ts <= w
    {
        last.until = fmt_ts(t);
        last.until_ts = t;
        last.count += 1;
        return;
    }
    if ex.len() < KEEP_EXAMPLES {
        ex.push(ActExample { at: fmt_ts(t), until: fmt_ts(t), count: 1, val: v.clone(), other, until_ts: t });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(v: f64) -> Option<Val> {
        Some(Val::Num(v))
    }

    fn act(key: &'static str, v: f64) -> Act {
        Act { key, val: Val::Num(v) }
    }

    const O: &str = "evse.override_a";

    fn get<'a>(r: &'a [ActReport], k: &str) -> &'a ActReport {
        r.iter().find(|x| x.key == k).unwrap()
    }

    /// Rust commands 15.2 at 100, HA shows it at 102: one matched edge, one matched change.
    /// Re-sends at 130 and 160 are matched acts but not edges. Rust asks 16 at 200 and HA
    /// never follows: a Rust-only episode and timeline disagreement until HA's own change
    /// to 10 at 300 is adopted 40 s later (Rust stays silent).
    #[test]
    fn edges_resends_and_timeline() {
        let mut c = ActComparator::new(40.0, 5.0);
        c.ha_state(0.0, O, num(14.8));
        c.rust_act(100.0, act(O, 15.2));
        c.ha_state(102.0, O, num(15.2));
        c.rust_act(130.0, act(O, 15.2));
        c.rust_act(160.0, act(O, 15.2));
        c.rust_act(200.0, act(O, 16.0));
        c.rust_act(230.0, act(O, 16.0));
        c.ha_state(300.0, O, num(10.0));
        c.ha_state(350.0, O, None);
        c.ha_state(360.0, O, num(10.0));
        let r = c.report(0.0, 400.0, &Spans::default());
        let k = get(&r, O);
        assert_eq!((k.ha_changes, k.ha_changes_matched), (2, 1));
        assert_eq!((k.rust_acts, k.rust_acts_matched), (5, 3));
        assert_eq!((k.rust_edges, k.rust_edges_matched), (2, 1));
        assert_eq!(k.rust_only_total, 2);
        assert_eq!(k.rust_only.len(), 1, "the re-send folds into the first");
        assert_eq!(k.rust_only[0].count, 2);
        assert_eq!(k.rust_only[0].other, Val::Num(15.2));
        assert_eq!(k.rust_only[0].at, "1970-01-01 01:03:20");
        assert_eq!(k.ha_only_total, 1);
        assert_eq!(k.ha_only[0].val, Val::Num(10.0));
        assert_eq!(k.ha_only[0].other, Val::Num(16.0), "Rust still wanted 16");
        // Intent: 15.2 from 100, 16 from 200, HA's 10 adopted at 340. HA is known at every
        // second within ±5 s (the 350–360 dropout is bridged by the window), so 100..=400
        // counts: 100..=199 agree, 200..=339 never see 16, 340..=400 agree.
        assert_eq!(k.timeline_s, 301);
        assert_eq!(k.timeline_agree_s, 100 + 61);
        assert_eq!(k.edge_agreement_pct, Some(50.0));
    }

    /// Latches use Rust's after-tick timeline; spans are skipped; HA changes Rust never made
    /// are listed; an HA change straight after a Rust act is no edge disagreement.
    #[test]
    fn latches_and_skipped_spans() {
        let mut c = ActComparator::new(40.0, 0.0);
        let k = "latch.batterij_net_laden";
        c.ha_state(0.0, k, Some(Val::Bool(false)));
        for t in 0..20 {
            c.rust_latch(f64::from(t), k, Val::Bool(t >= 10));
        }
        c.rust_act(10.0, Act { key: k, val: Val::Bool(true) });
        c.ha_state(10.5, k, Some(Val::Bool(true)));
        let mut skip = Spans::default();
        skip.push(15.0, 17.0);
        let r = c.report(0.0, 19.0, &skip);
        let x = get(&r, k);
        assert_eq!((x.ha_changes, x.ha_changes_matched, x.rust_edges, x.rust_edges_matched), (1, 1, 1, 1));
        assert_eq!(x.timeline_s, 17);
        assert_eq!(x.timeline_agree_s, 16, "only t=10 disagrees (HA flips at 10.5)");
        // Nothing to compare: no percentages, sorted last.
        c.rust_act(5.0, act("solis.rc_charge_w", 1000.0));
        let r = c.report(100.0, 50.0, &skip);
        assert_eq!(r.last().unwrap().timeline_agreement_pct, None);
        assert_eq!(get(&r, "solis.rc_charge_w").rust_only_total, 1);
        assert_eq!(get(&r, "solis.rc_charge_w").rust_only[0].other, Val::Null);
    }

    /// The trace of a key lists both sides in time order; an unknown key has none.
    #[test]
    fn trace_lists_both_sides() {
        let mut c = ActComparator::new(40.0, 5.0);
        c.ha_state(5.0, O, num(6.0));
        c.ha_state(6.0, O, None);
        c.rust_act(1.0, act(O, 7.0));
        let t = c.trace(O);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].rust, Val::Num(7.0));
        assert_eq!(t[1].ha, vec![Val::Num(6.0)]);
        assert_eq!(t[2].ha, vec![Val::Null]);
        assert!(c.trace("evse.mode").is_empty());
    }

    /// Example lists stop at `KEEP_EXAMPLES` but the totals keep counting.
    #[test]
    fn examples_are_bounded() {
        let mut c = ActComparator::new(1.0, 0.0);
        c.ha_state(0.0, O, num(6.0));
        for i in 0..20 {
            c.rust_act(f64::from(i * 10), act(O, f64::from(7 + i)));
        }
        let r = c.report(0.0, 0.0, &Spans::default());
        assert_eq!(get(&r, O).rust_only.len(), KEEP_EXAMPLES);
        assert_eq!(get(&r, O).rust_only_total, 20);
    }

    #[test]
    fn tolerances() {
        assert_eq!(key_tol("evse.override_a").abs, 0.05);
        assert_eq!(key_tol("cable.current_a").abs, 0.5);
        assert_eq!(key_tol("solis.rc_charge_w").abs, 5.0);
        assert_eq!(key_tol("evse.mode").abs, 0.0);
        assert!(is_latch("latch.x") && !is_latch("evse.mode"));
    }
}
