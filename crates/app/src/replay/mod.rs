//! `energie replay`: run the engine over a Home Assistant recorder export and measure whether
//! it decides what the live HA decided.
//!
//! The export ([`history`]) is streamed in time order into an entity-state table. A virtual
//! clock steps every `step_ms`; each step applies the rows up to that instant, builds the
//! `World` from the table (`ha::mapping::world_from`), and runs `Engine::tick`.
//!
//! - **Lockstep** (default): every step first adopts HA's Settings and Memory helpers from the
//!   table as it stood *before* the step's rows, like shadow lockstep, so each decision starts
//!   from HA's state and not from HA's own reaction to the same change; meters that drift
//!   from HA's entities are re-adopted and counted (`ha::mapping::MeterAdopter`).
//!   Without lockstep, Settings and Memory are seeded once and then evolve on their own.
//! - **Meters** are seeded from HA's meter entities at the start (`ha::mapping::seed_meters`)
//!   and after every gap, and once more at the end of that settle window: by then HA has
//!   restored its meters after a restart, and the first rows of the export (a midnight reset
//!   a few ms after 00:00) are in.
//! - **Gaps**: no row for more than `gap_s` means HA (or its recorder) was down. The clock
//!   jumps over it, the engine restarts (as HA's automations do: `homeassistant: start`,
//!   `for:` timers and state machines start over) with the table's helpers and re-seeded
//!   meters, and nothing is compared until `settle_s` after the first row back (HA itself
//!   needs that long to restore and re-render everything). The replay's own start is
//!   handled the same way.
//! - Comparison: [`fields`] (every `Derived` field), [`acts`] (every actuator and latch) and
//!   [`probe`] (the Buzz override at HA's own decision instants).
//! - The Solis RC power numbers show HA's writes (`3331.0`) and, in between, the integration's
//!   register read-backs (`3330`, `0`: 10 W resolution and a stale 5 min cache, see FINDINGS);
//!   only the writes are HA actuations.

pub mod acts;
pub mod fields;
pub mod history;
pub mod probe;
pub mod report;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use chrono::TimeZone;
use energie_core::automations::ev_laden_buzz::actions;
use energie_core::automations::ev_modulator::{ChargerView, FLOOR_A, MODE_STEMPEL_ID, law};
use energie_core::command::Command;
use energie_core::engine::Engine;
use energie_core::memory::Memory;
use energie_core::time::{TZ, Ts, epoch};
use energie_core::{derived, meters};
use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};
use crate::ha::entity::{EntityState, Lookup};
use crate::ha::mapping::{
    MeterAdopter, MeterAdoption, evse_mode_label, input_entities, memory_from, seed_meters, settings_from, world_from,
};
use crate::shadow::Val;
use crate::shadow::commands::{act_key, command_acts, latch_acts, read_act};
use history::Row;
pub use report::Report;

#[derive(Debug, Clone, PartialEq)]
pub struct ReplayConfig {
    pub history: PathBuf,
    pub from: Option<Ts>,
    pub to: Option<Ts>,
    pub step_ms: u64,
    pub report: Option<PathBuf>,
    pub summary: Option<PathBuf>,
    pub lockstep: bool,
    /// A field agrees if HA showed Rust's value within ±this.
    pub time_tol_s: f64,
    /// Actuation matching window.
    pub window_s: f64,
    /// No row for longer than this is a gap (HA down).
    pub gap_s: f64,
    /// Nothing is compared for this long after the start and after each gap.
    pub settle_s: f64,
    /// Record every disagreeing sample of this field, or every act of this actuator key, in
    /// the report (`trace`).
    pub trace: Option<String>,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        ReplayConfig {
            history: PathBuf::new(),
            from: None,
            to: None,
            step_ms: 1000,
            report: None,
            summary: None,
            lockstep: true,
            time_tol_s: 5.0,
            window_s: 40.0,
            gap_s: 60.0,
            settle_s: 120.0,
            trace: None,
        }
    }
}

/// Epoch seconds to Brussels time.
pub fn ts(t: f64) -> Ts {
    let ms = (t * 1000.0).round() as i64;
    TZ.timestamp_millis_opt(ms).single().expect("in range")
}

/// `YYYY-MM-DD HH:MM:SS`, Brussels time (for humans looking things up in HA's history).
pub fn fmt_ts(t: f64) -> String {
    ts(t).format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Time spans `[a, b]` where nothing is compared.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spans(Vec<(f64, f64)>);

impl Spans {
    pub fn push(&mut self, a: f64, b: f64) {
        self.0.push((a, b));
    }

    pub fn contains(&self, t: f64) -> bool {
        self.0.iter().any(|(a, b)| *a <= t && t <= *b)
    }
}

/// A run of consecutive disagreeing samples, with its worst sample.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Episode {
    pub start: String,
    pub end: String,
    pub duration_s: f64,
    /// At the worst sample.
    pub worst_at: String,
    pub rust: Val,
    pub ha: Val,
    pub err: Option<f64>,
    #[serde(skip)]
    t0: f64,
}

