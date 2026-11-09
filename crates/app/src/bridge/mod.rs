//! The MQTT bridge: the entity registry on a broker, for any MQTT client and for Home
//! Assistant. See `docs/MQTT.md` for the topics, the summary and the discovery.
//!
//! | Topic | Retained | |
//! | --- | --- | --- |
//! | `<prefix>/status` | yes | `online`, `offline` (also the last will) |
//! | `<prefix>/<group>/<key>` | yes | one entity's value: a number, `ON`/`OFF`, a text, `unavailable` |
//! | `<prefix>/<group>/<key>/attributes` | yes | `{name, unit, class, ha_entity, updated}` |
//! | `<prefix>/summary` | yes | one compact JSON for small displays, every `summary_interval_s` |
//! | `<prefix>/<group>/<key>/set` | no | set a settable entity (subscribed) |
//! | `<discovery_prefix>/<component>/<node>/<entity id>/config` | yes | HA MQTT discovery ([`discovery`]) |
//!
//! [`Publisher`] is the pure part: which messages a snapshot asks for (throttled per entity
//! by [`Publish::due`](energie_drivers::entity::Publish::due) and the `[[publish]]` rules,
//! filtered by the `include`/`exclude` globs), and what a `/set` message means. [`client`]
//! runs it against a broker: last will, reconnects with backoff, a full republish of the
//! retained state and the discovery on every (re)connect and whenever HA announces itself
//! (`<discovery_prefix>/status` = `online`). A `/set` goes through
//! [`Runtime::set_entity`](crate::runtime::Runtime::set_entity), the path of
//! `POST /api/entities/<id>`, which checks the value against the entity's control.

pub mod client;
pub mod discovery;
pub mod ha_tables;
pub mod summary;

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};

use energie_drivers::entity::{Control, Entity, Value};
use serde_json::json;

use crate::entities::glob;
use crate::runtime::Snapshot;

/// What the HA discovery says about a device instance.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    /// The driver kind.
    pub model: String,
}

/// The bridge's settings (from `[mqtt_publish]` and the device instances).
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeConfig {
    pub prefix: String,
    /// `Some(prefix)` publishes HA discovery under it.
    pub discovery_prefix: Option<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub summary_interval_s: f64,
    /// The node id in discovery topics and unique ids (`energie`).
    pub node: String,
    /// The device instances by id (entity group).
    pub devices: BTreeMap<String, DeviceInfo>,
    /// Accept `/set` and offer controls in discovery.
    pub allow_set: bool,
}

impl BridgeConfig {
    /// From the config file: the prefix with `/` as `_` is the node id.
    pub fn from_config(cfg: &crate::config::Config, devices: &[crate::plan::Instance]) -> Self {
        let m = &cfg.mqtt_publish;
        BridgeConfig {
            prefix: m.prefix.clone(),
            discovery_prefix: cfg.discovery().then(|| m.discovery_prefix.clone()),
            include: m.include.clone(),
            exclude: m.exclude.clone(),
            summary_interval_s: m.summary_interval_s as f64,
            node: m.prefix.replace('/', "_"),
            allow_set: m.allow_set,
            devices: devices
                .iter()
                .map(|d| {
                    let name = d.name.clone().unwrap_or_else(|| d.id.clone());
                    (d.id.clone(), DeviceInfo { name, model: d.kind.clone() })
                })
                .collect(),
        }
    }

    pub fn status_topic(&self) -> String {
        format!("{}/status", self.prefix)
    }

    /// `<prefix>/+/+/set`.
    pub fn set_filter(&self) -> String {
        format!("{}/+/+/set", self.prefix)
    }

    /// HA's birth/will topic, when discovery is on.
    pub fn ha_status_topic(&self) -> Option<String> {
        self.discovery_prefix.as_ref().map(|d| format!("{d}/status"))
    }

    pub fn state_topic(&self, e: &Entity) -> String {
        format!("{}/{}", self.prefix, e.topic())
    }

    pub fn summary_topic(&self) -> String {
        format!("{}/summary", self.prefix)
    }

