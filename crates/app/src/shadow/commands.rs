//! Rust's commands against HA's actuations.
//!
//! Both sides are reduced to [`Act`]s: a key (`evse.override_a`, `solis.rc_mode`, ...) and a
//! value. Rust's come from its commands and latch flips in Memory; HA's from state changes of
//! the output entities and from the SmartEVSE MQTT set topics.
//!
//! - A Rust act matches if HA's entity shows that value (now, or at any moment while the act
//!   is pending), or HA made the same act within ±window. Re-sending a value HA already has is
//!   therefore a match, as it should be.
//! - An HA act matches if Rust made the same act within ±window.
//!
//! An act still unmatched when its window has passed is a mismatch (`rust_only`/`ha_only`).

use std::collections::{BTreeMap, VecDeque};

use energie_core::command::{AircoUnit, Command};
use energie_core::memory::Memory;
use energie_core::time::{Ts, secs};
use serde::Serialize;

use super::Val;
use super::log::MismatchLog;
use crate::config::Tolerance;
use crate::ha::entity::{EntityState, Lookup, StateChange};
use crate::ha::mapping::{evse_mode_label, rc_mode_label};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Act {
    pub key: &'static str,
    pub val: Val,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActKind {
    Num,
    Text,
    Bool,
}

/// `(HA entity, act key, how its state reads)` for every observable actuation.
const ACT_ENTITIES: [(&str, &str, ActKind); 19] = [
    ("number.smartevse_1234_charge_current_override", "evse.override_a", ActKind::Num),
    ("select.smartevse_1234_mode", "evse.mode", ActKind::Text),
    ("select.ev_charging_cable_charging_operation", "cable.operation", ActKind::Text),
    ("number.ev_charging_cable_charge_current", "cable.current_a", ActKind::Num),
    ("select.solis_s6_eh1p_rc_force_charge_discharge_2", "solis.rc_mode", ActKind::Text),
    ("number.solis_s6_eh1p_rc_force_battery_charge_power_2", "solis.rc_charge_w", ActKind::Num),
    ("number.solis_s6_eh1p_rc_force_battery_discharge_power_2", "solis.rc_discharge_w", ActKind::Num),
    ("number.solis_s6_eh1p_rc_timeout_2", "solis.rc_timeout_min", ActKind::Num),
    ("switch.solis_s6_eh1p_allow_grid_to_charge_the_battery_2", "solis.allow_grid_charge", ActKind::Bool),
    ("switch.solis_s6_eh1p_grid_feed_in_power_limit_switch_2", "solis.feed_in_limit", ActKind::Bool),
    ("number.solis_s6_eh1p_backflow_power_2", "solis.backflow_w", ActKind::Num),
    ("switch.airco_beneden_power", "airco.beneden", ActKind::Bool),
    ("switch.airco_zolder_power", "airco.zolder", ActKind::Bool),
    ("input_boolean.batterij_net_laden", "latch.batterij_net_laden", ActKind::Bool),
    ("input_boolean.soluna_absorptie_bezig", "latch.soluna_absorptie_bezig", ActKind::Bool),
    ("input_boolean.ev_hoger_laadstil", "latch.ev_hoger_laadstil", ActKind::Bool),
    ("input_text.smartevse_plug_laatst_bekend", "latch.smartevse_plug_laatst_bekend", ActKind::Text),
    ("input_text.smartevse_modus_laatst_bekend", "latch.smartevse_modus_laatst_bekend", ActKind::Text),
    ("input_number.batterij_setpoint_bias", "latch.batterij_setpoint_bias", ActKind::Num),
];

/// The entities whose state changes are HA actuations (the store watches these).
pub fn act_entities() -> Vec<String> {
    ACT_ENTITIES.iter().map(|(e, _, _)| e.to_string()).collect()
}

fn read(kind: ActKind, s: &str) -> Option<Val> {
    match kind {
        ActKind::Num => s.trim().parse().ok().map(Val::Num),
        ActKind::Text => Some(Val::Text(s.to_string())),
        ActKind::Bool => match s {
            "on" => Some(Val::Bool(true)),
            "off" => Some(Val::Bool(false)),
            _ => None,
        },
    }
}

fn readable(s: &Option<String>) -> Option<&str> {
    s.as_deref().filter(|s| !matches!(*s, "unavailable" | "unknown" | ""))
}

/// The act key of an actuation entity.
pub fn act_key(entity: &str) -> Option<&'static str> {
    ACT_ENTITIES.iter().find(|(e, _, _)| *e == entity).map(|(_, k, _)| *k)
}

