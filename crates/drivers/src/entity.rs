//! The generic entity model: one named value with its metadata, the way the dashboard, the
//! history and an MQTT/HA-discovery bridge see it.
//!
//! A driver's typed [`crate::Reading`] already becomes entities in the app (one per field of its
//! World section); [`crate::Source::entities`] adds what a driver knows beyond that (a daily
//! energy counter, a firmware version, a forecast). An entity is addressed by its group (the
//! device instance id, or `world` / `derived` / `setting` for the logic's own values) and its
//! key within it; both are `[a-z0-9_]`, so `<group>/<key>` is a valid MQTT topic and
//! `<group>_<key>` a valid HA object id. A driver only picks the key; the app sets the group.

use serde::Serialize;

/// The state of an entity. `Null` is HA's `unavailable`/`unknown`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
}

impl From<Option<f64>> for Value {
    fn from(v: Option<f64>) -> Self {
        v.map_or(Value::Null, Value::Number)
    }
}

impl From<Option<bool>> for Value {
    fn from(v: Option<bool>) -> Self {
        v.map_or(Value::Null, Value::Bool)
    }
}

impl From<Option<&str>> for Value {
    fn from(v: Option<&str>) -> Self {
        v.map_or(Value::Null, |s| Value::Text(s.to_string()))
    }
}

impl Value {
    /// The number, if this is one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// From the JSON a client sends: `null`, a bool, a number or a string (anything else is
    /// `None`).
    pub fn from_json(v: &serde_json::Value) -> Option<Value> {
        Some(match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Number(n.as_f64()?),
            serde_json::Value::String(s) => Value::Text(s.clone()),
            _ => return None,
        })
    }
}

/// How an entity can be set: enough to generate an HA `number`, `select`, `switch` or `text`
/// (the unit is on the [`Entity`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Control {
    Number {
        min: Option<f64>,
        max: Option<f64>,
        step: f64,
    },
    Select {
        options: Vec<String>,
    },
    Switch,
    /// Free text of at most `max` characters.
    Text {
        max: usize,
    },
}

/// Whether `n` lies on the grid of `step` from `base` (allowing for float rounding).
fn on_step(n: f64, base: f64, step: f64) -> bool {
    if step <= 0.0 {
        return true;
    }
    let k = (n - base) / step;
    (k - k.round()).abs() <= 1e-6 + k.abs() * 1e-9
}

impl Control {
    /// Check `v` against this control: a finite number in range and on the step grid (counted
    /// from `min`, else 0), one of the options, a bool, a text that is not too long.
    pub fn accepts(&self, v: &Value) -> bool {
        match (self, v) {
            (Control::Number { min, max, step }, Value::Number(n)) => {
                n.is_finite()
                    && min.is_none_or(|m| *n >= m)
                    && max.is_none_or(|m| *n <= m)
                    && on_step(*n, min.unwrap_or(0.0), *step)
            }
            (Control::Select { options }, Value::Text(t)) => options.contains(t),
            (Control::Switch, Value::Bool(_)) => true,
            (Control::Text { max }, Value::Text(t)) => t.chars().count() <= *max,
            _ => false,
        }
    }
}

/// Whether a publisher (MQTT) should send this entity, and how often at most.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Publish {
    pub enabled: bool,
    /// Send at most once per this many seconds (a 1 s P1 value need not go out every second).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_interval_s: Option<f64>,
    /// Skip a numeric change smaller than this (in the entity's unit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadband: Option<f64>,
}

impl Default for Publish {
    fn default() -> Self {
        Publish { enabled: true, min_interval_s: None, deadband: None }
    }
}

impl Publish {
    /// Whether `new` at `now_s` is worth sending after `last` was sent at `last_s` (seconds on
    /// any monotonic clock). The first value always goes out; after that only once the
    /// interval has passed, and a number only when it moved at least the deadband (a change to
    /// or from `Null`, a text or a bool goes out on any change).
    pub fn due(&self, last: Option<(&Value, f64)>, new: &Value, now_s: f64) -> bool {
        if !self.enabled {
            return false;
        }
        let Some((last, last_s)) = last else { return true };
        if self.min_interval_s.is_some_and(|i| now_s - last_s < i) {
            return false;
        }
        match (last, new, self.deadband) {
            (Value::Number(a), Value::Number(b), Some(d)) => (a - b).abs() >= d,
            _ => last != new,
        }
    }
}

