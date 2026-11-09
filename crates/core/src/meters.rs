//! Stateful HA helpers: `utility_meter` (resetting counters), `integration` (Riemann sums) and
//! `statistics` (the P1 median). Their state lives in [`Memory`](crate::memory::Memory).
//!
//! Each helper reproduces the HA 2026.10 source of its integration, driven by one source
//! reading per engine tick. The utility meters and the median take every tick as a source
//! event (their sources are device sensors, re-reported on every poll). The integrals' sources
//! are template sensors, which HA only writes when the rendered value *changes*
//! (`async_track_template_result` drops identical results, so there is no `state_reported`
//! either): an integral sees an event only on a tick where its source value changed
//! ([`Integrator::tick`]), and the `max_sub_interval` timer in between. The rules that matter:
//!
//! **utility_meter** (`delta_values: false`, no tariffs; `calculate_adjustment` and
//! `async_reading` of HA 2026.10):
//! - `periodically_resetting: false` (the P1 registers and the energy integrals since the
//!   2026-11-07 fixes): adjustment = new − `last_valid_state`, the last *valid* source reading,
//!   which survives an unavailable/unknown source (and an HA restart, it is restored). The first
//!   reading after a gap is diffed against the reading before it, so the gap's energy counts.
//! - `periodically_resetting: true` (the EV and cost meters): adjustment = new − old source
//!   state, where *old* is the source's previous state. If the previous state was
//!   unavailable/unknown there is **no** adjustment: the first reading after a gap only
//!   re-baselines (what happened during the gap is lost).
//! - Either way the new reading becomes `last_valid_state`.
//! - `net_consumption: false` drops negative adjustments (`adjustment >= 0` guard). A source
//!   that decreases (meter reset, rounding jitter, also across a gap) does not subtract, and
//!   the next increase counts from the new, lower value. Nothing is "added from 0": the drop
//!   itself is lost.
//! - `net_consumption: true` adds negative adjustments too.
//! - At each cycle boundary (local `*/15 * * * *`, `0 0 * * *`, `0 0 1 * *`) the value is set
//!   to 0. A reading that arrives after the boundary counts entirely in the new cycle.
//! - Source unavailable: with `always_available: false` the meter becomes unavailable (and its
//!   attributes, `next_reset` included, disappear); with `true` it keeps its value. A source
//!   that is `unknown` (not unavailable) leaves the meter available and unchanged.
//! - The very first valid reading of a never-started meter sets it to 0.
//!
//! **integration**: trapezoidal `Δt·(old+new)/2`, left `Δt·old`, over the time since the
//! previous source event; scaled by `unit_prefix` (k = 1000) and `unit_time` (h = 3600 s).
//! With `max_sub_interval`, when no source event arrives for that long the held value is
//! integrated up to then (and every interval after), and the next event integrates only the
//! remainder since that time. The first event after start or after an unavailable source
//! adds nothing. `round` rounds the *reported* state (ties to even); the internal sum keeps
//! full precision.
//!
//! **statistics** (median, `sampling_size` 60, `max_age` 10 s, `keep_last_sample`,
//! `precision` 0): a sample per source event, samples older than `max_age` purged at
//! evaluation time except the very last one, Python `statistics.median` (mean of the two
//! middle samples for an even count), then `round(x, 0)` → `int`.

use std::collections::VecDeque;

use chrono::{Datelike, Duration, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};

use crate::derived::Derived;
use crate::jinja;
use crate::time::{self, TZ, Ts};
use crate::world::World;

// --- utility_meter -----------------------------------------------------------------------

/// `cycle` of a utility meter, local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cycle {
    QuarterHourly,
    Daily,
    Monthly,
}

impl Cycle {
    /// The first reset strictly after `t`.
    pub fn next_reset_after(self, t: Ts) -> Ts {
        match self {
            Cycle::QuarterHourly => time::next_quarter(t),
            Cycle::Daily => local_midnight(t.date_naive() + Duration::days(1)),
            Cycle::Monthly => {
                let d = t.date_naive();
                let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
                local_midnight(NaiveDate::from_ymd_opt(y, m, 1).expect("valid date"))
            }
        }
    }

    /// The start of the cycle containing `t`.
    pub fn period_start(self, t: Ts) -> Ts {
        match self {
            Cycle::QuarterHourly => time::quarter_start(t),
            Cycle::Daily => local_midnight(t.date_naive()),
            Cycle::Monthly => local_midnight(t.date_naive().with_day(1).expect("day 1 exists")),
        }
    }
}

/// Midnight is never inside a Brussels DST gap (the switches are at 02:00/03:00).
fn local_midnight(d: NaiveDate) -> Ts {
    TZ.from_local_datetime(&d.and_hms_opt(0, 0, 0).expect("valid time")).single().expect("midnight is unambiguous")
}

/// Static options of one utility meter (from its config entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtilityConfig {
    pub cycle: Cycle,
    pub net_consumption: bool,
    pub always_available: bool,
    /// `periodically_resetting`: the source itself resets, so an invalid source state breaks
    /// the delta chain instead of diffing against the last valid reading.
    pub periodically_resetting: bool,
}

/// What a utility meter sees of its source this tick.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    /// `unavailable`.
    Unavailable,
    /// `unknown` (or not numeric).
    Unknown,
    Value(f64),
}

impl Source {
    /// A device reading, where `None` is `unavailable`.
    pub fn device(v: Option<f64>) -> Self {
        v.map_or(Source::Unavailable, Source::Value)
    }

    /// Another helper's state, where `None` is `unknown` (it has no value yet).
    pub fn helper(v: Option<f64>) -> Self {
        v.map_or(Source::Unknown, Source::Value)
    }
}

/// Runtime state of one utility meter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UtilityMeter {
    /// The meter's own state; `None` = never started (`unknown`).
    pub value: Option<f64>,
    /// `false` while the source is unavailable and `always_available` is off.
    pub available: bool,
    /// `last_reset` attribute; `None` = unknown, taken as the start of the current cycle.
    #[serde(with = "crate::time::serde_opt")]
    pub last_reset: Option<Ts>,
    /// `last_period` attribute: the value just before the last reset.
    pub last_period: f64,
    /// The source reading the next one is diffed against: HA's `last_valid_state` (kept across
    /// an invalid source state) when not periodically resetting, else its `old_state` (`None`
    /// after unavailable/unknown). `None` = not seen.
    pub last_source: Option<f64>,
}