/// The act an HA entity's state stands for (`None`: not an actuation entity, or the state
/// does not read as one, e.g. `unavailable`).
pub fn read_act(entity: &str, state: &str) -> Option<Act> {
    let (_, key, kind) = ACT_ENTITIES.iter().find(|(e, _, _)| *e == entity)?;
    Some(Act { key, val: read(*kind, readable(&Some(state.to_string()))?)? })
}

/// An HA state change as an act. Changes from or to `unavailable` are device dropouts, not
/// actuations.
pub fn change_act(ch: &StateChange) -> Option<Act> {
    let (_, key, kind) = ACT_ENTITIES.iter().find(|(e, _, _)| *e == ch.entity_id)?;
    readable(&ch.old)?;
    let val = read(*kind, readable(&ch.new)?)?;
    Some(Act { key, val })
}

/// A message on `SmartEVSE/1234/Set/...` as an act. `CurrentOverride` is in tenths of an amp.
pub fn mqtt_act(topic: &str, payload: &str) -> Option<Act> {
    match topic.rsplit('/').next()? {
        "CurrentOverride" => {
            let deci: f64 = payload.trim().parse().ok()?;
            Some(Act { key: "evse.override_a", val: Val::Num(deci / 10.0) })
        }
        "Mode" => Some(Act { key: "evse.mode", val: Val::Text(payload.trim().to_string()) }),
        _ => None,
    }
}

/// Rust's command as acts (unobservable commands — reconnects, price refresh — have none).
pub fn command_acts(c: &Command) -> Vec<Act> {
    use Command::*;
    let a = |key, val| vec![Act { key, val }];
    match c {
        EvseCurrentDeciA(d) => a("evse.override_a", Val::Num(f64::from(*d) / 10.0)),
        EvseMode(m) => a("evse.mode", Val::Text(evse_mode_label(*m).into())),
        CableStart { amps } => vec![
            Act { key: "cable.operation", val: Val::Text("Start charging".into()) },
            Act { key: "cable.current_a", val: Val::Num(f64::from(*amps)) },
        ],
        CableStop => a("cable.operation", Val::Text("Stop charging".into())),
        CableSetCurrent(x) => a("cable.current_a", Val::Num(f64::from(*x))),
        SolisRcMode(m) => a("solis.rc_mode", Val::Text(rc_mode_label(*m).into())),
        SolisRcChargeW(w) => a("solis.rc_charge_w", Val::Num(f64::from(*w))),
        SolisRcDischargeW(w) => a("solis.rc_discharge_w", Val::Num(f64::from(*w))),
        SolisRcTimeoutMin(m) => a("solis.rc_timeout_min", Val::Num(f64::from(*m))),
        SolisAllowGridCharge(on) => a("solis.allow_grid_charge", Val::Bool(*on)),
        SolisFeedInLimit(on) => a("solis.feed_in_limit", Val::Bool(*on)),
        SolisBackflowW(w) => a("solis.backflow_w", Val::Num(f64::from(*w))),
        Airco { unit: AircoUnit::Beneden, on } => a("airco.beneden", Val::Bool(*on)),
        Airco { unit: AircoUnit::Zolder, on } => a("airco.zolder", Val::Bool(*on)),
        CableReconnect | SolisReconnect | RefreshPrices | Notify { .. } | Persistent { .. } => vec![],
    }
}

/// Latch flips in Memory across one engine tick, as acts.
pub fn latch_acts(before: &Memory, after: &Memory) -> Vec<Act> {
    let mut v = Vec::new();
    let mut flip = |key, b: bool, a: bool| {
        if a != b {
            v.push(Act { key, val: Val::Bool(a) });
        }
    };
    flip("latch.batterij_net_laden", before.batterij_net_laden, after.batterij_net_laden);
    flip("latch.soluna_absorptie_bezig", before.soluna_absorptie_bezig, after.soluna_absorptie_bezig);
    flip("latch.ev_hoger_laadstil", before.ev_hoger_laadstil, after.ev_hoger_laadstil);
    if before.smartevse_plug_laatst_bekend != after.smartevse_plug_laatst_bekend {
        v.push(Act {
            key: "latch.smartevse_plug_laatst_bekend",
            val: Val::Text(after.smartevse_plug_laatst_bekend.clone()),
        });
    }
    if before.smartevse_modus_laatst_bekend != after.smartevse_modus_laatst_bekend {
        v.push(Act {
            key: "latch.smartevse_modus_laatst_bekend",
            val: Val::Text(after.smartevse_modus_laatst_bekend.clone()),
        });
    }
    if (before.batterij_setpoint_bias - after.batterij_setpoint_bias).abs() > 1e-9 {
        v.push(Act { key: "latch.batterij_setpoint_bias", val: Val::Num(after.batterij_setpoint_bias) });
    }
    v
}