/// How HA's long-term statistics should treat a numeric entity (HA's `state_class`). Unset on
/// an [`Entity`]: a publisher picks one by the device class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StateClass {
    Measurement,
    Total,
    /// A meter that only grows (or resets to 0): HA's Energy dashboard takes these.
    TotalIncreasing,
    /// No statistics (a forecast, a setting).
    NoStatistics,
}

impl StateClass {
    /// HA's name; `None` for [`StateClass::NoStatistics`].
    pub fn ha(self) -> Option<&'static str> {
        match self {
            StateClass::Measurement => Some("measurement"),
            StateClass::Total => Some("total"),
            StateClass::TotalIncreasing => Some("total_increasing"),
            StateClass::NoStatistics => None,
        }
    }
}

/// `[a-z0-9_]+`: safe as an MQTT topic level and inside an HA object id.
pub fn is_topic_safe(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// One entity: a stable address, what it is, and its current state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entity {
    /// `<group>_<key>`, unique across the app.
    pub id: String,
    /// The device instance id, or `world` / `derived` / `setting`. Empty until the app sets it.
    pub group: String,
    /// The key within the group (`power_w`, `energy_today_kwh`).
    pub key: String,
    pub name: String,
    /// HA device class (`power`, `energy`, `battery`, `current`, `voltage`, `monetary`,
    /// `timestamp`, ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub value: Value,
    /// Set when the entity can be changed (and something is there to carry it out).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<Control>,
    pub publish: Publish,
    /// The Home Assistant entity that shows the same value today (`sensor.p1_meter_vermogen`),
    /// so an HA bridge can take over its entity id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ha_entity: Option<String>,
    /// HA's `state_class`, when the device class alone does not say it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_class: Option<StateClass>,
}

impl Entity {
    /// A read-only entity with key `key`, not yet in a group.
    pub fn sensor(key: impl Into<String>, name: impl Into<String>, value: Value) -> Self {
        let key = key.into();
        Entity {
            id: key.clone(),
            group: String::new(),
            key,
            name: name.into(),
            class: None,
            unit: None,
            value,
            control: None,
            publish: Publish::default(),
            ha_entity: None,
            state_class: None,
        }
    }

    /// The same entity with a state class.
    pub fn with_state_class(mut self, c: StateClass) -> Self {
        self.state_class = Some(c);
        self
    }

    /// The same entity, naming the HA entity it stands for.
    pub fn with_ha(mut self, entity_id: &str) -> Self {
        self.ha_entity = Some(entity_id.to_string());
        self
    }

    /// The same entity with a device class (empty: none) and a unit.
    pub fn with_unit(mut self, class: &str, unit: &str) -> Self {
        self.class = (!class.is_empty()).then(|| class.to_string());
        self.unit = Some(unit.to_string());
        self
    }

    /// The same entity in `group`: sets the group and the full id.
    pub fn in_group(mut self, group: &str) -> Self {
        self.id = format!("{group}_{}", self.key);
        self.group = group.to_string();
        self
    }

    /// The MQTT topic suffix `<group>/<key>`.
    pub fn topic(&self) -> String {
        format!("{}/{}", self.group, self.key)
    }
}