impl Episode {
    fn new(t: f64, rust: Val, ha: Val, err: Option<f64>) -> Self {
        Episode { start: fmt_ts(t), end: fmt_ts(t), duration_s: 0.0, worst_at: fmt_ts(t), rust, ha, err, t0: t }
    }

    fn extend(&mut self, t: f64, rust: Val, ha: Val, err: Option<f64>) {
        self.end = fmt_ts(t);
        self.duration_s = t - self.t0;
        if err.is_some_and(|e| self.err.is_none_or(|w| e > w)) {
            *self = Episode {
                start: self.start.clone(),
                end: self.end.clone(),
                worst_at: fmt_ts(t),
                rust,
                ha,
                err,
                ..*self
            };
        }
    }

    /// Ranking: the error, or the duration when there is none.
    fn score(&self) -> f64 {
        self.err.unwrap_or(self.duration_s)
    }
}

/// HA (or its recorder) was down.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gap {
    pub from: String,
    pub to: String,
    pub seconds: f64,
}

const BUZZ_OVERRIDE: &str = "number.smartevse_1234_charge_current_override";
const BUZZ_MODE: &str = "select.smartevse_1234_mode";

/// The option a command selects on the SmartEVSE, if it is a mode command.
fn mode_of(c: Command) -> Option<&'static str> {
    if let Command::EvseMode(m) = c { Some(evse_mode_label(m)) } else { None }
}

/// The SmartEVSE's own entities (and the templates that only follow them).
fn is_evse(entity: &str) -> bool {
    entity.contains("smartevse_1234") || entity.starts_with("sensor.ev_laadvermogen_smartevse")
}

/// The table with some entities rolled back (`None`: absent then).
struct Overlay<'a> {
    base: &'a HashMap<String, EntityState>,
    over: &'a HashMap<String, Option<EntityState>>,
}

impl Lookup for Overlay<'_> {
    fn get(&self, id: &str) -> Option<&EntityState> {
        match self.over.get(id) {
            Some(e) => e.as_ref(),
            None => self.base.get(id),
        }
    }

    fn all(&self) -> Box<dyn Iterator<Item = &EntityState> + '_> {
        Box::new(self.base.keys().filter_map(|k| self.get(k)))
    }
}

/// A Solis RC power number read back from the inverter (an integer state) rather than written
/// by HA (`number.set_value` leaves a float, `3331.0`).
fn is_readback(key: &str, state: &str) -> bool {
    matches!(key, "solis.rc_charge_w" | "solis.rc_discharge_w") && !state.contains('.')
}

