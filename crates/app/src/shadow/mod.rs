//! Shadow comparison: does Rust decide what the live HA decides?
//!
//! - [`fields`]: every `Derived` value against the HA entity that shows it, with a value
//!   tolerance and a time tolerance (HA template sensors update when a source changes, so a
//!   value counts as matching if HA showed it within ±N s).
//! - [`commands`]: Rust's commands against HA's actuations, derived from state changes of the
//!   output entities and (optionally) the SmartEVSE MQTT set topics, matched within a window.
//! - [`log`]: a small rotated JSONL file with the mismatch onsets.

pub mod commands;
pub mod fields;
pub mod log;

use energie_core::command::Command;
use energie_core::derived::Derived;
use energie_core::memory::Memory;
use energie_core::time::Ts;
use serde::Serialize;

use crate::config::{ShadowConfig, Tolerance};
use crate::ha::entity::{Lookup, StateChange};
use crate::ha::mapping::{MeterAdopter, MeterAdoption};
use crate::mqtt::MqttMessage;

/// A comparable value.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Val {
    Null,
    Num(f64),
    Bool(bool),
    Text(String),
}

impl Val {
    /// `|a − b|` for two numbers.
    pub fn err(&self, other: &Val) -> Option<f64> {
        match (self, other) {
            (Val::Num(a), Val::Num(b)) => Some((a - b).abs()),
            _ => None,
        }
    }

    /// Equal within `tol` (numbers) or exactly (everything else).
    pub fn matches(&self, other: &Val, tol: Tolerance) -> bool {
        match (self, other) {
            (Val::Num(a), Val::Num(b)) => (a - b).abs() <= tol.abs.max(tol.rel * b.abs()) + 1e-9,
            _ => self == other,
        }
    }
}

/// Everything the comparator learned, for `/api/shadow` and the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ShadowStats {
    pub since: Option<String>,
    pub ticks: u64,
    /// Matching samples over all compared samples, %.
    pub field_agreement_pct: Option<f64>,
    pub command_agreement_pct: Option<f64>,
    /// Worst agreement first.
    pub fields: Vec<fields::FieldStats>,
    pub commands: Vec<commands::KeyStats>,
    pub log_lines: u64,
    /// Meters re-adopted from HA after drifting apart (lockstep), and the latest one.
    pub meter_adoptions: u64,
    pub last_meter_adoption: Option<MeterAdoption>,
}

pub struct Shadow {
    pub fields: fields::FieldComparator,
    pub commands: commands::CommandComparator,
    pub log: log::MismatchLog,
    pub meters: MeterAdopter,
    since: Option<Ts>,
    ticks: u64,
}

impl Shadow {
    pub fn new(cfg: &ShadowConfig, log: log::MismatchLog) -> Self {
        Shadow {
            fields: fields::FieldComparator::new(cfg),
            commands: commands::CommandComparator::new(cfg.command_window_s as i64),
            log,
            meters: MeterAdopter::default(),
            since: None,
            ticks: 0,
        }
    }

    /// One tick's comparison. `before`/`after` are Memory around the engine tick (for the
    /// latch actuations), `changes`/`mqtt` what HA did since the last tick.
    #[allow(clippy::too_many_arguments)]
    pub fn observe(
        &mut self,
        now: Ts,
        derived: &Derived,
        cmds: &[Command],
        before: &Memory,
        after: &Memory,
        ha: &dyn Lookup,
        changes: &[StateChange],
        mqtt: &[MqttMessage],
    ) {
        self.since.get_or_insert(now);
        self.ticks += 1;
        self.fields.observe(now, derived, ha, &mut self.log);
        let mut rust = Vec::new();
        for c in cmds {
            rust.extend(commands::command_acts(c));
        }
        rust.extend(commands::latch_acts(before, after));
        let mut ha_acts: Vec<(Ts, commands::Act)> = Vec::new();
        for ch in changes {
            if let Some(a) = commands::change_act(ch) {
                ha_acts.push((ch.at, a));
            }
        }
        for m in mqtt {
            if let Some(a) = commands::mqtt_act(&m.topic, &m.payload) {
                ha_acts.push((m.at, a));
            }
        }
        self.commands.observe(now, rust, ha_acts, ha, &mut self.log);
    }

    /// Lockstep: re-adopt meters that drifted away from HA's, logging each one.
    pub fn adopt_meters(&mut self, meters: &mut energie_core::meters::MetersState, ha: &dyn Lookup, now: Ts) {
        for a in self.meters.adopt(meters, ha, now) {
            self.log.write(
                &serde_json::json!({"kind": "meter", "at": a.at, "entity": a.entity, "rust": a.rust, "ha": a.ha}),
            );
        }
    }