/// Device class and unit by the field-name suffix the code base uses (`_w`, `_kwh`, ...); an
/// empty class means a unit without one (`%`).
pub fn class_of(key: &str) -> Option<(&'static str, &'static str)> {
    let table = [
        ("_kwh", ("energy", "kWh")),
        ("_kw", ("power", "kW")),
        ("_w", ("power", "W")),
        ("_eur", ("monetary", "EUR")),
        ("_pct", ("", "%")),
        ("_a", ("current", "A")),
        ("_v", ("voltage", "V")),
        ("_km", ("distance", "km")),
        ("_min", ("duration", "min")),
    ];
    table.into_iter().find(|(suffix, _)| key.ends_with(suffix)).map(|(_, cu)| cu)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Values serialise as plain JSON: `null`, a bool, a number, a string.
    #[test]
    fn values_serialise_as_plain_json() {
        let v = [Value::Null, Value::Bool(true), Value::Number(1.5), Value::Text("Solar".into())];
        assert_eq!(serde_json::to_value(v).unwrap(), json!([null, true, 1.5, "Solar"]));
        assert_eq!(Value::from(Some(2.0)), Value::Number(2.0));
        assert_eq!(Value::from(None::<f64>), Value::Null);
        assert_eq!(Value::from(Some(false)), Value::Bool(false));
        assert_eq!(Value::from(None::<bool>), Value::Null);
        assert_eq!(Value::from(Some("a")), Value::Text("a".into()));
        assert_eq!(Value::from(None::<&str>), Value::Null);
        assert_eq!(Value::Number(3.0).as_f64(), Some(3.0));
        assert_eq!(Value::Null.as_f64(), None);
    }

    /// Client JSON becomes a value; arrays and objects are refused.
    #[test]
    fn values_from_client_json() {
        assert_eq!(Value::from_json(&json!(null)), Some(Value::Null));
        assert_eq!(Value::from_json(&json!(true)), Some(Value::Bool(true)));
        assert_eq!(Value::from_json(&json!(7)), Some(Value::Number(7.0)));
        assert_eq!(Value::from_json(&json!("x")), Some(Value::Text("x".into())));
        assert_eq!(Value::from_json(&json!([1])), None);
    }

    /// A control accepts only values of its type, in range, or among its options.
    #[test]
    fn controls_check_type_range_and_options() {
        let n = Control::Number { min: Some(6.0), max: Some(16.0), step: 1.0 };
        assert!(n.accepts(&Value::Number(6.0)) && n.accepts(&Value::Number(16.0)));
        assert!(!n.accepts(&Value::Number(5.9)) && !n.accepts(&Value::Number(16.1)));
        assert!(!n.accepts(&Value::Text("6".into())));
        assert!(Control::Number { min: None, max: None, step: 1.0 }.accepts(&Value::Number(-1e9)));
        let s = Control::Select { options: vec!["Normal".into(), "Solar".into()] };
        assert!(s.accepts(&Value::Text("Solar".into())));
        assert!(!s.accepts(&Value::Text("Turbo".into())));
        assert!(Control::Switch.accepts(&Value::Bool(false)));
        assert!(!Control::Switch.accepts(&Value::Null));
        let t = Control::Text { max: 3 };
        assert!(t.accepts(&Value::Text("abc".into())) && t.accepts(&Value::Text("é€x".into())));
        assert!(!t.accepts(&Value::Text("abcd".into())) && !t.accepts(&Value::Number(1.0)));
        assert_eq!(serde_json::to_value(&t).unwrap(), json!({"type": "text", "max": 3}));
        assert_eq!(serde_json::to_value(&n).unwrap(), json!({"type": "number", "min": 6.0, "max": 16.0, "step": 1.0}));
        assert_eq!(serde_json::to_value(Control::Switch).unwrap(), json!({"type": "switch"}));
    }

    /// A number must sit on its step grid, counted from `min` (else 0), within float rounding;
    /// NaN and infinities never fit; a step of 0 is no grid.
    #[test]
    fn controls_check_the_step() {
        let a = Control::Number { min: Some(0.0), max: Some(32.0), step: 0.1 };
        assert!(a.accepts(&Value::Number(14.5)) && a.accepts(&Value::Number(0.3)));
        assert!(!a.accepts(&Value::Number(14.55)));
        let limit = Control::Number { min: Some(50.0), max: Some(100.0), step: 10.0 };
        assert!(limit.accepts(&Value::Number(70.0)) && !limit.accepts(&Value::Number(75.0)));
        let cents = Control::Number { min: None, max: None, step: 0.01 };
        assert!(cents.accepts(&Value::Number(0.38)) && cents.accepts(&Value::Number(-0.07)));
        assert!(!cents.accepts(&Value::Number(0.385)));
        assert!(cents.accepts(&Value::Number(1619.61)), "large values keep their grid");
        assert!(!cents.accepts(&Value::Number(f64::NAN)) && !cents.accepts(&Value::Number(f64::INFINITY)));
        let free = Control::Number { min: None, max: None, step: 0.0 };
        assert!(free.accepts(&Value::Number(1.234567)));
    }

    /// An entity serialises without the metadata it does not have; the group sets id and topic.
    #[test]
    fn entity_json_skips_missing_metadata() {
        let e = Entity::sensor("energy_today_kwh", "Vandaag", Value::Number(3.2)).with_unit("energy", "kWh");
        assert_eq!(e.id, "energy_today_kwh");
        let e = e.in_group("omnik");
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({"id": "omnik_energy_today_kwh", "group": "omnik", "key": "energy_today_kwh", "name": "Vandaag",
                   "class": "energy", "unit": "kWh", "value": 3.2, "publish": {"enabled": true}})
        );
        assert_eq!(e.topic(), "omnik/energy_today_kwh");
        let e = e.with_ha("sensor.omnik_energy_today").with_state_class(StateClass::TotalIncreasing);
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(
            (&j["ha_entity"], &j["state_class"]),
            (&json!("sensor.omnik_energy_today"), &json!("total_increasing"))
        );
        let names: Vec<_> =
            [StateClass::Measurement, StateClass::Total, StateClass::TotalIncreasing, StateClass::NoStatistics]
                .map(StateClass::ha)
                .into();
        assert_eq!(names, [Some("measurement"), Some("total"), Some("total_increasing"), None]);
        let pct = Entity::sensor("soc", "SoC", Value::Null).with_unit("", "%");
        assert_eq!((pct.class, pct.unit.as_deref()), (None, Some("%")));
    }

    /// Ids are topic-safe: lower-case letters, digits and underscores only.
    #[test]
    fn topic_safe_ids() {
        assert!(is_topic_safe("p1_meter_2"));
        for bad in ["", "P1", "p1.power", "p1/power", "laad kabel", "é"] {
            assert!(!is_topic_safe(bad), "{bad}");
        }
    }

    /// A publisher sends the first value, then only after the interval and outside the
    /// deadband; a disabled entity is never sent; non-numbers go out on any change.
    #[test]
    fn publish_interval_and_deadband() {
        let p = Publish { enabled: true, min_interval_s: Some(5.0), deadband: Some(10.0) };
        let n = Value::Number;
        assert!(p.due(None, &n(100.0), 0.0));
        assert!(!p.due(Some((&n(100.0), 0.0)), &n(200.0), 4.9), "within the interval");
        assert!(!p.due(Some((&n(100.0), 0.0)), &n(109.0), 5.0), "within the deadband");
        assert!(p.due(Some((&n(100.0), 0.0)), &n(110.0), 5.0));
        assert!(p.due(Some((&n(100.0), 0.0)), &Value::Null, 5.0), "going unavailable");
        let plain = Publish::default();
        assert!(!plain.due(Some((&n(1.0), 0.0)), &n(1.0), 1.0), "unchanged");
        assert!(plain.due(Some((&Value::Text("a".into()), 0.0)), &Value::Text("b".into()), 0.0));
        let off = Publish { enabled: false, ..Publish::default() };
        assert!(!off.due(None, &n(1.0), 0.0));
    }

    /// Units follow the field suffixes; `_kwh` wins over `_w`, unknown suffixes have none.
    #[test]
    fn units_from_field_suffixes() {
        assert_eq!(class_of("import_kwh"), Some(("energy", "kWh")));
        assert_eq!(class_of("power_kw"), Some(("power", "kW")));
        assert_eq!(class_of("power_w"), Some(("power", "W")));
        assert_eq!(class_of("kost_eur"), Some(("monetary", "EUR")));
        assert_eq!(class_of("zonaandeel_pct"), Some(("", "%")));
        assert_eq!(class_of("override_a"), Some(("current", "A")));
        assert_eq!(class_of("voltage_v"), Some(("voltage", "V")));
        assert_eq!(class_of("odometer_km"), Some(("distance", "km")));
        assert_eq!(class_of("rc_timeout_min"), Some(("duration", "min")));
        assert_eq!(class_of("soc"), None);
    }
}