    /// Published: the `[[publish]]` rules allow it, an `include` glob matches and no `exclude`.
    pub fn wanted(&self, e: &Entity) -> bool {
        e.publish.enabled && self.include.iter().any(|g| glob(g, &e.id)) && !self.exclude.iter().any(|g| glob(g, &e.id))
    }
}

/// One message to publish.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub topic: String,
    pub payload: String,
    pub retain: bool,
}

impl Msg {
    fn retained(topic: String, payload: String) -> Msg {
        Msg { topic, payload, retain: true }
    }
}

/// An entity value as a topic payload: a number as JSON writes it, `ON`/`OFF`, the text as
/// is, `unavailable` for HA's unavailable.
pub fn payload(v: &Value) -> String {
    match v {
        Value::Null => "unavailable".into(),
        Value::Bool(b) => if *b { "ON" } else { "OFF" }.into(),
        Value::Number(n) => json!(n).to_string(),
        Value::Text(t) => t.clone(),
    }
}

/// What one `/set` message asks: the entity id and the value, parsed by the entity's control
/// (a number, `ON`/`OFF`/`true`/`false`/`1`/`0`, an option, a text). The runtime then checks
/// range, step and options.
pub fn parse_set(cfg: &BridgeConfig, entities: &[Entity], topic: &str, raw: &str) -> Result<(String, Value), String> {
    let rest = topic
        .strip_prefix(&cfg.prefix)
        .and_then(|r| r.strip_prefix('/'))
        .and_then(|r| r.strip_suffix("/set"))
        .ok_or_else(|| format!("{topic}: not a set topic"))?;
    let (group, key) = rest.split_once('/').ok_or_else(|| format!("{topic}: not a set topic"))?;
    let e = entities
        .iter()
        .find(|e| e.group == group && e.key == key && cfg.wanted(e))
        .ok_or_else(|| format!("{topic}: no such entity"))?;
    let control = e.control.as_ref().ok_or_else(|| format!("{}: not settable now", e.id))?;
    let t = raw.trim();
    let v = match control {
        Control::Number { .. } => {
            Value::Number(t.parse::<f64>().map_err(|_| format!("{}: {raw:?} is not a number", e.id))?)
        }
        Control::Switch => match t.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => Value::Bool(true),
            "off" | "false" | "0" => Value::Bool(false),
            _ => return Err(format!("{}: {raw:?} is not ON or OFF", e.id)),
        },
        Control::Select { .. } => Value::Text(t.to_string()),
        Control::Text { .. } => Value::Text(raw.to_string()),
    };
    Ok((e.id.clone(), v))
}

/// The messages a stream of snapshots asks for, with what was sent last.
pub struct Publisher {
    pub cfg: BridgeConfig,
    sent: HashMap<String, (Value, f64)>,
    summary_at: Option<f64>,
    /// The discovery config last sent per entity id: `(topic, payload)`.
    discovered: BTreeMap<String, (String, String)>,
    /// A fingerprint of the registry's shape when the discovery was last computed.
    shape: Option<u64>,
}

/// What the discovery of an entity depends on (not its value, except its type).
fn shape(es: &[Entity]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for e in es {
        e.id.hash(&mut h);
        e.name.hash(&mut h);
        (&e.class, &e.unit, &e.ha_entity).hash(&mut h);
        format!("{:?}{:?}", e.control, e.state_class).hash(&mut h);
        std::mem::discriminant(&e.value).hash(&mut h);
        e.publish.enabled.hash(&mut h);
    }
    h.finish()
}

impl Publisher {
    pub fn new(cfg: BridgeConfig) -> Self {
        Publisher { cfg, sent: HashMap::new(), summary_at: None, discovered: BTreeMap::new(), shape: None }
    }

    /// Forget what was sent: the next [`Publisher::tick`] sends everything again (a
    /// reconnect, HA coming online).
    pub fn reset(&mut self) {
        self.sent.clear();
        self.summary_at = None;
        self.discovered.clear();
        self.shape = None;
    }

    /// `<prefix>/status` = `online`, retained.
    pub fn online(&self) -> Msg {
        Msg::retained(self.cfg.status_topic(), "online".into())
    }

