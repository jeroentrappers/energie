//! Home Assistant MQTT discovery: one config per entity under
//! `<discovery_prefix>/<component>/<node>/<entity id>/config`.
//!
//! - **Component**: from the entity's control (`number`, `select`, `switch`, `text`), else
//!   `binary_sensor` for an on/off value and `sensor` for the rest.
//! - **Entity id**: an entity that takes over an HA entity ([`ha_tables::TAKEOVER`], or a
//!   driver's own entity that names its HA entity) gets `default_entity_id` (and `object_id`
//!   for HA before 2025.10) `<component>.<that entity's object id>`: once the old entity is
//!   gone, the MQTT entity has its id. Every other entity gets `<component>.<node>_<entity id>`.
//! - **Unit, device class, state class**: HA's own for the entity it stands for
//!   ([`ha_tables::HA_META`]), so the long-term statistics continue; otherwise from the entity
//!   (`state_class` when it names one, else by device class: energy `total_increasing`,
//!   monetary `total`, power/current/voltage/battery and `%` `measurement`).
//! - **Device**: `world` is the device `energie`; `derived`, `setting`, `memory` and each
//!   device instance are devices of their own, `via_device` `energie`.
//! - **Availability**: `<prefix>/status`; a value `unavailable` renders as HA's unknown.

use energie_drivers::entity::{Control, Entity, Value};
use serde_json::{Value as Json, json};

use super::BridgeConfig;
use super::ha_tables::{HA_META, TAKEOVER};

/// The groups the app fills itself (their HA ids come from [`TAKEOVER`] only).
const APP_GROUPS: [&str; 4] = ["world", "derived", "setting", "memory"];

/// HA's `value_template`: `unavailable` is HA's unknown (`None`).
const VALUE_TEMPLATE: &str = "{{ none if value == 'unavailable' else value }}";

/// The HA entity `e` replaces and the entity id it takes over, if any.
pub fn takeover(e: &Entity) -> Option<(String, String)> {
    if let Some((_, old, new)) = TAKEOVER.iter().find(|(id, ..)| *id == e.id) {
        return Some((old.to_string(), new.to_string()));
    }
    let own = e.ha_entity.as_ref().filter(|_| !APP_GROUPS.contains(&e.group.as_str()))?;
    Some((own.clone(), own.clone()))
}

/// HA's `(state_class, device_class, unit)` of entity `ha`, if the live snapshot has it.
pub fn meta(ha: &str) -> Option<(Option<&'static str>, Option<&'static str>, Option<&'static str>)> {
    HA_META.iter().find(|(id, ..)| *id == ha).map(|(_, s, d, u)| (*s, *d, *u))
}

fn domain(id: &str) -> &str {
    id.split_once('.').map_or(id, |(d, _)| d)
}

/// The HA platform of `e`.
pub fn component(e: &Entity) -> &'static str {
    match &e.control {
        Some(Control::Number { .. }) => "number",
        Some(Control::Select { .. }) => "select",
        Some(Control::Switch) => "switch",
        Some(Control::Text { .. }) => "text",
        None => {
            let ha = takeover(e).map(|(_, new)| new).or_else(|| e.ha_entity.clone());
            let on_off =
                ha.as_deref().is_some_and(|h| ["binary_sensor", "input_boolean", "switch"].contains(&domain(h)));
            if on_off || matches!(e.value, Value::Bool(_)) { "binary_sensor" } else { "sensor" }
        }
    }
}

/// The state class when HA's own entity does not say it.
fn state_class_of(e: &Entity) -> Option<&'static str> {
    if let Some(c) = e.state_class {
        return c.ha();
    }
    if matches!(e.value, Value::Text(_)) || e.unit.is_none() {
        return None;
    }
    match e.class.as_deref() {
        Some("energy") => Some("total_increasing"),
        Some("monetary") => Some("total"),
        Some("power" | "current" | "voltage" | "battery") => Some("measurement"),
        _ if e.unit.as_deref() == Some("%") => Some("measurement"),
        _ => None,
    }
}