impl Default for UtilityMeter {
    fn default() -> Self {
        UtilityMeter { value: None, available: true, last_reset: None, last_period: 0.0, last_source: None }
    }
}

impl UtilityMeter {
    /// A meter restored from HA: its state and `last_reset` attribute.
    pub fn seeded(value: f64, last_reset: Option<Ts>) -> Self {
        UtilityMeter { value: Some(value), last_reset, ..Default::default() }
    }

    /// A reset is due at or before `now` that has not been applied yet.
    fn reset_due(&self, cfg: UtilityConfig, now: Ts) -> Option<Ts> {
        let last = self.last_reset?;
        let next = cfg.cycle.next_reset_after(last);
        (next <= now).then_some(next)
    }

    /// The state as other entities read it at `now`: `None` when unavailable or unknown, 0
    /// when the cycle has rolled over since the last update (HA resets on a timer, before any
    /// later source event).
    pub fn reading(&self, cfg: UtilityConfig, now: Ts) -> Option<f64> {
        if !self.available {
            return None;
        }
        match self.reset_due(cfg, now) {
            Some(_) => Some(0.0),
            None => self.value,
        }
    }

    /// Apply resets due by `now`, then the source reading of this tick.
    pub fn update(&mut self, cfg: UtilityConfig, now: Ts, src: Source) {
        match self.last_reset {
            None => self.last_reset = Some(cfg.cycle.period_start(now)),
            Some(_) => {
                // Every missed boundary fires its own reset, like HA's cron catching up.
                while let Some(at) = self.reset_due(cfg, now) {
                    self.last_period = self.value.unwrap_or(0.0);
                    self.value = Some(0.0);
                    self.last_reset = Some(at);
                }
            }
        }

        let new = match src {
            Source::Unavailable => {
                if !cfg.always_available {
                    self.available = false;
                }
                if cfg.periodically_resetting {
                    self.last_source = None;
                }
                return;
            }
            Source::Unknown => {
                self.available = true;
                if cfg.periodically_resetting {
                    self.last_source = None;
                }
                return;
            }
            Source::Value(v) => v,
        };
        self.available = true;
        let value = self.value.unwrap_or(0.0);
        let adjustment = self.last_source.map(|old| new - old);
        self.value = Some(match adjustment {
            Some(a) if cfg.net_consumption || a >= 0.0 => value + a,
            _ => value,
        });
        self.last_source = Some(new);
    }
}

// --- integration -------------------------------------------------------------------------

/// Riemann sum method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Trapezoidal,
    Left,
}

/// Static options of one integration sensor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IntegrationConfig {
    pub method: Method,
    /// `round`; `None` reports full precision.
    pub round: Option<i32>,
    /// `unit_prefix × unit_time` in seconds: 1000 × 3600 for W → kWh, 3600 for kW → kWh
    /// or EUR/h → EUR.
    pub divisor: f64,
    /// `max_sub_interval`, seconds.
    pub max_sub_interval_s: f64,
}

/// Runtime state of one integration sensor.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Integrator {
    /// The full-precision integral; `None` = nothing integrated yet (`unknown`).
    pub total: Option<f64>,
    /// The source's last state; `None` = unavailable or not seen.
    pub last_value: Option<f64>,
    /// When the source last reported (HA's `old_state.last_reported`).
    #[serde(with = "crate::time::serde_opt")]
    pub last_event: Option<Ts>,
    /// When the integral last advanced (HA's `_last_integration_time`).
    #[serde(with = "crate::time::serde_opt")]
    pub last_integration: Option<Ts>,
    /// The last advance was a `max_sub_interval` timeout, not a source event.
    pub time_triggered: bool,
}

impl Integrator {
    /// An integral restored from HA's state.
    pub fn seeded(total: f64) -> Self {
        Integrator { total: Some(total), ..Default::default() }
    }

    /// The state as other entities read it (rounded when configured).
    pub fn reading(&self, cfg: IntegrationConfig) -> Option<f64> {
        self.total.map(|t| cfg.round.map_or(t, |n| jinja::round(t, n)))
    }

    fn add(&mut self, cfg: IntegrationConfig, area: f64) {
        self.total = Some(self.total.unwrap_or(0.0) + area / cfg.divisor);
    }

    /// `max_sub_interval` timeouts that fell due by `now`: the held value is integrated up to
    /// each of them.
    fn catch_up(&mut self, cfg: IntegrationConfig, now: Ts) {
        if let (Some(held), Some(mut since)) = (self.last_value, self.last_integration) {
            let step = Duration::milliseconds((cfg.max_sub_interval_s * 1000.0) as i64);
            while since + step <= now {
                since += step;
                self.add(cfg, held * cfg.max_sub_interval_s);
                self.last_integration = Some(since);
                self.time_triggered = true;
            }
        }
    }

    /// One engine tick: a source *event* when the reading changed (HA's `state_changed`;
    /// a template source never reports an unchanged value), otherwise only the
    /// `max_sub_interval` timer runs. A step from 0 to 2126 W is therefore a trapezoid back to
    /// the previous event, as HA integrates it, not a step at the tick it was seen.
    pub fn tick(&mut self, cfg: IntegrationConfig, now: Ts, v: Option<f64>) {
        if self.last_event.is_none() || v != self.last_value {
            self.update(cfg, now, v);
        } else {
            self.catch_up(cfg, now);
        }
    }

    /// Advance with the source reading at `now`; `None` is `unavailable`.
    pub fn update(&mut self, cfg: IntegrationConfig, now: Ts, v: Option<f64>) {
        self.catch_up(cfg, now);

        let old = self.last_value;
        let from = if self.time_triggered { self.last_integration } else { self.last_event };
        let first = self.last_event.is_none();
        self.last_value = v;
        self.last_event = Some(now);
        self.last_integration = Some(now);
        self.time_triggered = false;
        let (Some(new), Some(old), Some(from), false) = (v, old, from, first) else {
            return;
        };
        let dt = time::secs(from, now);
        let area = match cfg.method {
            Method::Trapezoidal => dt * (old + new) / 2.0,
            Method::Left => dt * old,
        };
        self.add(cfg, area);
    }
}