    /// `<prefix>/status` = `offline`, retained (a clean stop; the last will says the same).
    pub fn offline(&self) -> Msg {
        Msg::retained(self.cfg.status_topic(), "offline".into())
    }

    /// The discovery configs that changed since the last call: new and changed entities, an
    /// empty retained config for an entity that is gone or changed component.
    fn discovery(&mut self, es: &[Entity]) -> Vec<Msg> {
        let Some(dp) = self.cfg.discovery_prefix.clone() else { return Vec::new() };
        let fp = shape(es);
        if self.shape == Some(fp) {
            return Vec::new();
        }
        self.shape = Some(fp);
        let mut out = Vec::new();
        let mut now = BTreeMap::new();
        for e in es.iter().filter(|e| self.cfg.wanted(e)) {
            let read_only;
            let shown = if self.cfg.allow_set {
                e
            } else {
                read_only = Entity { control: None, ..e.clone() };
                &read_only
            };
            let (topic, cfg) = discovery::config(&self.cfg, &dp, shown);
            now.insert(e.id.clone(), (topic, cfg.to_string()));
        }
        for (id, (topic, _)) in &self.discovered {
            if now.get(id).is_none_or(|(t, _)| t != topic) {
                out.push(Msg::retained(topic.clone(), String::new()));
            }
        }
        for (id, (topic, body)) in &now {
            if self.discovered.get(id).is_none_or(|(_, b)| b != body) {
                out.push(Msg::retained(topic.clone(), body.clone()));
            }
        }
        self.discovered = now;
        out
    }

