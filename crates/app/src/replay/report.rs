//! The replay's results: a JSON report and a markdown summary ranking the worst-agreeing
//! fields and actuators, with timestamps to look up in HA's history.

use std::collections::BTreeMap;
use std::fmt::Write;

use serde::Serialize;

use super::Gap;
use super::acts::{ActExample, ActReport};
use super::fields::{FieldReport, TraceLine};
use super::probe::{ModeReport, ProbeReport};
use crate::ha::mapping::MeterAdoption;
use crate::shadow::Val;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub history: String,
    pub from: String,
    pub to: String,
    pub step_ms: u64,
    pub lockstep: bool,
    pub steps: u64,
    pub rows: u64,
    pub elapsed_s: f64,
    pub gaps: Vec<Gap>,
    pub meter_adoptions: u64,
    pub meter_adoptions_by_entity: BTreeMap<&'static str, u64>,
    /// The first 100 adoptions: when, which meter, Rust's and HA's value.
    pub meter_adoption_events: Vec<MeterAdoption>,
    /// Matching samples over all compared samples, %.
    pub field_agreement_pct: Option<f64>,
    /// Worst first.
    pub fields: Vec<FieldReport>,
    /// Worst first.
    pub actuators: Vec<ActReport>,
    /// The Buzz override: HA's changes against Rust's target on the same state.
    pub buzz_target: ProbeReport,
    /// The Buzz mode: HA's changes against what `ev_laden_buzz` would select.
    pub buzz_mode: ModeReport,
    /// `--trace`: the traced field's disagreeing samples.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trace: Vec<TraceLine>,
}

/// Fields listed with their worst episodes.
const DETAIL_FIELDS: usize = 15;

fn pct(p: Option<f64>) -> String {
    p.map_or("–".into(), |p| format!("{p:.2} %"))
}

fn num(v: Option<f64>) -> String {
    v.map_or("–".into(), |v| format!("{v:.4}"))
}

fn val(v: &Val) -> String {
    match v {
        Val::Null => "unavailable".into(),
        Val::Num(n) => format!("{n}"),
        Val::Bool(b) => b.to_string(),
        Val::Text(s) => s.clone(),
    }
}

fn example(e: &ActExample, other: &str) -> String {
    let span = if e.count > 1 { format!("{} … {} (×{})", e.at, e.until, e.count) } else { e.at.clone() };
    format!("{span}: {} ({other} {})", val(&e.val), val(&e.other))
}

/// The human summary.
pub fn markdown(r: &Report) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# Replay {} → {}\n", r.from, r.to);
    let _ = writeln!(
        s,
        "{} steps of {} ms, {} rows, lockstep {}, {:.0} s. Field agreement {}. Meter adoptions: {}.\n",
        r.steps,
        r.step_ms,
        r.rows,
        if r.lockstep { "on" } else { "off" },
        r.elapsed_s,
        pct(r.field_agreement_pct),
        r.meter_adoptions
    );
    if !r.gaps.is_empty() {
        let _ = writeln!(s, "Gaps (HA down; skipped, engine restarted):\n");
        for g in &r.gaps {
            let _ = writeln!(s, "- {} → {} ({:.0} s)", g.from, g.to, g.seconds);
        }
        let _ = writeln!(s);
    }
    for (e, n) in &r.meter_adoptions_by_entity {
        let _ = writeln!(s, "- adopted `{e}` ×{n}");
    }
    for a in &r.meter_adoption_events {
        let _ = writeln!(s, "  - {} `{}`: Rust {} → HA {}", a.at, a.entity, num(a.rust), a.ha);
    }
    let b = &r.buzz_target;
    let _ = writeln!(
        s,
        "\n## Buzz override at HA's decisions\n\n{} HA changes: {} within 0.05 A, {} within 0.5 A, mean error {} A.\n",
        b.samples,
        pct(b.agreement_pct),
        pct(b.within_half_pct),
        num(b.mean_abs_err)
    );
    for w in &b.worst {
        let _ = writeln!(s, "- {}: HA {} vs Rust {}", w.at, w.ha, w.rust);
    }
    let m = &r.buzz_mode;
    let _ = writeln!(
        s,
        "\n{} HA mode changes: {} the option `ev_laden_buzz` would select on that state.\n",
        m.samples,
        pct(m.agreement_pct)
    );
    for e in &m.examples {
        let _ = writeln!(s, "- {}: HA {} vs Rust {}", e.at, e.ha, e.rust.as_deref().unwrap_or("(no change)"));
    }
    let _ = writeln!(s, "\n## Actuators (worst first)\n");
    let _ = writeln!(
        s,
        "| key | timeline | HA changes matched | Rust edges matched | Rust acts matched |\n| --- | --- | --- | --- | --- |"
    );
    for a in &r.actuators {
        let _ = writeln!(
            s,
            "| `{}` | {} of {} s | {}/{} | {}/{} | {}/{} |",
            a.key,
            pct(a.timeline_agreement_pct),
            a.timeline_s,
            a.ha_changes_matched,
            a.ha_changes,
            a.rust_edges_matched,
            a.rust_edges,
            a.rust_acts_matched,
            a.rust_acts
        );
    }
    for a in r.actuators.iter().filter(|a| !a.ha_only.is_empty() || !a.rust_only.is_empty()) {
        let _ = writeln!(s, "\n### `{}`\n", a.key);
        for e in &a.ha_only {
            let _ = writeln!(s, "- HA only: {}", example(e, "Rust"));
        }
        if a.ha_only_total > a.ha_only.len() as u64 {
            let _ = writeln!(s, "- … {} HA-only changes in all", a.ha_only_total);
        }
        for e in &a.rust_only {
            let _ = writeln!(s, "- Rust only: {}", example(e, "HA"));
        }
        if a.rust_only_total > a.rust_only.iter().map(|e| e.count).sum::<u64>() {
            let _ = writeln!(s, "- … {} Rust-only acts in all", a.rust_only_total);
        }
    }
    let _ = writeln!(s, "\n## Fields (worst first)\n");
    let _ = writeln!(
        s,
        "| field | entity | agreement | samples | mean err | max err | episodes |\n| --- | --- | --- | --- | --- | --- | --- |"
    );
    for f in r.fields.iter().filter(|f| f.samples > 0) {
        let _ = writeln!(
            s,
            "| `{}` | `{}` | {} | {} | {} | {} | {} |",
            f.field,
            f.entity,
            pct(f.agreement_pct),
            f.samples,
            num(f.mean_abs_err),
            num(f.max_abs_err),
            f.mismatch_episodes
        );
    }
    let never: Vec<&str> = r.fields.iter().filter(|f| f.samples == 0).map(|f| f.field).collect();
    if !never.is_empty() {
        let _ = writeln!(s, "\nNever compared (entity not in the export): {}", never.join(", "));
    }
    let _ = writeln!(s, "\n## Worst episodes\n");
    for f in r.fields.iter().filter(|f| !f.worst.is_empty()).take(DETAIL_FIELDS) {
        let _ = writeln!(s, "### `{}` ({})\n", f.field, pct(f.agreement_pct));
        for e in &f.worst {
            let _ = writeln!(
                s,
                "- {} → {} ({:.0} s), worst {}: Rust {} vs HA {}",
                e.start,
                e.end,
                e.duration_s,
                e.worst_at,
                val(&e.rust),
                val(&e.ha)
            );
        }
        let _ = writeln!(s);
    }
    s
}