/// The topic and the config of entity `e`.
pub fn config(cfg: &BridgeConfig, discovery_prefix: &str, e: &Entity) -> (String, Json) {
    let comp = component(e);
    let node = &cfg.node;
    let object = match takeover(e) {
        Some((_, new)) => new.split_once('.').map_or(new.clone(), |(_, o)| o.to_string()),
        None => format!("{node}_{}", e.id),
    };
    let state = cfg.state_topic(e);
    let device = match e.group.as_str() {
        "world" => json!({"identifiers": [node], "name": "energie", "manufacturer": "energie", "model": "energie",
                          "sw_version": env!("CARGO_PKG_VERSION")}),
        g => {
            let (name, model) = match g {
                "derived" => ("energie berekend".to_string(), "energie".to_string()),
                "setting" => ("energie instellingen".to_string(), "energie".to_string()),
                "memory" => ("energie geheugen".to_string(), "energie".to_string()),
                _ => cfg
                    .devices
                    .get(g)
                    .map_or((g.to_string(), "energie".to_string()), |d| (d.name.clone(), d.model.clone())),
            };
            json!({"identifiers": [format!("{node}_{g}")], "name": name, "manufacturer": "energie", "model": model,
                   "via_device": node})
        }
    };
    let mut c = json!({
        "name": e.name,
        "unique_id": format!("{node}_{}", e.id),
        "object_id": object,
        "default_entity_id": format!("{comp}.{object}"),
        "state_topic": state,
        "json_attributes_topic": format!("{state}/attributes"),
        "availability_topic": cfg.status_topic(),
        "value_template": VALUE_TEMPLATE,
        "device": device,
        "origin": {"name": "energie", "sw": env!("CARGO_PKG_VERSION")},
    });
    let o = c.as_object_mut().expect("an object");
    if e.group == "memory" {
        o.insert("entity_category".into(), json!("diagnostic"));
    }
    let ha = takeover(e).map(|(old, _)| old).or_else(|| e.ha_entity.clone());
    let (state_class, class, unit) = match ha.as_deref().and_then(meta) {
        Some(m) => m,
        None => (state_class_of(e), e.class.as_deref(), e.unit.as_deref()),
    };
    let command = format!("{state}/set");
    match &e.control {
        Some(Control::Number { min, max, step }) => {
            o.insert("command_topic".into(), json!(command));
            o.insert("min".into(), json!(min.unwrap_or(-1e9)));
            o.insert("max".into(), json!(max.unwrap_or(1e9)));
            o.insert("step".into(), json!(if *step > 0.0 { *step } else { 0.001 }));
            o.insert("mode".into(), json!("box"));
        }
        Some(Control::Select { options }) => {
            o.insert("command_topic".into(), json!(command));
            o.insert("options".into(), json!(options));
        }
        Some(Control::Switch) => {
            o.insert("command_topic".into(), json!(command));
            for k in ["payload_on", "state_on"] {
                o.insert(k.into(), json!("ON"));
            }
            for k in ["payload_off", "state_off"] {
                o.insert(k.into(), json!("OFF"));
            }
        }
        Some(Control::Text { max }) => {
            o.insert("command_topic".into(), json!(command));
            o.insert("max".into(), json!((*max).min(255)));
        }
        None if comp == "binary_sensor" => {
            o.insert("payload_on".into(), json!("ON"));
            o.insert("payload_off".into(), json!("OFF"));
        }
        None => {
            if let Some(s) = state_class {
                o.insert("state_class".into(), json!(s));
            }
        }
    }
    if let Some(u) = unit.filter(|_| matches!(comp, "sensor" | "number")) {
        o.insert("unit_of_measurement".into(), json!(u));
    }
    if let Some(d) = class.filter(|d| !d.is_empty() && comp != "select" && comp != "text") {
        o.insert("device_class".into(), json!(d));
    }
    (format!("{discovery_prefix}/{comp}/{node}/{}/config", e.id), c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::tests::cfg;
    use crate::bridge::{DeviceInfo, Publisher};
    use crate::entities::{Inputs, collect};
    use energie_core::derived::Derived;
    use energie_core::memory::Memory;
    use energie_core::settings::Settings;
    use energie_core::world::World;
    use energie_drivers::entity::StateClass;

    fn registry(settable: bool) -> Vec<Entity> {
        collect(&Inputs {
            world: &World::default(),
            derived: &Derived::default(),
            settings: &Settings::default(),
            settings_editable: settable,
            memory: &Memory::default(),
            settable: &|_| settable,
            devices: &[],
            rules: &[],
        })
    }

    fn find(es: &[Entity], id: &str) -> Entity {
        es.iter().find(|e| e.id == id).unwrap().clone()
    }

    fn conf(e: &Entity) -> (String, Json) {
        config(&cfg(), "homeassistant", e)
    }

    /// Derived values take over their template sensor or meter, with HA's own unit and state
    /// class (statistics continue); the topic names component, node and id.
    #[test]
    fn derived_values_take_over_their_template_sensors() {
        let es = registry(true);
        let (topic, c) = conf(&find(&es, "derived_pv_energie_dag_kwh"));
        assert_eq!(topic, "homeassistant/sensor/energie/derived_pv_energie_dag_kwh/config");
        assert_eq!(
            (&c["default_entity_id"], &c["object_id"]),
            (&json!("sensor.pv_energie_dag"), &json!("pv_energie_dag"))
        );
        assert_eq!(c["state_class"], "total_increasing");
        assert_eq!(c.get("unit_of_measurement"), None, "HA's utility meter had no unit");
        assert_eq!(c["state_topic"], "energie/derived/pv_energie_dag_kwh");
        assert_eq!(c["json_attributes_topic"], "energie/derived/pv_energie_dag_kwh/attributes");
        assert_eq!(c["availability_topic"], "energie/status");
        assert_eq!(c["unique_id"], "energie_derived_pv_energie_dag_kwh");
        assert_eq!(c["device"]["name"], "energie berekend");
        assert_eq!(c["device"]["via_device"], "energie");
        let (topic, c) = conf(&find(&es, "derived_capaciteitspiek_risico"));
        assert!(topic.starts_with("homeassistant/binary_sensor/"));
        assert_eq!(c["default_entity_id"], "binary_sensor.capaciteitspiek_risico");
        assert_eq!((&c["device_class"], &c["payload_on"]), (&json!("problem"), &json!("ON")));
        let (_, c) = conf(&find(&es, "derived_batterij_geladen_energie_kwh"));
        assert_eq!(
            (&c["state_class"], &c["device_class"], &c["unit_of_measurement"]),
            (&json!("total"), &json!("energy"), &json!("kWh"))
        );
    }

    /// Settings become number/select/switch with the helper's object id; the memory read-only
    /// sensors and binary sensors, diagnostic.
    #[test]
    fn helpers_become_numbers_selects_switches_and_sensors() {
        let es = registry(true);
        let (_, c) = conf(&find(&es, "setting_batterij_reserve_soc"));
        assert_eq!(c["default_entity_id"], "number.batterij_reserve_soc");
        assert_eq!(
            (&c["command_topic"], &c["mode"]),
            (&json!("energie/setting/batterij_reserve_soc/set"), &json!("box"))
        );
        assert_eq!((&c["min"], &c["max"], &c["step"]), (&json!(5.0), &json!(90.0), &json!(5.0)));
        assert_eq!(c["unit_of_measurement"], "%");
        let (_, c) = conf(&find(&es, "setting_zonprioriteit"));
        assert_eq!(c["default_entity_id"], "select.zonprioriteit");
        assert_eq!(c["options"].as_array().unwrap().len(), 6);
        let (_, c) = conf(&find(&es, "setting_peak_guard_actief"));
        assert_eq!(c["default_entity_id"], "switch.peak_guard_actief");
        assert_eq!((&c["state_on"], &c["payload_off"]), (&json!("ON"), &json!("OFF")));
        let (_, c) = conf(&find(&es, "memory_batterij_net_laden"));
        assert_eq!(c["default_entity_id"], "binary_sensor.batterij_net_laden");
        assert_eq!(c["entity_category"], "diagnostic");
        let (_, c) = conf(&find(&es, "memory_capaciteit_piekhistoriek"));
        assert_eq!(c["default_entity_id"], "sensor.capaciteit_piekhistoriek");
        assert_eq!(c.get("command_topic"), None);
        let (_, c) = conf(&find(&es, "memory_nachtdekking"));
        assert_eq!((&c["unit_of_measurement"], c.get("state_class")), (&json!("%"), None));
        assert_eq!(c["device"]["name"], "energie geheugen");
        let (_, c) = conf(&find(&registry(false), "setting_peak_guard_actief"));
        assert_eq!(c["default_entity_id"], "binary_sensor.peak_guard_actief", "read only: a binary sensor");
    }

    /// World fields take over only what the cutover replaces (the Solis, the cable, the cars,
    /// the aircos, Fluvius); HA keeps the P1 meter, so its entity gets an energie id.
    #[test]
    fn world_fields_take_over_what_the_cutover_replaces() {
        let es = registry(true);
        let (topic, c) = conf(&find(&es, "world_battery_inverter_rc_mode"));
        assert!(topic.starts_with("homeassistant/select/"));
        assert_eq!(c["default_entity_id"], "select.solis_s6_eh1p_rc_force_charge_discharge_2");
        let (_, c) = conf(&find(&es, "world_battery_inverter_rc_charge_w"));
        assert_eq!(
            (&c["device_class"], &c["unit_of_measurement"], &c["min"]),
            (&json!("power"), &json!("W"), &json!(0.0))
        );
        assert_eq!(c["device"]["name"], "energie");
        let (_, c) = conf(&find(&es, "world_grid_meter_power_w"));
        assert_eq!(c["default_entity_id"], "sensor.energie_world_grid_meter_power_w");
        assert_eq!((&c["state_class"], &c["device_class"]), (&json!("measurement"), &json!("power")), "HA's P1 entity");
        let (_, c) = conf(&find(&es, "world_car_ev3_plugged"));
        assert_eq!(c["default_entity_id"], "binary_sensor.ev3_ev_battery_plug");
        let (_, c) = conf(&find(&registry(false), "world_load_airco_zolder_on"));
        assert_eq!(c["default_entity_id"], "binary_sensor.airco_zolder_power", "without a controller: read only");
        let (_, c) = conf(&find(&es, "world_charger_buzz_state"));
        assert_eq!(c["default_entity_id"], "sensor.energie_world_charger_buzz_state");
    }

    /// A driver's entity that names its HA entity takes it over (the forecast, the Solis
    /// energy totals); with a class but no HA entity the state class follows the class.
    #[test]
    fn device_entities_and_state_classes() {
        let mut c = cfg();
        c.devices.insert("solis".into(), DeviceInfo { name: "Solis".into(), model: "solis_modbus".into() });
        let total = Entity::sensor("pv_total_kwh", "PV totaal", Value::Number(12345.6))
            .with_unit("energy", "kWh")
            .with_ha("sensor.solis_s6_eh1p_pv_total_energy_generation_2")
            .in_group("solis");
        let (topic, d) = config(&c, "ha", &total);
        assert_eq!(topic, "ha/sensor/energie/solis_pv_total_kwh/config");
        assert_eq!(d["default_entity_id"], "sensor.solis_s6_eh1p_pv_total_energy_generation_2");
        assert_eq!((&d["state_class"], &d["device_class"]), (&json!("total_increasing"), &json!("energy")));
        assert_eq!((&d["device"]["name"], &d["device"]["model"]), (&json!("Solis"), &json!("solis_modbus")));
        let fc = Entity::sensor("energy_today_kwh", "Forecast vandaag", Value::Null)
            .with_unit("energy", "kWh")
            .with_state_class(StateClass::NoStatistics)
            .with_ha("sensor.energy_production_today")
            .in_group("forecast");
        let (_, d) = config(&c, "ha", &fc);
        assert_eq!(d["default_entity_id"], "sensor.energy_production_today");
        assert_eq!((d.get("state_class"), &d["device"]["name"]), (None, &json!("forecast")));
        let n = |class: &str, unit: &str| {
            Entity::sensor("x", "x", Value::Number(1.0)).with_unit(class, unit).in_group("dev")
        };
        let sc = |e: &Entity| config(&c, "ha", e).1.get("state_class").cloned();
        assert_eq!(sc(&n("monetary", "EUR")), Some(json!("total")));
        assert_eq!(sc(&n("current", "A")), Some(json!("measurement")));
        assert_eq!(sc(&n("", "%")), Some(json!("measurement")));
        assert_eq!(sc(&n("distance", "km")), None);
        assert_eq!(sc(&Entity::sensor("x", "x", Value::Number(1.0)).in_group("dev")), None, "no unit");
        assert_eq!(sc(&Entity::sensor("x", "x", Value::Text("a".into())).with_unit("", "%").in_group("dev")), None);
        let (_, d) = config(&c, "ha", &n("distance", "km"));
        assert_eq!(d["default_entity_id"], "sensor.energie_dev_x");
        let mut t = Entity::sensor("note", "Notitie", Value::Text(String::new())).in_group("dev");
        t.control = Some(Control::Text { max: 1000 });
        let (topic, d) = config(&c, "ha", &t);
        assert!(topic.starts_with("ha/text/"));
        assert_eq!((&d["max"], &d["command_topic"]), (&json!(255), &json!("energie/dev/note/set")));
        let mut free = Entity::sensor("bias", "Bias", Value::Number(0.0)).in_group("dev");
        free.control = Some(Control::Number { min: None, max: None, step: 0.0 });
        assert_eq!(config(&c, "ha", &free).1["step"], 0.001);
        assert_eq!(meta("sensor.bestaat_niet"), None);
        assert_eq!(domain("geen_domein"), "geen_domein");
    }

    /// Every row of the takeover table names an entity of the registry (the device rows: of
    /// this house's `ev3` and `forecast` devices), and the HA entity it replaces; each new
    /// id is taken once.
    #[test]
    fn the_takeover_table_matches_the_registry() {
        let es = registry(true);
        let mut new: Vec<_> = TAKEOVER.iter().map(|(_, _, n)| *n).collect();
        new.sort();
        new.dedup();
        assert_eq!(new.len(), TAKEOVER.len());
        for (id, old, n) in TAKEOVER {
            let ok = es.iter().any(|e| e.id == *id) || id.starts_with("ev3_") || id.starts_with("forecast_");
            assert!(ok, "{id}");
            assert_eq!(
                domain(old) == domain(n),
                !old.starts_with("input_")
                    && !["counter", "timer"].contains(&domain(old))
                    && *old != "number.ev3_dc_charging_limit",
                "{old} → {n}"
            );
        }
        assert_eq!(TAKEOVER.len(), 164);
    }

    /// The meta table is the live snapshot's attributes.
    #[test]
    fn the_meta_table_is_the_live_snapshot() {
        let live = crate::ha::mapping::tests::fixture();
        for (id, s, d, u) in HA_META {
            let a = &live[*id].attributes;
            let get = |k: &str| a.get(k).and_then(|v| v.as_str());
            assert_eq!((get("state_class"), get("device_class"), get("unit_of_measurement")), (*s, *d, *u), "{id}");
        }
    }

    /// Without `allow_set` a settable entity is offered as a plain sensor, with no command topic.
    #[test]
    fn without_allow_set_discovery_offers_no_controls() {
        let mut p = Publisher::new(crate::bridge::BridgeConfig {
            discovery_prefix: Some("homeassistant".into()),
            allow_set: false,
            ..cfg()
        });
        let mut e = Entity::sensor("reservering", "Reservering", Value::Number(1.0)).in_group("setting");
        e.control = Some(Control::Number { min: Some(0.0), max: None, step: 50.0 });
        let m = p.tick(&crate::bridge::tests::snap(vec![e]), 0.0);
        let config = m.iter().find(|m| m.topic.ends_with("/config")).expect("discovery");
        assert_eq!(config.topic, "homeassistant/sensor/energie/setting_reservering/config");
        let body: Json = serde_json::from_slice(config.payload.as_ref()).unwrap();
        assert_eq!(body.get("command_topic"), None);
    }

    /// The publisher sends a config per wanted entity once, again only when it changes (an
    /// empty config first when the component changed), and an empty one when an entity goes.
    #[test]
    fn discovery_is_sent_on_change_only() {
        let mut p =
            Publisher::new(crate::bridge::BridgeConfig { discovery_prefix: Some("homeassistant".into()), ..cfg() });
        let mut e = Entity::sensor("plugged", "Aangesloten", Value::Null).in_group("ev3");
        let other = Entity::sensor("soc", "SoC", Value::Number(1.0)).in_group("ev3");
        let snap = |es: Vec<Entity>| crate::bridge::tests::snap(es);
        let m = p.tick(&snap(vec![e.clone(), other.clone()]), 0.0);
        let configs: Vec<_> = m.iter().filter(|m| m.topic.ends_with("/config")).map(|m| m.topic.as_str()).collect();
        assert_eq!(
            configs,
            ["homeassistant/sensor/energie/ev3_plugged/config", "homeassistant/sensor/energie/ev3_soc/config"]
        );
        assert!(p.tick(&snap(vec![e.clone(), other.clone()]), 1.0).iter().all(|m| !m.topic.ends_with("/config")));
        e.value = Value::Bool(true);
        let m = p.tick(&snap(vec![e.clone(), other.clone()]), 2.0);
        let configs: Vec<_> = m
            .iter()
            .filter(|m| m.topic.ends_with("/config"))
            .map(|m| (m.topic.as_str(), m.payload.is_empty()))
            .collect();
        assert_eq!(
            configs,
            [
                ("homeassistant/sensor/energie/ev3_plugged/config", true),
                ("homeassistant/binary_sensor/energie/ev3_plugged/config", false)
            ]
        );
        let m = p.tick(&snap(vec![e]), 3.0);
        let configs: Vec<_> = m
            .iter()
            .filter(|m| m.topic.ends_with("/config"))
            .map(|m| (m.topic.as_str(), m.payload.is_empty()))
            .collect();
        assert_eq!(configs, [("homeassistant/sensor/energie/ev3_soc/config", true)]);
    }
}