/// HA's current value for an act key.
fn ha_current(ha: &dyn Lookup, key: &str) -> Option<Val> {
    let (entity, _, kind) = ACT_ENTITIES.iter().find(|(_, k, _)| *k == key)?;
    ha.get(entity).and_then(EntityState::value).and_then(|s| read(*kind, s))
}

/// Within 0.05 (deci-amp rounding) for numbers.
const TOL: Tolerance = Tolerance { abs: 0.05, rel: 0.0 };

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct KeyStats {
    pub key: &'static str,
    pub rust: u64,
    pub rust_matched: u64,
    pub ha: u64,
    pub ha_matched: u64,
    pub agreement_pct: Option<f64>,
    pub last_mismatch: Option<CmdMismatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CmdMismatch {
    pub at: String,
    /// `rust_only` or `ha_only`.
    pub side: &'static str,
    pub val: Val,
}

struct Pending {
    at: Ts,
    act: Act,
    matched: bool,
}

pub struct CommandComparator {
    window_s: f64,
    rust: VecDeque<Pending>,
    ha: VecDeque<Pending>,
    stats: BTreeMap<&'static str, KeyStats>,
}

impl CommandComparator {
    pub fn new(window_s: i64) -> Self {
        CommandComparator {
            window_s: window_s as f64,
            rust: VecDeque::new(),
            ha: VecDeque::new(),
            stats: BTreeMap::new(),
        }
    }

    fn key(&mut self, key: &'static str) -> &mut KeyStats {
        self.stats.entry(key).or_insert(KeyStats {
            key,
            rust: 0,
            rust_matched: 0,
            ha: 0,
            ha_matched: 0,
            agreement_pct: None,
            last_mismatch: None,
        })
    }

    pub fn observe(
        &mut self,
        now: Ts,
        rust: Vec<Act>,
        ha_acts: Vec<(Ts, Act)>,
        ha: &dyn Lookup,
        log: &mut MismatchLog,
    ) {
        for act in rust {
            self.key(act.key).rust += 1;
            self.rust.push_back(Pending { at: now, act, matched: false });
        }
        for (at, act) in ha_acts {
            self.key(act.key).ha += 1;
            self.ha.push_back(Pending { at, act, matched: false });
        }
        let w = self.window_s;
        let near = |a: Ts, b: Ts| secs(a, b).abs() <= w + 1e-9;
        // Match Rust acts: HA's entity shows the value, or HA made the same act nearby.
        for i in 0..self.rust.len() {
            let r = &self.rust[i];
            if r.matched {
                continue;
            }
            let shows = ha_current(ha, r.act.key).is_some_and(|v| r.act.val.matches(&v, TOL));
            let did = self
                .ha
                .iter()
                .any(|h| h.act.key == r.act.key && near(h.at, r.at) && h.act.val.matches(&r.act.val, TOL));
            if shows || did {
                self.rust[i].matched = true;
                let key = self.rust[i].act.key;
                self.key(key).rust_matched += 1;
            }
        }
        // Match HA acts: Rust made the same act nearby.
        for i in 0..self.ha.len() {
            let h = &self.ha[i];
            if h.matched {
                continue;
            }
            if self
                .rust
                .iter()
                .any(|r| r.act.key == h.act.key && near(r.at, h.at) && r.act.val.matches(&h.act.val, TOL))
            {
                self.ha[i].matched = true;
                let key = self.ha[i].act.key;
                self.key(key).ha_matched += 1;
            }
        }
        self.expire(now, log);
    }

    /// Drop acts older than twice the window; unmatched ones become mismatches.
    fn expire(&mut self, now: Ts, log: &mut MismatchLog) {
        let horizon = 2.0 * self.window_s;
        for side in ["rust_only", "ha_only"] {
            loop {
                let q = if side == "rust_only" { &mut self.rust } else { &mut self.ha };
                let Some(front) = q.front() else { break };
                if secs(front.at, now) <= horizon {
                    break;
                }
                let p = q.pop_front().expect("front exists");
                if !p.matched {
                    let m = CmdMismatch { at: p.at.to_rfc3339(), side, val: p.act.val.clone() };
                    log.write(&serde_json::json!({"kind": "command", "at": m.at, "key": p.act.key, "side": side, "val": m.val}));
                    self.key(p.act.key).last_mismatch = Some(m);
                }
            }
        }
    }

    pub fn stats(&self) -> Vec<KeyStats> {
        let mut v: Vec<KeyStats> = self.stats.values().cloned().collect();
        for k in &mut v {
            k.agreement_pct = super::pct(k.rust_matched + k.ha_matched, k.rust + k.ha);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::{EntityState, index};
    use crate::plug::tests::t0;
    use chrono::Duration;
    use energie_core::world::{EvseMode as Em, RcMode};

    fn ch(entity: &str, old: Option<&str>, new: Option<&str>) -> StateChange {
        StateChange { at: t0(), entity_id: entity.into(), old: old.map(Into::into), new: new.map(Into::into) }
    }

    /// Every command that HA can observe maps to the act HA's entity change would show.
    #[test]
    fn commands_become_acts() {
        use Command::*;
        let one = |c: Command| command_acts(&c).into_iter().map(|a| (a.key, a.val)).collect::<Vec<_>>();
        assert_eq!(one(EvseCurrentDeciA(148)), vec![("evse.override_a", Val::Num(14.8))]);
        assert_eq!(one(EvseMode(Em::Normal)), vec![("evse.mode", Val::Text("Normal".into()))]);
        assert_eq!(
            one(CableStart { amps: 9 }),
            vec![("cable.operation", Val::Text("Start charging".into())), ("cable.current_a", Val::Num(9.0))]
        );
        assert_eq!(one(CableStop), vec![("cable.operation", Val::Text("Stop charging".into()))]);
        assert_eq!(one(CableSetCurrent(7)), vec![("cable.current_a", Val::Num(7.0))]);
        assert_eq!(
            one(SolisRcMode(RcMode::ForceCharge)),
            vec![("solis.rc_mode", Val::Text("Solis RC Force Battery Charge".into()))]
        );
        assert_eq!(one(SolisRcChargeW(1200)), vec![("solis.rc_charge_w", Val::Num(1200.0))]);
        assert_eq!(one(SolisRcDischargeW(800)), vec![("solis.rc_discharge_w", Val::Num(800.0))]);
        assert_eq!(one(SolisRcTimeoutMin(10)), vec![("solis.rc_timeout_min", Val::Num(10.0))]);
        assert_eq!(one(SolisAllowGridCharge(true)), vec![("solis.allow_grid_charge", Val::Bool(true))]);
        assert_eq!(one(SolisFeedInLimit(false)), vec![("solis.feed_in_limit", Val::Bool(false))]);
        assert_eq!(one(SolisBackflowW(0)), vec![("solis.backflow_w", Val::Num(0.0))]);
        assert_eq!(one(Airco { unit: AircoUnit::Beneden, on: true }), vec![("airco.beneden", Val::Bool(true))]);
        assert_eq!(one(Airco { unit: AircoUnit::Zolder, on: false }), vec![("airco.zolder", Val::Bool(false))]);
        for c in [CableReconnect, SolisReconnect, RefreshPrices, Command::notify("a", "b")] {
            assert!(command_acts(&c).is_empty());
        }
    }

    /// HA state changes are acts, except dropouts to/from `unavailable` and unwatched entities.
    #[test]
    fn state_changes_become_acts() {
        let a = change_act(&ch("number.smartevse_1234_charge_current_override", Some("14.8"), Some("15.2"))).unwrap();
        assert_eq!((a.key, a.val), ("evse.override_a", Val::Num(15.2)));
        let b = change_act(&ch("input_boolean.batterij_net_laden", Some("off"), Some("on"))).unwrap();
        assert_eq!(b.val, Val::Bool(true));
        assert!(change_act(&ch("number.ev_charging_cable_charge_current", Some("unavailable"), Some("6"))).is_none());
        assert!(change_act(&ch("number.ev_charging_cable_charge_current", Some("6"), Some("unavailable"))).is_none());
        assert!(change_act(&ch("number.ev_charging_cable_charge_current", None, Some("6"))).is_none());
        assert!(change_act(&ch("switch.airco_zolder_power", Some("off"), Some("weird"))).is_none());
        assert!(change_act(&ch("sensor.p1_meter_vermogen", Some("1"), Some("2"))).is_none());
        assert_eq!(act_entities().len(), ACT_ENTITIES.len());
        let r = read_act("input_number.batterij_setpoint_bias", "200.0").unwrap();
        assert_eq!((r.key, r.val), ("latch.batterij_setpoint_bias", Val::Num(200.0)));
        assert!(read_act("input_text.smartevse_plug_laatst_bekend", "unavailable").is_none());
        assert!(read_act("sensor.p1_meter_vermogen", "1").is_none());
        assert_eq!(act_key("select.smartevse_1234_mode"), Some("evse.mode"));
        assert_eq!(act_key("sensor.p1_meter_vermogen"), None);
    }

    /// MQTT set topics become acts; the override payload is in tenths of an amp.
    #[test]
    fn mqtt_set_topics_become_acts() {
        let a = mqtt_act("SmartEVSE/1234/Set/CurrentOverride", "145").unwrap();
        assert_eq!((a.key, a.val), ("evse.override_a", Val::Num(14.5)));
        let m = mqtt_act("SmartEVSE/1234/Set/Mode", "Normal").unwrap();
        assert_eq!(m.val, Val::Text("Normal".into()));
        assert!(mqtt_act("SmartEVSE/1234/Set/CurrentOverride", "x").is_none());
        assert!(mqtt_act("SmartEVSE/1234/Set/Other", "1").is_none());
    }

    /// Latch flips in Memory are acts; unchanged latches are not.
    #[test]
    fn latch_flips_become_acts() {
        let a = Memory::default();
        let mut b = a.clone();
        b.batterij_net_laden = true;
        b.soluna_absorptie_bezig = true;
        b.ev_hoger_laadstil = true;
        b.smartevse_plug_laatst_bekend = "Connected".into();
        b.batterij_setpoint_bias = 12.5;
        b.smartevse_modus_laatst_bekend = "Normal".into();
        let acts = latch_acts(&a, &b);
        assert_eq!(acts.len(), 6);
        assert_eq!(acts[3].val, Val::Text("Connected".into()));
        assert_eq!(acts[4], Act { key: "latch.smartevse_modus_laatst_bekend", val: Val::Text("Normal".into()) });
        assert_eq!(acts[5], Act { key: "latch.batterij_setpoint_bias", val: Val::Num(12.5) });
        assert!(latch_acts(&a, &a).is_empty());
    }

    fn log() -> (MismatchLog, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        (MismatchLog::new(Some(d.path().join("m")), 1 << 20), d)
    }

    /// Rust and HA doing the same thing within the window agree; a Rust command HA already
    /// shows (a re-send) agrees too.
    #[test]
    fn same_actuation_within_the_window_matches() {
        let (mut l, _d) = log();
        let mut c = CommandComparator::new(40);
        let ha = index(vec![EntityState::new("number.smartevse_1234_charge_current_override", "14.8")]);
        // Rust sends 15.2 at t0; HA's automation publishes it 12 s later.
        c.observe(t0(), command_acts(&Command::EvseCurrentDeciA(152)), vec![], &ha, &mut l);
        let later = t0() + Duration::seconds(12);
        let a = mqtt_act("SmartEVSE/1234/Set/CurrentOverride", "152").unwrap();
        c.observe(later, vec![], vec![(later, a)], &ha, &mut l);
        // A re-send of what HA shows.
        c.observe(later, command_acts(&Command::EvseCurrentDeciA(148)), vec![], &ha, &mut l);
        for i in 0..100 {
            c.observe(later + Duration::seconds(i), vec![], vec![], &ha, &mut l);
        }
        let s = &c.stats()[0];
        assert_eq!((s.rust, s.rust_matched, s.ha, s.ha_matched), (2, 2, 1, 1));
        assert_eq!(s.agreement_pct, Some(100.0));
        assert_eq!(l.lines, 0);
        assert!(c.rust.is_empty() && c.ha.is_empty());
    }

    /// Acts only one side made are mismatches once their window has passed.
    #[test]
    fn one_sided_actuations_are_mismatches_after_the_window() {
        let (mut l, _d) = log();
        let mut c = CommandComparator::new(10);
        let ha = index(vec![EntityState::new("select.solis_s6_eh1p_rc_force_charge_discharge_2", "None")]);
        c.observe(t0(), command_acts(&Command::SolisRcMode(RcMode::ForceDischarge)), vec![], &ha, &mut l);
        let a = change_act(&ch("switch.airco_zolder_power", Some("off"), Some("on"))).unwrap();
        c.observe(t0(), vec![], vec![(t0(), a)], &ha, &mut l);
        c.observe(t0() + Duration::seconds(15), vec![], vec![], &ha, &mut l);
        assert_eq!(l.lines, 0, "still inside 2× window");
        c.observe(t0() + Duration::seconds(25), vec![], vec![], &ha, &mut l);
        assert_eq!(l.lines, 2);
        let st = c.stats();
        let rc = st.iter().find(|k| k.key == "solis.rc_mode").unwrap();
        assert_eq!(rc.last_mismatch.as_ref().unwrap().side, "rust_only");
        assert_eq!(rc.agreement_pct, Some(0.0));
        let ac = st.iter().find(|k| k.key == "airco.zolder").unwrap();
        assert_eq!(ac.last_mismatch.as_ref().unwrap().side, "ha_only");
    }
}