// --- statistics --------------------------------------------------------------------------

/// `sampling_size` of `P1 mediaan 90s`.
pub const MEDIAN_SAMPLES: usize = 60;
/// `max_age` of `P1 mediaan 90s`, seconds. Only the entity id says 90 s; the helper's name in
/// the entity registry is "P1 mediaan (10s)".
pub const MEDIAN_MAX_AGE_S: f64 = 10.0;

/// The `P1 mediaan 90s` statistics sensor (`sensor.p1_meter_p1_mediaan_90s`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Median {
    /// Sample values, oldest first.
    pub states: VecDeque<f64>,
    /// Sample timestamps, epoch seconds.
    pub ages: VecDeque<f64>,
    /// `false` after an unavailable source event.
    pub available: bool,
}

impl Default for Median {
    fn default() -> Self {
        Median { states: VecDeque::new(), ages: VecDeque::new(), available: true }
    }
}

impl Median {
    fn purge(&mut self, now: Ts) {
        let now = time::epoch(now);
        while let Some(&age) = self.ages.front() {
            if now - age <= MEDIAN_MAX_AGE_S || self.ages.len() == 1 {
                break;
            }
            self.ages.pop_front();
            self.states.pop_front();
        }
    }

    /// A source event at `now`; `None` is `unavailable`.
    pub fn update(&mut self, now: Ts, v: Option<f64>) {
        self.available = v.is_some();
        if let Some(v) = v {
            if self.states.len() == MEDIAN_SAMPLES {
                self.states.pop_front();
                self.ages.pop_front();
            }
            self.states.push_back(v);
            self.ages.push_back(time::epoch(now));
        }
        self.purge(now);
    }

    /// One engine tick: a sample only when P1 has a new reading. HA samples per P1 event
    /// (one per HomeWizard poll, ~5 s); the engine ticks every second with the last reading,
    /// and sampling that every tick would fill the 10 s window with copies of it.
    pub fn tick(&mut self, now: Ts, v: Option<f64>) {
        if v.is_none() || !self.available || v != self.states.back().copied() {
            self.update(now, v);
        }
    }

    /// The state at `now` (after HA's timed purge), W.
    pub fn reading(&self, now: Ts) -> Option<f64> {
        if !self.available {
            return None;
        }
        let mut s = self.clone();
        s.purge(now);
        let mut v: Vec<f64> = s.states.into_iter().collect();
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
        Some(jinja::round(median, 0))
    }
}

// --- the configured helpers --------------------------------------------------------------

/// Live config entries of the utility meters.
pub mod utility {
    use super::{Cycle::*, UtilityConfig};
    /// `Kwartier import`: `periodically_resetting: false` since 2026-11-07.
    pub const KWARTIER_IMPORT: UtilityConfig = UtilityConfig {
        cycle: QuarterHourly,
        net_consumption: false,
        always_available: false,
        periodically_resetting: false,
    };
    /// PV / huisverbruik / netimport / injectie / batterij geladen / ontladen dag:
    /// `periodically_resetting: false` since 2026-11-07.
    pub const DAG_ALTIJD: UtilityConfig =
        UtilityConfig { cycle: Daily, net_consumption: false, always_available: true, periodically_resetting: false };
    pub const ENERGIEKOST_VANDAAG: UtilityConfig =
        UtilityConfig { cycle: Daily, net_consumption: true, always_available: false, periodically_resetting: true };
    /// EV3/Buzz laadkost and laadenergie dag.
    pub const EV_DAG: UtilityConfig =
        UtilityConfig { cycle: Daily, net_consumption: false, always_available: false, periodically_resetting: true };
    /// EV3/Buzz laadkost and laadenergie maand.
    pub const EV_MAAND: UtilityConfig =
        UtilityConfig { cycle: Monthly, net_consumption: false, always_available: false, periodically_resetting: true };
}

/// Live config entries of the integration sensors.
pub mod integration {
    use super::{IntegrationConfig, Method};
    /// W → kWh, trapezoidal, round 3, `max_sub_interval` 1 min.
    pub const KWH_FROM_W: IntegrationConfig = IntegrationConfig {
        method: Method::Trapezoidal,
        round: Some(3),
        divisor: 3_600_000.0,
        max_sub_interval_s: 60.0,
    };
    /// `Energiekost cumulatief`: EUR/h → EUR, trapezoidal, round 3, 5 min.
    pub const ENERGIEKOST: IntegrationConfig =
        IntegrationConfig { method: Method::Trapezoidal, round: Some(3), divisor: 3_600.0, max_sub_interval_s: 300.0 };
    /// EV energies (kW → kWh) and costs (EUR/h → EUR): left, unrounded, 1 min.
    pub const EV: IntegrationConfig =
        IntegrationConfig { method: Method::Left, round: None, divisor: 3_600.0, max_sub_interval_s: 60.0 };
}

/// One car's four integrals and four utility meters.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CarMeters {
    /// `sensor.<car>_laadenergie_zon`, kWh.
    pub laadenergie_zon: Integrator,
    /// `sensor.<car>_laadenergie_net`, kWh.
    pub laadenergie_net: Integrator,
    /// `sensor.<car>_laadenergie_totaal`, kWh.
    pub laadenergie_totaal: Integrator,
    /// `sensor.<car>_laadkost_net`, EUR.
    pub laadkost_net: Integrator,
    /// `sensor.<car>_laadkost_dag` / `_maand` (source `laadkost_net`).
    pub laadkost_dag: UtilityMeter,
    pub laadkost_maand: UtilityMeter,
    /// `sensor.<car>_laadenergie_dag` / `_maand` (source `laadenergie_totaal`).
    pub laadenergie_dag: UtilityMeter,
    pub laadenergie_maand: UtilityMeter,
}

