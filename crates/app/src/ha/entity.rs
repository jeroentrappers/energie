//! HA state objects, typed parsing of their strings, and the in-memory mirror of HA's states.

use std::collections::{HashMap, HashSet, VecDeque};

use chrono::{DateTime, NaiveDateTime, TimeZone};
use energie_core::time::{TZ, Ts};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One entity as HA reports it (`get_states` item, `new_state` of an event).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityState {
    pub entity_id: String,
    pub state: String,
    #[serde(default)]
    pub attributes: Map<String, Value>,
    #[serde(default)]
    pub last_changed: Option<String>,
    #[serde(default)]
    pub last_updated: Option<String>,
}

impl EntityState {
    pub fn new(entity_id: &str, state: &str) -> Self {
        EntityState {
            entity_id: entity_id.into(),
            state: state.into(),
            attributes: Map::new(),
            last_changed: None,
            last_updated: None,
        }
    }

    pub fn with_attr(mut self, k: &str, v: Value) -> Self {
        self.attributes.insert(k.into(), v);
        self
    }

    /// The state, or `None` for `unavailable` / `unknown` / empty.
    pub fn value(&self) -> Option<&str> {
        match self.state.as_str() {
            "unavailable" | "unknown" | "" => None,
            s => Some(s),
        }
    }

    /// The state as a finite number.
    pub fn num(&self) -> Option<f64> {
        self.value().and_then(parse_num)
    }

    /// `on` / `off` as a bool.
    pub fn on_off(&self) -> Option<bool> {
        match self.value()? {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        }
    }

    pub fn attr(&self, k: &str) -> Option<&Value> {
        self.attributes.get(k).filter(|v| !v.is_null())
    }

    /// A numeric attribute (a JSON number or a numeric string).
    pub fn attr_num(&self, k: &str) -> Option<f64> {
        match self.attr(k)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => parse_num(s),
            _ => None,
        }
    }

    pub fn attr_str(&self, k: &str) -> Option<&str> {
        self.attr(k)?.as_str()
    }

    /// `last_changed`, in Brussels time.
    pub fn changed_at(&self) -> Option<Ts> {
        self.last_changed.as_deref().and_then(parse_time)
    }
}

pub fn parse_num(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// HA's time spellings: RFC 3339 (`2026-11-07T12:46:00.45+00:00`; chrono also accepts the
/// space-separated price-slot form `2026-11-07 00:15:00+01:00`) and the naive local `input_datetime` state
/// (`2026-10-30 13:32:52`, Brussels time).
pub fn parse_time(s: &str) -> Option<Ts> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&TZ));
    }
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok()?;
    TZ.from_local_datetime(&naive).earliest()
}

/// Lookup of entities by id: the live store, or a plain map in tests.
pub trait Lookup {
    fn get(&self, id: &str) -> Option<&EntityState>;
    fn all(&self) -> Box<dyn Iterator<Item = &EntityState> + '_>;
}

impl Lookup for HashMap<String, EntityState> {
    fn get(&self, id: &str) -> Option<&EntityState> {
        HashMap::get(self, id)
    }
    fn all(&self) -> Box<dyn Iterator<Item = &EntityState> + '_> {
        Box::new(self.values())
    }
}

/// Build a lookup map from a `get_states` result.
pub fn index(states: Vec<EntityState>) -> HashMap<String, EntityState> {
    states.into_iter().map(|e| (e.entity_id.clone(), e)).collect()
}

/// A state change of a watched entity, kept until the runtime drains it.
#[derive(Debug, Clone, PartialEq)]
pub struct StateChange {
    pub at: Ts,
    pub entity_id: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

/// The mirror of HA's states, shared between the WebSocket client and the runtime.
#[derive(Debug, Default)]
pub struct HaStore {
    pub entities: HashMap<String, EntityState>,
    pub connected: bool,
    /// Full snapshots received (one per successful (re)connect).
    pub snapshots: u64,
    pub events: u64,
    /// Entities whose changes are queued in `changes`.
    pub watch: HashSet<String>,
    pub changes: VecDeque<StateChange>,
}

/// More changes than this and the oldest are dropped (nobody is draining).
const MAX_CHANGES: usize = 10_000;

impl HaStore {
    pub fn watching(ids: impl IntoIterator<Item = String>) -> Self {
        HaStore { watch: ids.into_iter().collect(), ..Default::default() }
    }

    /// Replace everything with a fresh `get_states` result.
    pub fn replace_all(&mut self, states: Vec<EntityState>) {
        self.entities = index(states);
        self.connected = true;
        self.snapshots += 1;
    }

    /// Apply a `state_changed` event at `at`.
    pub fn apply_change(&mut self, at: Ts, entity_id: &str, new: Option<EntityState>) {
        self.events += 1;
        let old = match &new {
            Some(n) => self.entities.insert(entity_id.to_string(), n.clone()),
            None => self.entities.remove(entity_id),
        };
        let old_state = old.map(|o| o.state);
        let new_state = new.map(|n| n.state);
        if self.watch.contains(entity_id) && old_state != new_state {
            if self.changes.len() >= MAX_CHANGES {
                self.changes.pop_front();
            }
            self.changes.push_back(StateChange { at, entity_id: entity_id.into(), old: old_state, new: new_state });
        }
    }