    /// Everything `snap` asks for at `now_s` (any monotonic clock, seconds): discovery that
    /// changed, the values that are due (with their attributes), and the summary when due.
    pub fn tick(&mut self, snap: &Snapshot, now_s: f64) -> Vec<Msg> {
        let mut out = self.discovery(&snap.entities);
        let updated = snap.now.clone().map_or(serde_json::Value::Null, serde_json::Value::String);
        for e in snap.entities.iter().filter(|e| self.cfg.wanted(e)) {
            let last = self.sent.get(&e.id).map(|(v, t)| (v, *t));
            if !e.publish.due(last, &e.value, now_s) {
                continue;
            }
            let topic = self.cfg.state_topic(e);
            let attrs =
                json!({"name": e.name, "unit": e.unit, "class": e.class, "ha_entity": e.ha_entity, "updated": updated});
            out.push(Msg::retained(format!("{topic}/attributes"), attrs.to_string()));
            out.push(Msg::retained(topic, payload(&e.value)));
            self.sent.insert(e.id.clone(), (e.value.clone(), now_s));
        }
        if self.summary_at.is_none_or(|t| now_s - t >= self.cfg.summary_interval_s) {
            self.summary_at = Some(now_s);
            out.push(Msg::retained(self.cfg.summary_topic(), summary::summary(snap).to_string()));
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use energie_drivers::entity::Publish;
    use std::sync::Arc;

    pub(crate) fn cfg() -> BridgeConfig {
        BridgeConfig {
            allow_set: true,
            prefix: "energie".into(),
            discovery_prefix: None,
            include: vec!["*".into()],
            exclude: vec![],
            summary_interval_s: 5.0,
            node: "energie".into(),
            devices: BTreeMap::new(),
        }
    }

    pub(crate) fn snap(es: Vec<Entity>) -> Snapshot {
        Snapshot { now: Some("2026-11-08T12:00:00+01:00".into()), entities: Arc::new(es), ..Default::default() }
    }

    fn num(group: &str, key: &str, v: f64) -> Entity {
        Entity::sensor(key, key, Value::Number(v)).with_unit("power", "W").in_group(group)
    }

    fn topics(m: &[Msg]) -> Vec<&str> {
        m.iter().map(|m| m.topic.as_str()).collect()
    }

    /// Values are plain payloads: numbers as JSON writes them, ON/OFF, text, `unavailable`.
    #[test]
    fn payloads() {
        assert_eq!(payload(&Value::Number(3066.0)), "3066.0");
        assert_eq!(payload(&Value::Number(-0.25)), "-0.25");
        assert_eq!(payload(&Value::Bool(true)), "ON");
        assert_eq!(payload(&Value::Bool(false)), "OFF");
        assert_eq!(payload(&Value::Text("Solar".into())), "Solar");
        assert_eq!(payload(&Value::Null), "unavailable");
    }

    /// The first tick sends every value with its attributes and the summary; later ticks only
    /// what is due (changed, outside the deadband, after the interval); a reset sends it all.
    #[test]
    fn values_are_throttled_per_entity() {
        let mut p = Publisher::new(cfg());
        let mut fast = num("world", "grid_meter_power_w", 3066.0);
        fast.publish = Publish { enabled: true, min_interval_s: Some(5.0), deadband: Some(10.0) };
        let slow = num("derived", "pv_vermogen_totaal_w", 533.0);
        let m = p.tick(&snap(vec![fast.clone(), slow.clone()]), 0.0);
        assert_eq!(
            topics(&m),
            [
                "energie/world/grid_meter_power_w/attributes",
                "energie/world/grid_meter_power_w",
                "energie/derived/pv_vermogen_totaal_w/attributes",
                "energie/derived/pv_vermogen_totaal_w",
                "energie/summary"
            ]
        );
        assert!(m.iter().all(|m| m.retain));
        assert_eq!(m[1].payload, "3066.0");
        let attrs: serde_json::Value = serde_json::from_str(&m[0].payload).unwrap();
        assert_eq!(
            attrs,
            json!({"name": "grid_meter_power_w", "unit": "W", "class": "power", "ha_entity": null, "updated": "2026-11-08T12:00:00+01:00"})
        );
        // One second later: the P1 value moved 100 W but is inside its interval; nothing else changed.
        fast.value = Value::Number(3166.0);
        assert!(p.tick(&snap(vec![fast.clone(), slow.clone()]), 1.0).is_empty());
        let m = p.tick(&snap(vec![fast.clone(), slow.clone()]), 5.0);
        assert_eq!(
            topics(&m),
            ["energie/world/grid_meter_power_w/attributes", "energie/world/grid_meter_power_w", "energie/summary"]
        );
        fast.value = Value::Number(3170.0);
        assert!(
            p.tick(&snap(vec![fast.clone(), slow.clone()]), 11.0).iter().all(|m| m.topic == "energie/summary"),
            "deadband"
        );
        p.reset();
        assert_eq!(p.tick(&snap(vec![fast, slow]), 12.0).len(), 5);
    }

    /// Globs pick the entities; a `[[publish]]` rule that disables one wins.
    #[test]
    fn include_exclude_and_disabled() {
        let c = BridgeConfig {
            include: vec!["world_*".into(), "derived_*".into()],
            exclude: vec!["*_kwh".into()],
            ..cfg()
        };
        let mut off = num("world", "grid_meter_import_kwh", 1.0);
        assert!(!c.wanted(&off));
        off = num("world", "grid_meter_power_w", 1.0);
        assert!(c.wanted(&off));
        off.publish.enabled = false;
        assert!(!c.wanted(&off));
        assert!(!c.wanted(&num("setting", "x", 1.0)));
        let mut p = Publisher::new(c);
        assert_eq!(topics(&p.tick(&snap(vec![off, num("setting", "x", 1.0)]), 0.0)), ["energie/summary"]);
    }

    /// The topics the bridge uses.
    #[test]
    fn topic_names() {
        let c = BridgeConfig { prefix: "huis/energie".into(), discovery_prefix: Some("homeassistant".into()), ..cfg() };
        assert_eq!(c.status_topic(), "huis/energie/status");
        assert_eq!(c.set_filter(), "huis/energie/+/+/set");
        assert_eq!(c.ha_status_topic().as_deref(), Some("homeassistant/status"));
        assert_eq!(cfg().ha_status_topic(), None);
        let p = Publisher::new(c);
        assert_eq!((p.online().payload.as_str(), p.offline().payload.as_str()), ("online", "offline"));
        assert!(p.online().retain && p.offline().topic == "huis/energie/status");
    }

    /// A `/set` is parsed by the entity's control; unknown, read-only and excluded entities
    /// and unparseable payloads are refused with a reason.
    #[test]
    fn set_messages() {
        let mut n = num("setting", "batterij_reservering_w", 700.0);
        n.control = Some(Control::Number { min: None, max: None, step: 50.0 });
        let mut s = Entity::sensor("peak_guard_actief", "Peak guard", Value::Bool(true)).in_group("setting");
        s.control = Some(Control::Switch);
        let mut sel = Entity::sensor("ev_net_niveau", "Net", Value::Text("Uit".into())).in_group("setting");
        sel.control = Some(Control::Select { options: vec!["Uit".into()] });
        let mut txt = Entity::sensor("note", "Notitie", Value::Text(String::new())).in_group("dev");
        txt.control = Some(Control::Text { max: 10 });
        let ro = num("world", "grid_meter_power_w", 1.0);
        let es = vec![n, s, sel, txt, ro];
        let c = cfg();
        let set = |t: &str, p: &str| parse_set(&c, &es, t, p);
        assert_eq!(
            set("energie/setting/batterij_reservering_w/set", " 750 "),
            Ok(("setting_batterij_reservering_w".into(), Value::Number(750.0)))
        );
        assert_eq!(
            set("energie/setting/peak_guard_actief/set", "OFF"),
            Ok(("setting_peak_guard_actief".into(), Value::Bool(false)))
        );
        for on in ["on", "true", "1"] {
            assert_eq!(set("energie/setting/peak_guard_actief/set", on).unwrap().1, Value::Bool(true));
        }
        for off in ["false", "0"] {
            assert_eq!(set("energie/setting/peak_guard_actief/set", off).unwrap().1, Value::Bool(false));
        }
        assert_eq!(set("energie/setting/ev_net_niveau/set", "Uit\n").unwrap().1, Value::Text("Uit".into()));
        assert_eq!(set("energie/dev/note/set", " a ").unwrap().1, Value::Text(" a ".into()));
        let err = |t: &str, p: &str| set(t, p).unwrap_err();
        assert!(err("energie/setting/batterij_reservering_w/set", "veel").contains("not a number"));
        assert!(err("energie/setting/peak_guard_actief/set", "misschien").contains("ON or OFF"));
        assert!(err("energie/world/grid_meter_power_w/set", "1").contains("not settable"));
        assert!(err("energie/world/bestaat_niet/set", "1").contains("no such entity"));
        assert!(err("anders/world/x/set", "1").contains("not a set topic"));
        assert!(err("energie/x/set", "1").contains("not a set topic"));
        let excl = BridgeConfig { exclude: vec!["setting_*".into()], ..cfg() };
        assert!(
            parse_set(&excl, &es, "energie/setting/peak_guard_actief/set", "ON")
                .unwrap_err()
                .contains("no such entity")
        );
    }

    /// The config file's section and the device instances become the bridge settings.
    #[test]
    fn from_the_config_file() {
        let c = crate::config::Config::parse(
            "mode = \"control\"\n[mqtt_publish]\nenabled = true\nbroker = \"127.0.0.1\"\nprefix = \"huis/energie\"\nexclude = [\"derived_kwartier_*\"]\n",
        )
        .unwrap();
        let devices = vec![
            crate::plan::Instance {
                id: "p1".into(),
                kind: "homewizard".into(),
                label: "device p1".into(),
                name: Some("P1-meter".into()),
                reads: vec![],
                controls: vec![],
                poll_ms: 1000,
                grace_polls: 3,
                read_only: true,
                table: json!({}),
            },
            crate::plan::Instance { id: "ev3".into(), kind: "kia_connect".into(), name: None, ..Default::default() },
        ];
        let b = BridgeConfig::from_config(&c, &devices);
        assert_eq!((b.node.as_str(), b.discovery_prefix.as_deref()), ("huis_energie", Some("homeassistant")));
        assert_eq!(b.devices["p1"], DeviceInfo { name: "P1-meter".into(), model: "homewizard".into() });
        assert_eq!(b.devices["ev3"].name, "ev3");
        assert_eq!((b.exclude.len(), b.summary_interval_s), (1, 5.0));
    }
}