    pub fn stats(&self) -> ShadowStats {
        let fields = self.fields.stats();
        let (m, n) = fields.iter().fold((0, 0), |(m, n), f| (m + f.matches, n + f.samples));
        let commands = self.commands.stats();
        let (cm, cn) = commands.iter().fold((0, 0), |(m, n), k| (m + k.rust_matched + k.ha_matched, n + k.rust + k.ha));
        ShadowStats {
            since: self.since.map(|t| t.to_rfc3339()),
            ticks: self.ticks,
            field_agreement_pct: pct(m, n),
            command_agreement_pct: pct(cm, cn),
            fields,
            commands,
            log_lines: self.log.lines,
            meter_adoptions: self.meters.adoptions,
            last_meter_adoption: self.meters.last.clone(),
        }
    }
}

pub(crate) fn pct(m: u64, n: u64) -> Option<f64> {
    (n > 0).then(|| (m as f64) * 100.0 / (n as f64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::{EntityState, index};
    use crate::plug::tests::t0;
    use chrono::Duration;

    /// Numbers match within `max(abs, rel·|ha|)`; everything else must be equal.
    #[test]
    fn value_matching_rules() {
        let t = Tolerance { abs: 5.0, rel: 0.0 };
        assert!(Val::Num(100.0).matches(&Val::Num(105.0), t));
        assert!(!Val::Num(100.0).matches(&Val::Num(105.1), t));
        let r = Tolerance { abs: 0.0, rel: 0.01 };
        assert!(Val::Num(1010.0).matches(&Val::Num(1000.0), r));
        assert!(Val::Null.matches(&Val::Null, t));
        assert!(!Val::Null.matches(&Val::Num(0.0), t));
        assert!(Val::Text("buzz".into()).matches(&Val::Text("buzz".into()), t));
        assert_eq!(Val::Num(1.0).err(&Val::Num(3.0)), Some(2.0));
        assert_eq!(Val::Num(1.0).err(&Val::Null), None);
        assert_eq!(serde_json::to_string(&Val::Null).unwrap(), "null");
    }

    /// The aggregate combines field and command agreement and remembers when it started.
    #[test]
    fn aggregate_stats() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = ShadowConfig { time_tolerance_s: 0, ..Default::default() };
        let mut s = Shadow::new(&cfg, log::MismatchLog::new(Some(dir.path().join("m.jsonl")), 1 << 20));
        assert_eq!(s.stats().field_agreement_pct, None);
        let ha = index(vec![
            EntityState::new("sensor.batterij_setpoint", "0"),
            EntityState::new("select.smartevse_1234_mode", "Normal"),
        ]);
        let d = Derived::default();
        let before = Memory::default();
        let after = Memory { batterij_net_laden: true, ..Default::default() };
        let ch = StateChange {
            at: t0(),
            entity_id: "input_boolean.batterij_net_laden".into(),
            old: Some("off".into()),
            new: Some("on".into()),
        };
        let mq = MqttMessage { at: t0(), topic: "SmartEVSE/1234/Set/Mode".into(), payload: "Normal".into() };
        s.observe(
            t0(),
            &d,
            &[Command::EvseMode(energie_core::world::EvseMode::Normal)],
            &before,
            &after,
            &ha,
            &[ch],
            &[mq],
        );
        s.observe(t0() + Duration::seconds(1), &d, &[], &after, &after, &ha, &[], &[]);
        let st = s.stats();
        assert_eq!(st.ticks, 2);
        assert_eq!(st.since.as_deref(), Some("2026-11-07T13:46:00+01:00"));
        assert!(st.field_agreement_pct.is_some());
        assert_eq!(st.command_agreement_pct, Some(100.0));
        assert_eq!(pct(1, 4), Some(25.0));
    }

    /// A meter that drifted from HA's for the hold time is re-adopted, logged and counted.
    #[test]
    fn drifted_meters_are_adopted_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let mut s =
            Shadow::new(&ShadowConfig::default(), log::MismatchLog::new(Some(dir.path().join("m.jsonl")), 1 << 20));
        let ha = index(vec![EntityState::new("sensor.pv_energie_totaal", "12.5")]);
        let mut m = energie_core::meters::MetersState::default();
        s.adopt_meters(&mut m, &ha, t0());
        s.adopt_meters(&mut m, &ha, t0() + Duration::seconds(120));
        assert_eq!(m.pv_energie_totaal.total, Some(12.5));
        let st = s.stats();
        assert_eq!(st.meter_adoptions, 1);
        assert_eq!(st.last_meter_adoption.unwrap().entity, "sensor.pv_energie_totaal");
        assert_eq!(st.log_lines, 1);
    }
}