impl CarMeters {
    /// `zon`, `net`, `totaal` in kW and the cost rate in EUR/h, from this tick's templates.
    fn update(&mut self, now: Ts, zon: f64, net: f64, totaal: f64, kost: f64) {
        use integration::EV;
        self.laadenergie_zon.tick(EV, now, Some(zon));
        self.laadenergie_net.tick(EV, now, Some(net));
        self.laadenergie_totaal.tick(EV, now, Some(totaal));
        self.laadkost_net.tick(EV, now, Some(kost));
        let kost = Source::helper(self.laadkost_net.reading(EV));
        let energie = Source::helper(self.laadenergie_totaal.reading(EV));
        self.laadkost_dag.update(utility::EV_DAG, now, kost);
        self.laadkost_maand.update(utility::EV_MAAND, now, kost);
        self.laadenergie_dag.update(utility::EV_DAG, now, energie);
        self.laadenergie_maand.update(utility::EV_MAAND, now, energie);
    }
}

/// Every meter the stack keeps. Fields are public so the app can seed them from HA.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetersState {
    /// `sensor.p1_meter_kwartier_import` ← `sensor.p1_meter_energie_import`.
    pub kwartier_import: UtilityMeter,
    /// `sensor.p1_meter_p1_mediaan_90s` ← `sensor.p1_meter_vermogen`.
    pub p1_mediaan: Median,
    /// `sensor.batterij_ontladen_energie` ← `sensor.batterij_ontlaadvermogen`.
    pub batterij_ontladen_energie: Integrator,
    /// `sensor.batterij_geladen_energie` ← `sensor.batterij_laadvermogen`.
    pub batterij_geladen_energie: Integrator,
    /// `sensor.pv_energie_totaal` ← `sensor.pv_vermogen_totaal`.
    pub pv_energie_totaal: Integrator,
    /// `sensor.huisverbruik_energie` ← `sensor.huisverbruik_vermogen`.
    pub huisverbruik_energie: Integrator,
    /// `sensor.energiekost_cumulatief` ← `sensor.kost_per_uur`.
    pub energiekost_cumulatief: Integrator,
    /// `sensor.pv_energie_dag` ← `sensor.pv_energie_totaal`.
    pub pv_energie_dag: UtilityMeter,
    /// `sensor.huisverbruik_dag` ← `sensor.huisverbruik_energie`.
    pub huisverbruik_dag: UtilityMeter,
    /// `sensor.netimport_dag` ← `sensor.p1_meter_energie_import`.
    pub netimport_dag: UtilityMeter,
    /// `sensor.injectie_dag` ← `sensor.p1_meter_energie_export`.
    pub injectie_dag: UtilityMeter,
    /// `sensor.batterij_geladen_dag` ← `sensor.batterij_geladen_energie`.
    pub batterij_geladen_dag: UtilityMeter,
    /// `sensor.batterij_ontladen_dag` ← `sensor.batterij_ontladen_energie`.
    pub batterij_ontladen_dag: UtilityMeter,
    /// `sensor.energiekost_vandaag` ← `sensor.energiekost_cumulatief` (net consumption).
    pub energiekost_vandaag: UtilityMeter,
    /// EV3 (`sensor.ev3_*`).
    pub ev3: CarMeters,
    /// Buzz (`sensor.buzz_*`).
    pub buzz: CarMeters,
}

/// Advance every meter with this tick's world and derived values.
pub fn update(now: Ts, w: &World, d: &Derived, st: &mut MetersState) {
    update_sources(now, w, st);
    update_integrals(now, d, st);
}

/// The helpers fed by device sensors (the P1 median and the P1 register meters). In HA they
/// update on the P1 event itself, before the templates that read them re-render, so the
/// engine runs this *before* computing this tick's templates: `Kwartier toegestaan vermogen`
/// at the tick a new P1 import reading arrives sees the quarter meter including it.
pub fn update_sources(now: Ts, w: &World, st: &mut MetersState) {
    use utility::{DAG_ALTIJD, KWARTIER_IMPORT};

    st.p1_mediaan.tick(now, w.p1.power_w);

    // The P1 registers never reset (`periodically_resetting: false`): after a P1 dropout the
    // import across the gap is counted against the last valid reading.
    let import = Source::device(w.p1.import_kwh);
    st.kwartier_import.update(KWARTIER_IMPORT, now, import);
    st.netimport_dag.update(DAG_ALTIJD, now, import);
    st.injectie_dag.update(DAG_ALTIJD, now, Source::device(w.p1.export_kwh));
}