/// The latches compared as timelines: Rust's after-tick value of each.
fn latch_values(m: &Memory) -> [(&'static str, Val); 6] {
    [
        ("latch.batterij_net_laden", Val::Bool(m.batterij_net_laden)),
        ("latch.soluna_absorptie_bezig", Val::Bool(m.soluna_absorptie_bezig)),
        ("latch.ev_hoger_laadstil", Val::Bool(m.ev_hoger_laadstil)),
        ("latch.smartevse_plug_laatst_bekend", Val::Text(m.smartevse_plug_laatst_bekend.clone())),
        ("latch.smartevse_modus_laatst_bekend", Val::Text(m.smartevse_modus_laatst_bekend.clone())),
        ("latch.batterij_setpoint_bias", Val::Num(m.batterij_setpoint_bias)),
    ]
}

pub struct Replay {
    cfg: ReplayConfig,
    table: HashMap<String, EntityState>,
    engine: Engine,
    fields: fields::FieldComparator,
    acts: acts::ActComparator,
    adopter: MeterAdopter,
    buzz: probe::Probe,
    buzz_mode: probe::ModeProbe,
    /// The SmartEVSE entities as they were before the charger's rows since the last other row
    /// (the charger answers HA's command before the override echo arrives).
    evse_undo: HashMap<String, Option<EntityState>>,
    /// The entities the engine reads (template rows reacting to the charger do not count as
    /// a new state for the probe).
    inputs: HashSet<&'static str>,
    adoptions: BTreeMap<&'static str, u64>,
    adoption_events: Vec<MeterAdoption>,
    /// Seed the meters again at this time (the end of a settle window).
    reseed_at: Option<f64>,
    gaps: Vec<Gap>,
    skip: Spans,
    steps: u64,
    rows: u64,
    first_step: Option<f64>,
    last_step: Option<f64>,
    last_row: Option<f64>,
}

impl Replay {
    pub fn new(cfg: ReplayConfig) -> Self {
        let fields = fields::FieldComparator::new(cfg.time_tol_s, cfg.trace.clone());
        let acts = acts::ActComparator::new(cfg.window_s, cfg.time_tol_s);
        Replay {
            cfg,
            table: HashMap::new(),
            engine: Engine::new(Default::default(), Default::default()),
            fields,
            acts,
            adopter: MeterAdopter::default(),
            buzz: probe::Probe::new(0.05),
            buzz_mode: probe::ModeProbe::default(),
            evse_undo: HashMap::new(),
            inputs: input_entities().into_iter().collect(),
            adoptions: BTreeMap::new(),
            adoption_events: Vec::new(),
            reseed_at: None,
            gaps: Vec::new(),
            skip: Spans::default(),
            steps: 0,
            rows: 0,
            first_step: None,
            last_step: None,
            last_row: None,
        }
    }

    fn apply(&mut self, r: Row) {
        self.rows += 1;
        self.last_row = Some(r.ts);
        if (r.entity == BUZZ_OVERRIDE || r.entity == BUZZ_MODE) && !self.skip.contains(r.ts) {
            self.probe_buzz(r.ts, &r.entity, &r.state);
        }
        if is_evse(&r.entity) {
            if !self.evse_undo.contains_key(&r.entity) {
                self.evse_undo.insert(r.entity.clone(), self.table.get(&r.entity).cloned());
            }
        } else if self.inputs.contains(r.entity.as_str()) {
            self.evse_undo.clear();
        }
        let e = self.table.entry(r.entity).or_insert_with_key(|id| EntityState::new(id, ""));
        // `{}`: the recorder dropped the attributes (over its 16 KiB limit, e.g. the price
        // lists once tomorrow is published); keep the last recorded ones.
        if let Some(Value::Object(m)) = r.attrs.as_deref().and_then(|a| serde_json::from_str(a).ok())
            && !m.is_empty()
        {
            e.attributes = m;
        }
        e.state = r.state;
        self.fields.ha_row(r.ts, e);
        if let Some(key) = act_key(&e.entity_id)
            && !is_readback(key, &e.state)
        {
            self.acts.ha_state(r.ts, key, read_act(&e.entity_id, &e.state).map(|a| a.val));
        }
    }

    /// HA changes the Buzz override to `state` at `t`: what would Rust set on the state HA
    /// decided on? That is every row up to the last one that is not the charger's own answer
    /// to the command (its charge current and power arrive a few ms before the echo).
    fn probe_buzz(&mut self, t: f64, entity: &str, state: &str) {
        let old = self.table.get(entity).and_then(EntityState::value);
        let new = EntityState::new(entity, state);
        if old.is_none() || new.value().is_none() || old == new.value() {
            return;
        }
        let before = Overlay { base: &self.table, over: &self.evse_undo };
        let (at, w) = (ts(t), world_from(&before));
        let s = &self.engine.settings;
        // The P1 meters as HA had them at `t` (they update before the templates).
        let mut m = self.engine.memory.clone();
        meters::update_sources(at, &w, &mut m.meters);
        let m = &m;
        let d = derived::compute(at, &w, s, m);
        let target = law(&ChargerView::buzz(&w, &d, m), at, &w, &d, s, m).target;
        if entity == BUZZ_OVERRIDE {
            // `ev_laden_buzz` publishes the target, or parks the override at 6 A below the floor.
            let new = new.num().expect("a number");
            self.buzz.compare(t, new, if target >= FLOOR_A { target } else { FLOOR_A });
        } else {
            let since = m.last_triggered(MODE_STEMPEL_ID).map_or(99999.0, |l| energie_core::time::secs(l, at));
            let mode = actions(target, &w, since).into_iter().find_map(mode_of);
            self.buzz_mode.compare(t, &new.state, mode);
        }
    }

    /// (Re)start the engine from the table, as HA starts: helpers restored, meters restored,
    /// automations fresh.
    fn begin(&mut self, now: f64) {
        let settings = settings_from(&self.table, self.engine.settings.clone());
        let mut memory = memory_from(&self.table, self.engine.memory.clone());
        memory.meters = seed_meters(&self.table, ts(now));
        self.engine = Engine::new(settings, memory);
        self.adopter = MeterAdopter::default();
        self.skip.push(now, now + self.cfg.settle_s);
        self.reseed_at = Some(now + self.cfg.settle_s);
    }

    /// Lockstep: take HA's helpers as they were *before* this step's rows. Those rows hold
    /// what changed in the last second, and also HA's automations' reactions to it (a latch
    /// written 50 ms after the plug changed); adopting the reactions first would leave Rust
    /// nothing to decide.
    fn adopt(&mut self, now: f64) {
        self.engine.settings = settings_from(&self.table, self.engine.settings.clone());
        self.engine.memory = memory_from(&self.table, self.engine.memory.clone());
        for a in self.adopter.adopt(&mut self.engine.memory.meters, &self.table, ts(now)) {
            *self.adoptions.entry(a.entity).or_default() += 1;
            if self.adoption_events.len() < 100 {
                self.adoption_events.push(a);
            }
        }
    }

    fn step(&mut self, now: f64) {
        let at = ts(now);
        if self.reseed_at.is_some_and(|t| now >= t) {
            self.engine.memory.meters = seed_meters(&self.table, at);
            self.reseed_at = None;
        }
        let world = world_from(&self.table);
        let before = self.engine.memory.clone();
        let out = self.engine.tick(at, &world);
        self.fields.rust(now, &out.derived, !self.skip.contains(now));
        for c in &out.commands {
            for a in command_acts(c) {
                self.acts.rust_act(now, a);
            }
        }
        for a in latch_acts(&before, &self.engine.memory) {
            self.acts.rust_act(now, a);
        }
        for (key, v) in latch_values(&self.engine.memory) {
            self.acts.rust_latch(now, key, v);
        }
        self.steps += 1;
        self.first_step.get_or_insert(now);
        self.last_step = Some(now);
    }

    /// Replay `rows` (in time order) and report.
    pub fn run(mut self, rows: impl Iterator<Item = Result<Row>>) -> Result<Report> {
        let started = Instant::now();
        let step = self.cfg.step_ms.max(1) as f64 / 1000.0;
        let from = self.cfg.from.map(epoch);
        let to = self.cfg.to.map(epoch);
        let mut rows = rows.peekable();
        let first = match rows.peek() {
            None => return Err(Error::new("the history is empty")),
            Some(Err(e)) => return Err(e.clone()),
            Some(Ok(r)) => r.ts,
        };
        let start = from.unwrap_or(first).ceil();
        let mut now = start;
        let mut restart = true;
        loop {
            if self.cfg.lockstep && !restart {
                self.adopt(now);
            }
            while let Some(r) = rows.next_if(|r| r.as_ref().map_or(true, |r| r.ts <= now)) {
                self.apply(r?);
            }
            if to.is_some_and(|t| now > t) {
                break;
            }
            let last_row = self.last_row.unwrap_or(first);
            match rows.peek() {
                None if now > last_row => break,
                Some(Ok(next)) if next.ts - last_row > self.cfg.gap_s && now > last_row => {
                    self.gaps.push(Gap { from: fmt_ts(last_row), to: fmt_ts(next.ts), seconds: next.ts - last_row });
                    now = start + ((next.ts - start) / step).ceil() * step;
                    restart = true;
                    continue;
                }
                _ => {}
            }
            if restart {
                self.begin(now);
                restart = false;
            }
            self.step(now);
            now += step;
        }
        self.fields.finish();
        let (a, b) = (self.first_step.unwrap_or(start), self.last_step.unwrap_or(start));
        let fields = self.fields.report();
        let (m, n) = fields.iter().fold((0, 0), |(m, n), f| (m + f.matches, n + f.samples));
        Ok(Report {
            history: self.cfg.history.display().to_string(),
            from: fmt_ts(a),
            to: fmt_ts(b),
            step_ms: self.cfg.step_ms,
            lockstep: self.cfg.lockstep,
            steps: self.steps,
            rows: self.rows,
            elapsed_s: started.elapsed().as_secs_f64(),
            gaps: self.gaps,
            meter_adoptions: self.adoptions.values().sum(),
            meter_adoptions_by_entity: self.adoptions,
            meter_adoption_events: self.adoption_events,
            field_agreement_pct: crate::shadow::pct(m, n),
            fields,
            actuators: self.acts.report(a, b, &self.skip),
            buzz_target: self.buzz.report(),
            buzz_mode: self.buzz_mode.report(),
            trace: {
                let mut t = std::mem::take(&mut self.fields.trace);
                if let Some(k) = &self.cfg.trace {
                    t.extend(self.acts.trace(k));
                }
                t
            },
        })
    }
}

/// Replay the configured file and write the report and summary.
pub fn run(cfg: ReplayConfig) -> Result<Report> {
    let rows = history::open(&cfg.history)?;
    let (json, md) = (cfg.report.clone(), cfg.summary.clone());
    let r = Replay::new(cfg).run(rows)?;
    if let Some(p) = json {
        std::fs::write(&p, serde_json::to_string_pretty(&r)?)
            .map_err(|e| Error::new(format!("{}: {e}", p.display())))?;
    }
    if let Some(p) = md {
        std::fs::write(&p, report::markdown(&r)).map_err(|e| Error::new(format!("{}: {e}", p.display())))?;
    }
    Ok(r)
}

#[cfg(test)]
mod tests;
