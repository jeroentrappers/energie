//! Per-field parity of `Derived` against HA's template sensors.
//!
//! Every tick a sample (Rust's values, HA's values) is recorded. A sample is judged `N`
//! seconds later (`time_tolerance_s`): HA's value at `t` matches if any Rust value in
//! `[t − N, t + N]` is within the field's tolerance. That absorbs HA updating a template a few
//! seconds before or after Rust, without hiding a real disagreement.

use std::collections::VecDeque;

use energie_core::derived::Derived;
use energie_core::time::{Ts, secs};
use serde::Serialize;
use serde_json::Value;

use super::Val;
use super::log::MismatchLog;
use crate::config::ShadowConfig;
use crate::ha::entity::Lookup;
use crate::ha::outputs::{Output, outputs};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Mismatch {
    pub at: String,
    pub rust: Val,
    pub ha: Val,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldStats {
    pub field: &'static str,
    pub entity: &'static str,
    /// Samples where HA had the entity.
    pub samples: u64,
    pub matches: u64,
    /// Samples where HA did not have the entity at all.
    pub missing: u64,
    pub agreement_pct: Option<f64>,
    /// Largest remaining numeric difference after the time tolerance.
    pub max_err: Option<f64>,
    pub last_mismatch: Option<Mismatch>,
    /// The latest judged sample disagreed.
    pub mismatching: bool,
}

struct Sample {
    at: Ts,
    rust: Vec<Val>,
    ha: Vec<Option<Val>>,
    judged: bool,
}

pub struct FieldComparator {
    outputs: Vec<Output>,
    window_s: f64,
    history: VecDeque<Sample>,
    stats: Vec<FieldStats>,
}

impl FieldComparator {
    pub fn new(cfg: &ShadowConfig) -> Self {
        let mut outputs = outputs();
        for o in &mut outputs {
            if let Some(t) = cfg.tolerances.get(o.field) {
                o.tol = *t;
            }
        }
        let stats = outputs
            .iter()
            .map(|o| FieldStats {
                field: o.field,
                entity: o.entity,
                samples: 0,
                matches: 0,
                missing: 0,
                agreement_pct: None,
                max_err: None,
                last_mismatch: None,
                mismatching: false,
            })
            .collect();
        FieldComparator { outputs, window_s: cfg.time_tolerance_s as f64, history: VecDeque::new(), stats }
    }

    pub fn observe(&mut self, now: Ts, d: &Derived, ha: &dyn Lookup, log: &mut MismatchLog) {
        let json = serde_json::to_value(d).expect("Derived serialises");
        let rust = self.outputs.iter().map(|o| o.rust_val(json.get(o.field))).collect();
        let hav = self.outputs.iter().map(|o| o.ha_val(ha.get(o.entity))).collect();
        self.history.push_back(Sample { at: now, rust, ha: hav, judged: false });
        for i in 0..self.history.len() {
            let s = &self.history[i];
            if !s.judged && secs(s.at, now) >= self.window_s {
                self.judge(i, log);
                self.history[i].judged = true;
            }
        }
        while self.history.front().is_some_and(|s| s.judged && secs(s.at, now) > 2.0 * self.window_s) {
            self.history.pop_front();
        }
    }

    fn judge(&mut self, i: usize, log: &mut MismatchLog) {
        let at = self.history[i].at;
        let window: Vec<&Sample> =
            self.history.iter().filter(|s| secs(s.at, at).abs() <= self.window_s + 1e-9).collect();
        for (f, o) in self.outputs.iter().enumerate() {
            let st = &mut self.stats[f];
            let Some(ha) = &self.history[i].ha[f] else {
                st.missing += 1;
                continue;
            };
            st.samples += 1;
            let matched = window.iter().any(|s| s.rust[f].matches(ha, o.tol));
            let best = window.iter().filter_map(|s| s.rust[f].err(ha)).reduce(f64::min);
            if let Some(e) = best {
                st.max_err = Some(st.max_err.map_or(e, |m| m.max(e)));
            }
            if matched {
                st.matches += 1;
                st.mismatching = false;
            } else {
                let m = Mismatch { at: at.to_rfc3339(), rust: self.history[i].rust[f].clone(), ha: ha.clone() };
                if !st.mismatching {
                    log.write(&serde_json::json!({
                        "kind": "field", "at": m.at, "field": o.field, "entity": o.entity,
                        "rust": m.rust, "ha": m.ha, "err": best,
                    }));
                }
                st.last_mismatch = Some(m);
                st.mismatching = true;
            }
            st.agreement_pct = super::pct(st.matches, st.samples);
        }
    }

    /// Per-field stats, worst agreement first (fields never compared last).
    pub fn stats(&self) -> Vec<FieldStats> {
        let mut v = self.stats.clone();
        v.sort_by(|a, b| {
            let ka = a.agreement_pct.unwrap_or(f64::INFINITY);
            let kb = b.agreement_pct.unwrap_or(f64::INFINITY);
            ka.total_cmp(&kb).then(a.field.cmp(b.field))
        });
        v
    }

    /// Stats of one field.
    pub fn field(&self, name: &str) -> Option<&FieldStats> {
        self.stats.iter().find(|s| s.field == name)
    }
}

/// `Derived` as a JSON object (for callers that only want a field or two).
pub fn derived_json(d: &Derived) -> Value {
    serde_json::to_value(d).expect("Derived serialises")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Tolerance;
    use crate::ha::entity::{EntityState, index};
    use crate::plug::tests::t0;
    use chrono::Duration;
    use std::collections::HashMap;

    /// HA showing a setpoint (and a binary sensor, so non-numeric fields are compared too).
    fn ha_setpoint(v: &str) -> HashMap<String, EntityState> {
        index(vec![
            EntityState::new("sensor.batterij_setpoint", v),
            EntityState::new("binary_sensor.capaciteitspiek_risico", "off"),
        ])
    }

    fn derived_setpoint(w: f64) -> Derived {
        Derived { batterij_setpoint_w: w, ..Default::default() }
    }

    fn cmp(window: u64) -> (FieldComparator, MismatchLog, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ShadowConfig { time_tolerance_s: window, ..Default::default() };
        let log = MismatchLog::new(Some(dir.path().join("m.jsonl")), 1 << 20);
        (FieldComparator::new(&cfg), log, dir)
    }

    /// Equal values within tolerance agree; a sample is judged only after the time window.
    #[test]
    fn agreeing_values_count_as_matches_after_the_window() {
        let (mut c, mut log, _d) = cmp(2);
        for i in 0..5 {
            c.observe(t0() + Duration::seconds(i), &derived_setpoint(-1203.0), &ha_setpoint("-1200"), &mut log);
        }
        let s = c.field("batterij_setpoint_w").unwrap();
        assert_eq!((s.samples, s.matches), (3, 3));
        assert_eq!(s.agreement_pct, Some(100.0));
        assert_eq!(s.max_err, Some(3.0));
        assert!(!s.mismatching);
        assert_eq!(log.lines, 0);
    }

    /// HA updating a template a couple of seconds after Rust still matches within ±N s.
    #[test]
    fn ha_lagging_rust_by_less_than_the_window_matches() {
        let (mut c, mut log, _d) = cmp(3);
        // Rust jumps to -2000 at t=5; HA follows at t=7.
        for i in 0..15 {
            let rust = if i >= 5 { -2000.0 } else { 0.0 };
            let ha = if i >= 7 { "-2000" } else { "0" };
            c.observe(t0() + Duration::seconds(i), &derived_setpoint(rust), &ha_setpoint(ha), &mut log);
        }
        let s = c.field("batterij_setpoint_w").unwrap();
        assert_eq!(s.samples, s.matches, "{s:?}");
    }

    /// A real disagreement is counted, its onset logged once, and the error recorded.
    #[test]
    fn a_persistent_disagreement_is_counted_and_logged_once() {
        let (mut c, mut log, dir) = cmp(1);
        for i in 0..6 {
            c.observe(t0() + Duration::seconds(i), &derived_setpoint(-500.0), &ha_setpoint("-1500"), &mut log);
        }
        // Back in agreement, then a second onset.
        c.observe(t0() + Duration::seconds(6), &derived_setpoint(-1500.0), &ha_setpoint("-1500"), &mut log);
        for i in 7..12 {
            c.observe(t0() + Duration::seconds(i), &derived_setpoint(-1500.0), &ha_setpoint("-1500"), &mut log);
        }
        for i in 12..16 {
            c.observe(t0() + Duration::seconds(i), &derived_setpoint(0.0), &ha_setpoint("-1500"), &mut log);
        }
        let s = c.field("batterij_setpoint_w").unwrap();
        assert!(s.matches > 0 && s.matches < s.samples);
        assert_eq!(s.max_err, Some(1500.0));
        assert!(s.mismatching);
        let m = s.last_mismatch.as_ref().unwrap();
        assert_eq!((m.rust.clone(), m.ha.clone()), (Val::Num(0.0), Val::Num(-1500.0)));
        assert_eq!(log.lines, 2);
        let text = std::fs::read_to_string(dir.path().join("m.jsonl")).unwrap();
        assert!(text.lines().next().unwrap().contains("\"field\":\"batterij_setpoint_w\""));
        // The worst field sorts first.
        assert_eq!(c.stats()[0].field, "batterij_setpoint_w");
    }

    /// Entities HA does not have are counted as missing, not as disagreement.
    #[test]
    fn missing_entities_are_not_mismatches() {
        let (mut c, mut log, _d) = cmp(0);
        c.observe(t0(), &Derived::default(), &index(vec![]), &mut log);
        let s = c.field("batterij_setpoint_w").unwrap();
        assert_eq!((s.samples, s.missing), (0, 1));
        assert_eq!(s.agreement_pct, None);
    }

    /// A per-field tolerance from the config replaces the default.
    #[test]
    fn configured_tolerance_overrides_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = ShadowConfig { time_tolerance_s: 0, ..Default::default() };
        cfg.tolerances.insert("batterij_setpoint_w".into(), Tolerance { abs: 100.0, rel: 0.0 });
        let mut c = FieldComparator::new(&cfg);
        let mut log = MismatchLog::new(Some(dir.path().join("m")), 1 << 20);
        c.observe(t0(), &derived_setpoint(-1250.0), &ha_setpoint("-1200"), &mut log);
        assert_eq!(c.field("batterij_setpoint_w").unwrap().matches, 1);
        assert!(c.field("nope").is_none());
        assert_eq!(derived_json(&Derived::default())["batterij_setpoint_w"], 0.0);
    }

    /// Old judged samples are dropped, so memory stays bounded.
    #[test]
    fn history_is_bounded() {
        let (mut c, mut log, _d) = cmp(2);
        for i in 0..100 {
            c.observe(t0() + Duration::seconds(i), &Derived::default(), &index(vec![]), &mut log);
        }
        assert!(c.history.len() <= 6, "{}", c.history.len());
    }
}