/// The helpers fed by this tick's templates (after they are computed).
pub fn update_integrals(now: Ts, d: &Derived, st: &mut MetersState) {
    use integration::{ENERGIEKOST, KWH_FROM_W};
    use utility::{DAG_ALTIJD, ENERGIEKOST_VANDAAG};

    st.batterij_ontladen_energie.tick(KWH_FROM_W, now, Some(d.batterij_ontlaadvermogen_w));
    st.batterij_geladen_energie.tick(KWH_FROM_W, now, Some(d.batterij_laadvermogen_w));
    st.pv_energie_totaal.tick(KWH_FROM_W, now, Some(d.pv_vermogen_totaal_w));
    st.huisverbruik_energie.tick(KWH_FROM_W, now, Some(d.huisverbruik_vermogen_w));
    st.energiekost_cumulatief.tick(ENERGIEKOST, now, Some(d.kost_per_uur_eur));

    let helper = |i: &Integrator, cfg| Source::helper(i.reading(cfg));
    st.pv_energie_dag.update(DAG_ALTIJD, now, helper(&st.pv_energie_totaal, KWH_FROM_W));
    st.huisverbruik_dag.update(DAG_ALTIJD, now, helper(&st.huisverbruik_energie, KWH_FROM_W));
    st.batterij_geladen_dag.update(DAG_ALTIJD, now, helper(&st.batterij_geladen_energie, KWH_FROM_W));
    st.batterij_ontladen_dag.update(DAG_ALTIJD, now, helper(&st.batterij_ontladen_energie, KWH_FROM_W));
    st.energiekost_vandaag.update(ENERGIEKOST_VANDAAG, now, helper(&st.energiekost_cumulatief, ENERGIEKOST));

    st.ev3.update(
        now,
        d.ev3_laadvermogen_zon_kw,
        d.ev3_laadvermogen_net_kw,
        d.ev3_laadvermogen_totaal_kw,
        d.ev3_laadkost_per_uur_eur,
    );
    st.buzz.update(
        now,
        d.buzz_laadvermogen_zon_kw,
        d.buzz_laadvermogen_net_kw,
        d.buzz_laadvermogen_totaal_kw,
        d.buzz_laadkost_per_uur_eur,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::at;

    const T0: (i32, u32, u32, u32, u32, u32) = (2026, 11, 7, 13, 46, 0);

    fn t0() -> Ts {
        let (y, mo, d, h, mi, s) = T0;
        at(y, mo, d, h, mi, s)
    }

    fn secs(s: i64) -> Ts {
        t0() + Duration::seconds(s)
    }

    const DAG: UtilityConfig = utility::DAG_ALTIJD;
    const KWARTIER: UtilityConfig = utility::KWARTIER_IMPORT;

    fn run(cfg: UtilityConfig, readings: &[Source]) -> (UtilityMeter, Vec<Option<f64>>) {
        let mut um = UtilityMeter::default();
        let mut seen = Vec::new();
        for (i, r) in readings.iter().enumerate() {
            let now = secs(i as i64);
            um.update(cfg, now, *r);
            seen.push(um.reading(cfg, now).map(|v| jinja::round(v, 6)));
        }
        (um, seen)
    }

    use Source::{Unavailable, Unknown, Value as V};

    /// utility_meter: the first reading starts a fresh meter at 0, then every increase of the
    /// source is added: 100 → 100.5 → 101.25 gives 0, 0.5, 1.25.
    #[test]
    fn utility_meter_starts_at_zero_and_adds_deltas() {
        let (_, seen) = run(DAG, &[V(100.0), V(100.5), V(101.25)]);
        assert_eq!(seen, vec![Some(0.0), Some(0.5), Some(1.25)]);
    }

    /// `net_consumption: false` drops a decrease (`adjustment >= 0` guard) and counts on from
    /// the lower value: 100 → 99 → 99.5 gives 0, 0, 0.5.
    #[test]
    fn utility_meter_without_net_consumption_ignores_decreases() {
        let (_, seen) = run(DAG, &[V(100.0), V(99.0), V(99.5)]);
        assert_eq!(seen, vec![Some(0.0), Some(0.0), Some(0.5)]);
    }

    /// `Energiekost vandaag` (`net_consumption: true`) follows the cumulative cost down while
    /// exporting: 10 → 9.5 → 9.75 gives 0, −0.5, −0.25.
    #[test]
    fn utility_meter_with_net_consumption_subtracts() {
        let (_, seen) = run(utility::ENERGIEKOST_VANDAAG, &[V(10.0), V(9.5), V(9.75)]);
        assert_eq!(seen, vec![Some(0.0), Some(-0.5), Some(-0.25)]);
    }

    /// `periodically_resetting: true` (the EV meters): after an unavailable source the first
    /// reading only re-baselines. 100, unavailable, 105, 106 → the 5 kWh across the gap are
    /// lost, the final 1 counts; `always_available: false`, so it is unavailable in the gap.
    #[test]
    fn a_periodically_resetting_meter_rebaselines_after_an_unavailable_source() {
        let readings = [V(100.0), V(100.5), Unavailable, V(105.0), V(106.0)];
        let (um, seen) = run(utility::EV_DAG, &readings);
        assert_eq!(seen, vec![Some(0.0), Some(0.5), None, Some(0.5), Some(1.5)]);
        assert!(um.available);
    }

    /// `periodically_resetting: false` (`Kwartier import`, the daily energy meters since
    /// 2026-11-07): the first reading after the gap is diffed against `last_valid_state`.
    /// 100, 100.5, unavailable, 105, 106 → 0, 0.5, –, 5.0, 6.0: the 4.5 kWh drawn during the gap
    /// count. `Kwartier import` (`always_available: false`) is unavailable during the gap; a
    /// daily meter (`always_available: true`) keeps showing 0.5.
    #[test]
    fn a_non_resetting_meter_counts_the_gap_against_the_last_valid_reading() {
        let readings = [V(100.0), V(100.5), Unavailable, V(105.0), V(106.0)];
        let (um, seen) = run(KWARTIER, &readings);
        assert_eq!(seen, vec![Some(0.0), Some(0.5), None, Some(5.0), Some(6.0)]);
        assert!(um.available);
        let (_, seen) = run(DAG, &readings);
        assert_eq!(seen, vec![Some(0.0), Some(0.5), Some(0.5), Some(5.0), Some(6.0)]);
    }

    /// `periodically_resetting: false`, a source lower after the gap: the adjustment 99 − 100 is
    /// negative and dropped (`net_consumption: false`), and 99 becomes `last_valid_state`, so
    /// 99.5 adds 0.5. With `net_consumption: true` the −1 would be added.
    #[test]
    fn a_decrease_across_the_gap_is_dropped_without_net_consumption() {
        let (um, seen) = run(DAG, &[V(100.0), Unavailable, V(99.0), V(99.5)]);
        assert_eq!(seen, vec![Some(0.0), Some(0.0), Some(0.0), Some(0.5)]);
        assert_eq!(um.last_source, Some(99.5));
        let net = UtilityConfig { periodically_resetting: false, ..utility::ENERGIEKOST_VANDAAG };
        let (_, seen) = run(net, &[V(100.0), Unavailable, V(99.0), V(99.5)]);
        assert_eq!(seen, vec![Some(0.0), None, Some(-1.0), Some(-0.5)]);
    }

    /// An `unknown` source (a helper without a value yet) leaves the meter available and
    /// unchanged. Periodically resetting it breaks the delta chain (1, unknown, 3, 4 → 0, 0, 0,
    /// 1); otherwise 3 is diffed against 1 (→ 2, then 3).
    #[test]
    fn utility_meter_holds_on_an_unknown_source() {
        let (_, seen) = run(utility::EV_DAG, &[V(1.0), Unknown, V(3.0), V(4.0)]);
        assert_eq!(seen, vec![Some(0.0), Some(0.0), Some(0.0), Some(1.0)]);
        let (_, seen) = run(KWARTIER, &[V(1.0), Unknown, V(3.0), V(4.0)]);
        assert_eq!(seen, vec![Some(0.0), Some(0.0), Some(2.0), Some(3.0)]);
        let (um, seen) = run(KWARTIER, &[Unknown]);
        assert_eq!(seen, vec![None]);
        assert_eq!(um.value, None);
    }

    /// `Kwartier import` resets at the quarter: the reading before 14:00 is 0.2, at 14:00:00.5
    /// other templates already read 0, and the reading at 14:00:01 (+0.01 kWh) counts entirely
    /// in the new quarter. `last_period` keeps 0.2.
    #[test]
    fn quarter_meter_resets_on_the_boundary() {
        let mut um = UtilityMeter::seeded(0.19, Some(at(2026, 11, 7, 13, 45, 0)));
        um.update(KWARTIER, at(2026, 11, 7, 13, 59, 58), V(500.0));
        um.update(KWARTIER, at(2026, 11, 7, 13, 59, 59), V(500.01));
        assert_eq!(um.reading(KWARTIER, at(2026, 11, 7, 13, 59, 59)).map(|v| jinja::round(v, 6)), Some(0.2));
        let mid = at(2026, 11, 7, 14, 0, 0) + Duration::milliseconds(500);
        assert_eq!(um.reading(KWARTIER, mid), Some(0.0));
        um.update(KWARTIER, at(2026, 11, 7, 14, 0, 1), V(500.02));
        assert_eq!(um.reading(KWARTIER, at(2026, 11, 7, 14, 0, 1)).map(|v| jinja::round(v, 6)), Some(0.01));
        assert_eq!(jinja::round(um.last_period, 6), 0.2);
        assert_eq!(um.last_reset, Some(at(2026, 11, 7, 14, 0, 0)));
    }

    /// A meter that missed several boundaries (app down) resets once per boundary, like HA's
    /// timer catching up: the value ends at 0 and `last_period` holds the last (empty) cycle.
    /// A meter seeded without `last_reset` adopts the current cycle start and does not reset.
    #[test]
    fn missed_boundaries_and_unknown_last_reset() {
        let mut um = UtilityMeter::seeded(3.0, Some(at(2026, 11, 5, 0, 0, 0)));
        um.update(DAG, at(2026, 11, 7, 8, 0, 0), Unknown);
        assert_eq!(um.value, Some(0.0));
        assert_eq!(um.last_period, 0.0);
        assert_eq!(um.last_reset, Some(at(2026, 11, 7, 0, 0, 0)));

        let mut um = UtilityMeter::seeded(3.0, None);
        um.update(DAG, at(2026, 11, 7, 8, 0, 0), Unknown);
        assert_eq!(um.value, Some(3.0));
        assert_eq!(um.last_reset, Some(at(2026, 11, 7, 0, 0, 0)));
    }

    /// Cycle boundaries in local time, across DST and the year end.
    #[test]
    fn cycle_boundaries_are_local() {
        let q = Cycle::QuarterHourly;
        assert_eq!(q.next_reset_after(at(2026, 11, 7, 13, 45, 0)), at(2026, 11, 7, 14, 0, 0));
        assert_eq!(q.period_start(at(2026, 11, 7, 13, 59, 59)), at(2026, 11, 7, 13, 45, 0));
        let d = Cycle::Daily;
        // 2026-10-25 has 25 hours.
        let next = d.next_reset_after(at(2026, 10, 25, 0, 0, 0));
        assert_eq!(next, at(2026, 10, 26, 0, 0, 0));
        assert_eq!((next - at(2026, 10, 25, 0, 0, 0)).num_hours(), 25);
        assert_eq!(d.period_start(at(2026, 3, 29, 23, 59, 0)), at(2026, 3, 29, 0, 0, 0));
        let m = Cycle::Monthly;
        assert_eq!(m.next_reset_after(at(2026, 11, 7, 12, 0, 0)), at(2026, 12, 1, 0, 0, 0));
        assert_eq!(m.next_reset_after(at(2026, 12, 31, 23, 0, 0)), at(2027, 1, 1, 0, 0, 0));
        assert_eq!(m.period_start(at(2026, 11, 7, 12, 0, 0)), at(2026, 11, 1, 0, 0, 0));
    }

    const W_KWH: IntegrationConfig = integration::KWH_FROM_W;
    const EV: IntegrationConfig = integration::EV;

    /// Trapezoidal W → kWh: 1000 W then 2000 W 36 s later is 36·1500 = 54 000 Ws =
    /// 0.015 kWh. The first event after start integrates nothing (state stays unknown).
    #[test]
    fn trapezoidal_integration_of_watts_to_kwh() {
        let mut i = Integrator::default();
        i.update(W_KWH, secs(0), Some(1000.0));
        assert_eq!(i.reading(W_KWH), None);
        i.update(W_KWH, secs(36), Some(2000.0));
        assert_eq!(i.reading(W_KWH), Some(0.015));
    }

    /// Left Riemann kW → kWh (EV integrals): 3.6 kW then 7.2 kW 10 s later uses the old
    /// value: 10·3.6/3600 = 0.01 kWh. No rounding on these.
    #[test]
    fn left_integration_uses_the_previous_value() {
        let mut i = Integrator::seeded(100.0);
        i.update(EV, secs(0), Some(3.6));
        assert_eq!(i.reading(EV), Some(100.0));
        i.update(EV, secs(10), Some(7.2));
        assert!((i.reading(EV).unwrap() - 100.01).abs() < 1e-12);
    }

    /// `max_sub_interval` 1 min: 1200 W held, next event 150 s later at 2400 W. HA integrates
    /// the held 1200 W at +60 and +120 (72 000 Ws each), then the event integrates the last
    /// 30 s trapezoidally: 30·(1200 + 2400)/2 = 54 000 Ws. Total 198 000 Ws = 0.055 kWh.
    #[test]
    fn max_sub_interval_integrates_the_held_value() {
        let mut i = Integrator::default();
        i.update(W_KWH, secs(0), Some(1200.0));
        i.update(W_KWH, secs(150), Some(2400.0));
        assert_eq!(i.reading(W_KWH), Some(0.055));
        assert!(!i.time_triggered);
        // Exactly one interval later the timeout runs before the event: 60·2400 = 0.04 kWh,
        // and the event then adds 0 s.
        i.update(W_KWH, secs(210), Some(0.0));
        assert_eq!(i.reading(W_KWH), Some(0.095));
    }

    /// An unavailable source stops the integral: nothing for the gap, no timeouts while
    /// unavailable, and the first reading after it only re-baselines. 1000 W for 10 s, a
    /// 120 s gap, then 10 s at 1000 W: 2·10 000 Ws = 0.00556 kWh → 0.006 rounded.
    #[test]
    fn unavailable_source_breaks_the_integral() {
        let mut i = Integrator::default();
        i.update(W_KWH, secs(0), Some(1000.0));
        i.update(W_KWH, secs(10), Some(1000.0));
        i.update(W_KWH, secs(20), None);
        i.update(W_KWH, secs(140), Some(1000.0));
        i.update(W_KWH, secs(150), Some(1000.0));
        assert_eq!(i.reading(W_KWH), Some(0.006));
        assert!((i.total.unwrap() - 20_000.0 / 3_600_000.0).abs() < 1e-15);
    }

    /// statistics median, `precision: 0`: odd count takes the middle sample, even count the
    /// mean of the middle two, rounded half to even: (100 + 101)/2 = 100.5 → 100,
    /// (101 + 102)/2 = 101.5 → 102.
    #[test]
    fn median_of_odd_and_even_counts_rounds_half_to_even() {
        let mut m = Median::default();
        assert_eq!(m.reading(secs(0)), None);
        m.update(secs(0), Some(100.0));
        m.update(secs(1), Some(101.0));
        assert_eq!(m.reading(secs(1)), Some(100.0));
        m.update(secs(2), Some(5000.0));
        assert_eq!(m.reading(secs(2)), Some(101.0));
        let mut m = Median::default();
        m.update(secs(0), Some(102.0));
        m.update(secs(1), Some(101.0));
        assert_eq!(m.reading(secs(1)), Some(102.0));
    }

    /// `max_age` 10 s: a sample older than 10 s is dropped, at the next event or when read;
    /// with `keep_last_sample` the newest sample survives on its own forever.
    #[test]
    fn median_window_is_ten_seconds_and_keeps_the_last_sample() {
        let mut m = Median::default();
        m.update(secs(0), Some(1000.0));
        m.update(secs(1), Some(2000.0));
        m.update(secs(2), Some(3000.0));
        assert_eq!(m.reading(secs(10)), Some(2000.0));
        // At 10.5 s the 0 s sample is 10.5 s old → median(2000, 3000) = 2500.
        assert_eq!(m.reading(secs(10) + Duration::milliseconds(500)), Some(2500.0));
        assert_eq!(m.reading(secs(500)), Some(3000.0));
        m.update(secs(500), Some(10.0));
        assert_eq!(m.states.len(), 1);
        assert_eq!(m.reading(secs(500)), Some(10.0));
    }

    /// `sampling_size` 60: the 61st sample pushes out the oldest.
    #[test]
    fn median_buffer_holds_sixty_samples() {
        let mut m = Median::default();
        let base = secs(0);
        for k in 0..61 {
            m.update(base + Duration::milliseconds(100 * k), Some(k as f64));
        }
        assert_eq!(m.states.len(), MEDIAN_SAMPLES);
        assert_eq!(m.states.front(), Some(&1.0));
        // Samples 1..=60 → (30 + 31)/2 = 30.5 → 30.
        assert_eq!(m.reading(base + Duration::seconds(6)), Some(30.0));
    }

    /// An unavailable P1 makes the median unavailable; the next reading brings it back.
    #[test]
    fn median_follows_source_availability() {
        let mut m = Median::default();
        m.update(secs(0), Some(500.0));
        m.update(secs(1), None);
        assert_eq!(m.reading(secs(1)), None);
        m.update(secs(2), Some(700.0));
        assert_eq!(m.reading(secs(2)), Some(600.0));
    }

    /// The whole set, one tick at a time: integrals feed their daily meters (by rounded
    /// state), the P1 registers feed the quarter/import/export meters, the car rates feed the
    /// car meters. Two ticks 36 s apart at a steady 1000 W PV and house load, 2000 W battery
    /// charge and 3.6 kW to the Buzz, at 0.5 EUR/h:
    /// - W → kWh integrals: 36·1000/3.6e6 = 0.01 kWh (battery 0.02); daily meters follow
    ///   from 0 → 0.01.
    /// - Buzz 3.6 kW · 36 s = 0.036 kWh, cost 0.5·36/3600 = 0.005 EUR.
    /// - P1 import 100.000 → 100.010: quarter and day +0.01.
    #[test]
    fn update_drives_every_meter_from_world_and_derived() {
        let mut st = MetersState::default();
        let mut w = World::default();
        w.p1.power_w = Some(1000.0);
        w.p1.import_kwh = Some(100.0);
        w.p1.export_kwh = Some(50.0);
        let d = Derived {
            pv_vermogen_totaal_w: 1000.0,
            huisverbruik_vermogen_w: 1000.0,
            batterij_laadvermogen_w: 2000.0,
            kost_per_uur_eur: 0.5,
            buzz_laadvermogen_totaal_kw: 3.6,
            buzz_laadkost_per_uur_eur: 0.5,
            ev3_laadvermogen_zon_kw: 1.0,
            ..Derived::default()
        };
        // An integral only sees an event when its source value changes: nudge every rate by
        // a negligible amount so each tick is one.
        let nudged = |k: f64| Derived {
            pv_vermogen_totaal_w: d.pv_vermogen_totaal_w + k * 1e-9,
            huisverbruik_vermogen_w: d.huisverbruik_vermogen_w + k * 1e-9,
            batterij_laadvermogen_w: d.batterij_laadvermogen_w + k * 1e-9,
            kost_per_uur_eur: d.kost_per_uur_eur + k * 1e-12,
            buzz_laadvermogen_totaal_kw: d.buzz_laadvermogen_totaal_kw + k * 1e-12,
            buzz_laadkost_per_uur_eur: d.buzz_laadkost_per_uur_eur + k * 1e-12,
            ev3_laadvermogen_zon_kw: d.ev3_laadvermogen_zon_kw + k * 1e-12,
            ..d.clone()
        };
        update(secs(0), &w, &d, &mut st);
        let first = st.clone();
        w.p1.import_kwh = Some(100.01);
        update(secs(36), &w, &nudged(1.0), &mut st);
        update(secs(72), &w, &nudged(2.0), &mut st);
        let now = secs(72);
        let r6 = |v: Option<f64>| v.map(|v| jinja::round(v, 6));
        assert_eq!(st.pv_energie_totaal.reading(W_KWH), Some(0.02));
        assert_eq!(st.batterij_geladen_energie.reading(W_KWH), Some(0.04));
        assert_eq!(st.batterij_ontladen_energie.reading(W_KWH), Some(0.0));
        assert_eq!(r6(st.pv_energie_dag.reading(DAG, now)), Some(0.01));
        assert_eq!(r6(st.huisverbruik_dag.reading(DAG, now)), Some(0.01));
        assert_eq!(r6(st.batterij_geladen_dag.reading(DAG, now)), Some(0.02));
        assert_eq!(r6(st.batterij_ontladen_dag.reading(DAG, now)), Some(0.0));
        assert_eq!(st.energiekost_cumulatief.reading(integration::ENERGIEKOST), Some(0.01));
        assert_eq!(r6(st.energiekost_vandaag.reading(utility::ENERGIEKOST_VANDAAG, now)), Some(0.005));
        assert_eq!(r6(st.kwartier_import.reading(KWARTIER, now)), Some(0.01));
        assert_eq!(r6(st.netimport_dag.reading(DAG, now)), Some(0.01));
        assert_eq!(r6(st.injectie_dag.reading(DAG, now)), Some(0.0));
        assert_eq!(r6(st.buzz.laadenergie_totaal.reading(EV)), Some(0.072));
        assert_eq!(r6(st.buzz.laadkost_net.reading(EV)), Some(0.01));
        assert_eq!(r6(st.buzz.laadenergie_dag.reading(utility::EV_DAG, now)), Some(0.036));
        assert_eq!(r6(st.buzz.laadenergie_maand.reading(utility::EV_MAAND, now)), Some(0.036));
        assert_eq!(r6(st.buzz.laadkost_dag.reading(utility::EV_DAG, now)), Some(0.005));
        assert_eq!(r6(st.buzz.laadkost_maand.reading(utility::EV_MAAND, now)), Some(0.005));
        assert_eq!(r6(st.ev3.laadenergie_zon.reading(EV)), Some(0.02));
        // No event for a steady 0 kW: only the 1 min timer ran (at 60 s).
        assert_eq!(st.ev3.laadenergie_net.reading(EV), Some(0.0));
        assert_eq!(st.p1_mediaan.reading(now), Some(1000.0));

        let json = serde_json::to_string(&first).unwrap();
        assert_eq!(serde_json::from_str::<MetersState>(&json).unwrap(), first);
        assert_eq!(serde_json::from_str::<MetersState>("{}").unwrap(), MetersState::default());
    }

    /// `Batterij geladen energie` (integration, trapezoidal, source `sensor.batterij_laadvermogen`,
    /// a template): HA only writes a template when its value changes, so the integral's events
    /// are the source's changes and a step is a trapezoid back to the previous event. Recorder,
    /// 2026-11-07 01:26:06: the source went 0 → 2126 W (and 2126 → 666 13 s later). Ticked every
    /// second: 0 W for 30 s, 2126 W for 15 s, then 0 W. HA: (0+2126)/2·30 + (2126+0)/2·15 =
    /// 47835 Ws = 0.0132875 kWh. Integrating every tick as an event gave 2126·14 + 2·1063 =
    /// 31890 Ws, a third less.
    #[test]
    fn integral_events_are_source_changes_not_ticks() {
        let cfg = integration::KWH_FROM_W;
        let mut i = Integrator::default();
        for t in 0..=60 {
            let v = if (30..45).contains(&t) { 2126.0 } else { 0.0 };
            i.tick(cfg, secs(t), Some(v));
        }
        assert!((i.total.unwrap() - 47835.0 / 3.6e6).abs() < 1e-12, "{:?}", i.total);
        assert_eq!(i.reading(cfg), Some(0.013));
        // Unchanged for over a minute: the max_sub_interval timer still integrates the held value.
        let mut j = Integrator::default();
        for t in 0..=125 {
            j.tick(cfg, secs(t), Some(3600.0));
        }
        assert_eq!(j.last_event, Some(secs(0)));
        assert!((j.total.unwrap() - 0.12).abs() < 1e-12, "two timer intervals of 3.6 kW: {:?}", j.total);
    }

    /// `P1 mediaan 90s` samples per P1 *event*: HA's HomeWizard entity reports every ~5 s
    /// (recorder 2026-11-03 02:15:23/28/33/38). Ticked every second, a reading held for 9 s is
    /// one sample, not nine: 100 W from 0 s, 200 W at 9 s, 300 W at 10 s gives the median of
    /// [100, 200, 300] = 200 W; one sample per tick would give 100 W.
    #[test]
    fn median_samples_per_p1_reading_not_per_tick() {
        let mut m = Median::default();
        for t in 0..=10 {
            let v = match t {
                0..=8 => 100.0,
                9 => 200.0,
                _ => 300.0,
            };
            m.tick(secs(t), Some(v));
        }
        assert_eq!(m.states, VecDeque::from([100.0, 200.0, 300.0]));
        assert_eq!(m.reading(secs(10)), Some(200.0));
        // Unavailable and back: the same value after a dropout is a new sample.
        m.tick(secs(11), None);
        m.tick(secs(12), Some(300.0));
        assert_eq!(m.states, VecDeque::from([200.0, 300.0, 300.0]));
    }
}