    pub fn drain_changes(&mut self) -> Vec<StateChange> {
        self.changes.drain(..).collect()
    }
}

impl Lookup for HaStore {
    fn get(&self, id: &str) -> Option<&EntityState> {
        self.entities.get(id)
    }
    fn all(&self) -> Box<dyn Iterator<Item = &EntityState> + '_> {
        Box::new(self.entities.values())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `unavailable`/`unknown` are `None`; numbers, on/off and attributes parse typed.
    #[test]
    fn typed_parsing_of_states_and_attributes() {
        assert_eq!(EntityState::new("s.x", "3066.0").num(), Some(3066.0));
        assert_eq!(EntityState::new("s.x", "unavailable").num(), None);
        assert_eq!(EntityState::new("s.x", "unknown").value(), None);
        assert_eq!(EntityState::new("s.x", "").value(), None);
        assert_eq!(EntityState::new("s.x", "abc").num(), None);
        assert_eq!(EntityState::new("s.x", "nan").num(), None);
        assert_eq!(EntityState::new("s.x", "on").on_off(), Some(true));
        assert_eq!(EntityState::new("s.x", "off").on_off(), Some(false));
        assert_eq!(EntityState::new("s.x", "maybe").on_off(), None);
        let e = EntityState::new("n.x", "14.8")
            .with_attr("max", json!(29.0))
            .with_attr("s", json!("16"))
            .with_attr("b", json!(true))
            .with_attr("z", Value::Null)
            .with_attr("t", json!("txt"));
        assert_eq!(e.attr_num("max"), Some(29.0));
        assert_eq!(e.attr_num("s"), Some(16.0));
        assert_eq!(e.attr_num("b"), None);
        assert_eq!(e.attr_num("z"), None);
        assert_eq!(e.attr_num("missing"), None);
        assert_eq!(e.attr_str("t"), Some("txt"));
        assert_eq!(e.attr_str("max"), None);
    }

    /// The three time spellings HA uses all land in Brussels time.
    #[test]
    fn time_spellings() {
        let a = parse_time("2026-11-07T12:46:00+00:00").unwrap();
        assert_eq!(a.to_rfc3339(), "2026-11-07T13:46:00+01:00");
        let b = parse_time("2026-11-07 00:15:00+01:00").unwrap();
        assert_eq!(b.to_rfc3339(), "2026-11-07T00:15:00+01:00");
        let c = parse_time("2026-10-30 13:32:52").unwrap();
        assert_eq!(c.to_rfc3339(), "2026-10-30T13:32:52+01:00");
        assert_eq!(parse_time("gisteren"), None);
        let mut e = EntityState::new("s.x", "1");
        e.last_changed = Some("2026-11-07T12:46:00+00:00".into());
        assert_eq!(e.changed_at(), Some(a));
    }

    /// The store mirrors snapshots and events, and queues changes of watched entities only
    /// when the state string actually changed.
    #[test]
    fn store_applies_snapshots_and_queues_watched_changes() {
        let t = parse_time("2026-11-07T12:46:00+00:00").unwrap();
        let mut s = HaStore::watching(["select.m".to_string()]);
        s.replace_all(vec![EntityState::new("select.m", "Normal"), EntityState::new("sensor.p", "1")]);
        assert!(s.connected);
        assert_eq!(s.snapshots, 1);
        s.apply_change(t, "sensor.p", Some(EntityState::new("sensor.p", "2")));
        s.apply_change(t, "select.m", Some(EntityState::new("select.m", "Normal")));
        s.apply_change(t, "select.m", Some(EntityState::new("select.m", "Off")));
        s.apply_change(t, "select.m", None);
        assert_eq!(Lookup::get(&s, "sensor.p").unwrap().state, "2");
        assert!(Lookup::get(&s, "select.m").is_none());
        assert_eq!(s.all().count(), 1);
        let ch = s.drain_changes();
        assert_eq!(ch.len(), 2);
        assert_eq!((ch[0].old.as_deref(), ch[0].new.as_deref()), (Some("Normal"), Some("Off")));
        assert_eq!((ch[1].old.as_deref(), ch[1].new.clone()), (Some("Off"), None));
        assert!(s.drain_changes().is_empty());
        assert_eq!(s.events, 4);
    }

    /// An undrained change queue is bounded.
    #[test]
    fn change_queue_is_bounded() {
        let t = parse_time("2026-11-07T12:46:00+00:00").unwrap();
        let mut s = HaStore::watching(["n.x".to_string()]);
        for i in 0..(MAX_CHANGES + 5) {
            s.apply_change(t, "n.x", Some(EntityState::new("n.x", &i.to_string())));
        }
        assert_eq!(s.changes.len(), MAX_CHANGES);
        assert_eq!(s.changes.back().unwrap().new.as_deref(), Some("10004"));
    }

    /// A map built from `get_states` works as a lookup.
    #[test]
    fn index_builds_a_lookup() {
        let m = index(vec![EntityState::new("a.b", "1")]);
        assert_eq!(Lookup::get(&m, "a.b").unwrap().state, "1");
        assert_eq!(m.all().count(), 1);
    }
}
