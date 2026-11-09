//! The entity registry: every value the app knows, as [`Entity`]s with stable, topic-safe ids,
//! for the dashboard (`/api/entities`, the `entities` SSE event), the history and a future
//! MQTT/HA-discovery bridge.
//!
//! | Group | Key | What |
//! | --- | --- | --- |
//! | `world` | `<slot>_<field>` (`grid_meter_power_w`) | the World as the logic sees it, by role; settable where the role has a controller |
//! | `<device id>` | `<field>` (`power_w`), and the driver's own keys | one native instance's own readings (each PV inverter before the sum); the driver's own settable entities (the EV3's AC charging limit) while the instance is writable |
//! | `derived` | the `Derived` field (`pv_vermogen_totaal_w`) | template sensors and meters |
//! | `setting` | the `Settings` field | the helpers; settable when the settings are editable |
//! | `memory` | the `Memory` field (`capaciteit_piekhistoriek`) | the latches, counters and measurements the controllers keep; read only |
//!
//! Every entity that stands for an existing Home Assistant entity names it in `ha_entity`
//! (World fields by [`crate::ha::mapping::world_bindings`], derived values by
//! [`crate::ha::outputs`], settings and memory by their helper bindings, a driver's extras by
//! the driver), so an HA bridge can take over that entity id.
//!
//! The id is `<group>_<key>`, the MQTT topic suffix `<group>/<key>`. `[[publish]]` rules set
//! [`Publish`] per entity (glob on the id, later rules win):
//!
//! ```toml
//! [[publish]]
//! match = "world_grid_meter_*"   # the 1 s P1 values
//! min_interval_s = 5
//! deadband = 10
//! [[publish]]
//! match = "derived_kwartier_*"
//! enabled = false
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use energie_core::command::{AircoUnit, Command};
use energie_core::derived::Derived;
use energie_core::memory::Memory;
use energie_core::settings::Settings;
use energie_core::world::World;
use energie_drivers::entity::{Control, Entity, Publish, Value, class_of, is_topic_safe};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::error::{Error, Result};
use crate::ha::mapping::{memory_bindings, settings_bindings, world_bindings};
use crate::ha::outputs::{Kind, outputs};
use crate::plug::{Device, DeviceView, Section};
use crate::web::fields::fields;

/// One `[[publish]]` rule.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishRule {
    /// A glob on the entity id (`*` matches any run of characters).
    #[serde(rename = "match")]
    pub pattern: String,
    pub enabled: Option<bool>,
    pub min_interval_s: Option<f64>,
    pub deadband: Option<f64>,
}

impl PublishRule {
    pub fn check(&self) -> Result<()> {
        let bad = |x: Option<f64>| x.is_some_and(|v| v.is_nan() || v < 0.0);
        if self.pattern.is_empty() || bad(self.min_interval_s) || bad(self.deadband) {
            return Err(Error::new(format!("[[publish]] {:?}: needs a match and non-negative numbers", self.pattern)));
        }
        Ok(())
    }

    fn apply(&self, p: &mut Publish) {
        p.enabled = self.enabled.unwrap_or(p.enabled);
        p.min_interval_s = self.min_interval_s.or(p.min_interval_s);
        p.deadband = self.deadband.or(p.deadband);
    }
}

/// `*`-glob match.
pub fn glob(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        None => pat == s,
        Some((head, rest)) => {
            s.starts_with(head) && (0..=s.len() - head.len()).any(|i| glob(rest, &s[head.len() + i..]))
        }
    }
}

/// Apply the rules in order.
pub fn apply_rules(es: &mut [Entity], rules: &[PublishRule]) {
    for e in es {
        for r in rules.iter().filter(|r| glob(&r.pattern, &e.id)) {
            r.apply(&mut e.publish);
        }
    }
}

type Get = fn(&World) -> Value;
type ToCommand = fn(&Value) -> Option<Command>;

/// One World field as an entity.
pub struct WorldField {
    pub section: Section,
    pub key: &'static str,
    pub name: &'static str,
    /// Device class and unit (empty: none).
    pub class: &'static str,
    pub unit: &'static str,
    pub get: Get,
    pub control: Option<(Control, ToCommand)>,
}

impl WorldField {
    fn entity(&self, w: &World, group: &str, key: String, settable: bool) -> Entity {
        let mut e = Entity::sensor(key, self.name, (self.get)(w));
        if !self.unit.is_empty() {
            e = e.with_unit(self.class, self.unit);
        }
        e.control = self.control.as_ref().filter(|_| settable).map(|(c, _)| c.clone());
        e.in_group(group)
    }
}

const W: (&str, &str) = ("power", "W");
const KW: (&str, &str) = ("power", "kW");
const KWH: (&str, &str) = ("energy", "kWh");
const A: (&str, &str) = ("current", "A");
const V: (&str, &str) = ("voltage", "V");
const PCT: (&str, &str) = ("", "%");
const SOC: (&str, &str) = ("battery", "%");
const KM: (&str, &str) = ("distance", "km");
const MIN: (&str, &str) = ("duration", "min");
const EUR_KWH: (&str, &str) = ("", "EUR/kWh");
const NONE: (&str, &str) = ("", "");

fn text<T: serde::Serialize>(v: Option<T>) -> Value {
    v.map_or(Value::Null, |m| {
        Value::Text(serde_json::to_value(m).ok().and_then(|j| j.as_str().map(String::from)).unwrap_or_default())
    })
}

fn choice<T: DeserializeOwned>(v: &Value) -> Option<T> {
    match v {
        Value::Text(t) => serde_json::from_value(json!(t)).ok(),
        _ => None,
    }
}

fn on(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn watts(min: f64) -> Control {
    Control::Number { min: Some(min), max: None, step: 1.0 }
}

fn select(options: &[&str]) -> Control {
    Control::Select { options: options.iter().map(|o| o.to_string()).collect() }
}

/// Every World field, in World order.
pub fn world_fields() -> &'static [WorldField] {
    static F: OnceLock<Vec<WorldField>> = OnceLock::new();
    F.get_or_init(|| {
        use Section::*;
        let f = |section, key, name, (class, unit): (&'static str, &'static str), get: Get| WorldField {
            section,
            key,
            name,
            class,
            unit,
            get,
            control: None,
        };
        let c =
            |field: WorldField, control: Control, cmd: ToCommand| WorldField { control: Some((control, cmd)), ..field };
        vec![
            f(P1, "power_w", "Vermogen", W, |w| w.p1.power_w.into()),
            f(P1, "phase_1_a", "Stroom fase 1", A, |w| w.p1.phase_a[0].into()),
            f(P1, "phase_2_a", "Stroom fase 2", A, |w| w.p1.phase_a[1].into()),
            f(P1, "phase_3_a", "Stroom fase 3", A, |w| w.p1.phase_a[2].into()),
            f(P1, "import_kwh", "Import", KWH, |w| w.p1.import_kwh.into()),
            f(P1, "export_kwh", "Export", KWH, |w| w.p1.export_kwh.into()),
            f(P1, "month_peak_w", "Maandpiek", W, |w| w.p1.month_peak_w.into()),
            f(Solis, "soc", "Laadtoestand", SOC, |w| w.solis.soc.into()),
            f(Solis, "battery_power_w", "Batterijvermogen", W, |w| w.solis.battery_power_w.into()),
            f(Solis, "pv_power_w", "PV-vermogen", W, |w| w.solis.pv_power_w.into()),
            c(
                f(Solis, "rc_mode", "RC-modus", NONE, |w| text(w.solis.rc_mode)),
                select(&["None", "ForceCharge", "ForceDischarge"]),
                |v| choice(v).map(Command::SolisRcMode),
            ),
            c(f(Solis, "rc_charge_w", "RC-laadvermogen", W, |w| w.solis.rc_charge_w.into()), watts(0.0), |v| {
                v.as_f64().map(|x| Command::SolisRcChargeW(x as u32))
            }),
            c(
                f(Solis, "rc_discharge_w", "RC-ontlaadvermogen", W, |w| w.solis.rc_discharge_w.into()),
                watts(0.0),
                |v| v.as_f64().map(|x| Command::SolisRcDischargeW(x as u32)),
            ),
            c(f(Solis, "rc_timeout_min", "RC-timeout", MIN, |w| w.solis.rc_timeout_min.into()), watts(0.0), |v| {
                v.as_f64().map(|x| Command::SolisRcTimeoutMin(x as u32))
            }),
            c(
                f(Solis, "allow_grid_charge", "Laden van het net", NONE, |w| w.solis.allow_grid_charge.into()),
                Control::Switch,
                |v| on(v).map(Command::SolisAllowGridCharge),
            ),
            c(
                f(Solis, "feed_in_limit_on", "Injectielimiet", NONE, |w| w.solis.feed_in_limit_on.into()),
                Control::Switch,
                |v| on(v).map(Command::SolisFeedInLimit),
            ),
            c(f(Solis, "backflow_w", "Injectielimiet vermogen", W, |w| w.solis.backflow_w.into()), watts(0.0), |v| {
                v.as_f64().map(|x| Command::SolisBackflowW(x as u32))
            }),
            f(Omnik, "power_w", "PV-vermogen", W, |w| w.omnik.power_w.into()),
            f(Evse, "plug", "Stekker", NONE, |w| w.evse.plug.as_deref().into()),
            f(Evse, "state", "Toestand", NONE, |w| w.evse.state.as_deref().into()),
            f(Evse, "state_id", "Pilot", NONE, |w| w.evse.state_id.as_deref().into()),
            c(
                f(Evse, "mode", "Modus", NONE, |w| text(w.evse.mode)),
                select(&["Off", "Normal", "Smart", "Solar", "Pause"]),
                |v| choice(v).map(Command::EvseMode),
            ),
            c(
                f(Evse, "override_a", "Laadstroom", A, |w| w.evse.override_a.into()),
                Control::Number { min: Some(0.0), max: Some(32.0), step: 0.1 },
                |v| v.as_f64().map(|a| Command::EvseCurrentDeciA((a * 10.0).round() as u32)),
            ),
            f(Evse, "override_max_a", "Laadstroom max", A, |w| w.evse.override_max_a.into()),
            f(Evse, "charge_current_a", "Stroom", A, |w| w.evse.charge_current_a.into()),
            f(Cable, "status", "Toestand", NONE, |w| w.cable.status.as_deref().into()),
            f(Cable, "current_a", "Stroom", A, |w| w.cable.current_a.into()),
            f(Cable, "voltage_v", "Spanning", V, |w| w.cable.voltage_v.into()),
            f(Cable, "power_kw", "Vermogen", KW, |w| w.cable.power_kw.into()),
            c(
                f(Cable, "set_current_a", "Laadstroom", A, |w| w.cable.set_current_a.into()),
                Control::Number { min: Some(6.0), max: Some(16.0), step: 1.0 },
                |v| v.as_f64().map(|a| Command::CableSetCurrent(a as u8)),
            ),
            f(Cable, "set_current_max_a", "Laadstroom max", A, |w| w.cable.set_current_max_a.into()),
            f(Ev3, "plugged", "Aangesloten", NONE, |w| w.ev3.plugged.into()),
            f(Ev3, "soc", "Laadtoestand", SOC, |w| w.ev3.soc.into()),
            f(Ev3, "ac_limit", "AC-laadlimiet", PCT, |w| w.ev3.ac_limit.into()),
            f(Ev3, "odometer_km", "Kilometerstand", KM, |w| w.ev3.odometer_km.into()),
            f(Buzz, "soc", "Laadtoestand", SOC, |w| w.buzz.soc.into()),
            f(Buzz, "target_soc", "Laaddoel", PCT, |w| w.buzz.target_soc.into()),
            f(Buzz, "odometer_km", "Kilometerstand", KM, |w| w.buzz.odometer_km.into()),
            f(Prices, "afname_now", "Afnameprijs", EUR_KWH, |w| w.prices.afname_now.into()),
            f(Prices, "afname_pct_in_range", "Plaats in de prijsband", PCT, |w| w.prices.afname_pct_in_range.into()),
            f(Prices, "afname_slots", "Kwartierprijzen", NONE, |w| Value::Number(w.prices.afname_slots.len() as f64)),
            f(Prices, "afname_has_tomorrow", "Prijzen morgen", NONE, |w| Value::Bool(w.prices.afname_has_tomorrow)),
            f(Prices, "injectie_now", "Injectieprijs", EUR_KWH, |w| w.prices.injectie_now.into()),
            f(Prices, "injectie_has_tomorrow", "Injectieprijzen morgen", NONE, |w| {
                Value::Bool(w.prices.injectie_has_tomorrow)
            }),
            f(Prices, "epex_now", "EPEX", EUR_KWH, |w| w.prices.epex_now.into()),
            f(Prices, "backup_now", "Reserveprijs", EUR_KWH, |w| w.prices.backup_now.into()),
            c(f(Airco, "beneden_on", "Airco beneden", NONE, |w| w.airco.beneden_on.into()), Control::Switch, |v| {
                on(v).map(|on| Command::Airco { unit: AircoUnit::Beneden, on })
            }),
            c(f(Airco, "zolder_on", "Airco zolder", NONE, |w| w.airco.zolder_on.into()), Control::Switch, |v| {
                on(v).map(|on| Command::Airco { unit: AircoUnit::Zolder, on })
            }),
            f(Vriezer, "power_w", "Vermogen", W, |w| w.vriezer.power_w.into()),
            f(Vriezer, "switch_on", "Aan", NONE, |w| w.vriezer.switch_on.into()),
            f(Fluvius, "peak_kw", "Capaciteitspiek", KW, |w| w.fluvius.peak_kw.into()),
        ]
    })
}

/// World fields HA shows as an attribute of another entity, or not at all: no `ha_entity`.
const HA_ATTRIBUTES: [&str; 5] = [
    "evse.override_max_a",
    "cable.set_current_max_a",
    "prices.afname_slots",
    "prices.afname_has_tomorrow",
    "prices.injectie_has_tomorrow",
];

/// The HA entity of a World field (`p1.power_w` → `sensor.p1_meter_vermogen`), from the
/// bindings the HA source reads it with; `p1.phase_a[0]` is the field `phase_1_a`.
pub fn ha_of_world(section: Section, key: &str) -> Option<&'static str> {
    static M: OnceLock<BTreeMap<String, &'static str>> = OnceLock::new();
    let m = M.get_or_init(|| {
        world_bindings()
            .into_iter()
            .filter(|b| !HA_ATTRIBUTES.contains(&b.field))
            .map(|b| {
                let field = match b.field.split_once('[') {
                    Some((head, i)) => {
                        let n: usize = i.trim_end_matches(']').parse().expect("an index");
                        format!("{}_{}_a", head.trim_end_matches("_a"), n + 1)
                    }
                    None => b.field.to_string(),
                };
                (field, b.entity)
            })
            .collect()
    });
    let section = serde_json::to_value(section).expect("a section serialises");
    m.get(&format!("{}.{key}", section.as_str().expect("as a string"))).copied()
}

/// The World by role: group `world`, key `<slot>_<field>`. A field is settable when its
/// device has a controller (`settable(device)`).
pub fn world_entities(w: &World, settable: &dyn Fn(Device) -> bool) -> Vec<Entity> {
    world_fields()
        .iter()
        .map(|f| {
            let can = Device::of_slot(f.section.slot()).is_some_and(settable);
            let mut e = f.entity(w, "world", format!("{}_{}", f.section.slot().key(), f.key), can);
            e.ha_entity = ha_of_world(f.section, f.key).map(String::from);
            e
        })
        .collect()
}

/// One native instance's own readings (its slots' fields, prefixed by the slot when it reads
/// several) and the driver's extra entities. An extra with the key of a slot field takes its
/// place (so a driver can make one of its fields settable); an extra keeps its control only
/// while the instance is settable. World commands go through the role, not here.
pub fn device_entities(v: &dyn DeviceView) -> Vec<Entity> {
    let w = v.world();
    let several = v.slots().len() > 1;
    let mut out: Vec<Entity> = v
        .slots()
        .iter()
        .filter_map(|s| Section::of_slot(*s).map(|sec| (s, sec)))
        .flat_map(|(s, sec)| world_fields().iter().filter(move |f| f.section == sec).map(move |f| (s, f)))
        .map(|(s, f)| {
            let key = if several { format!("{}_{}", s.key(), f.key) } else { f.key.to_string() };
            f.entity(&w, v.id(), key, false)
        })
        .collect();
    let settable = v.settable();
    for mut e in v.extra().into_iter().filter(|e| is_topic_safe(&e.key)) {
        if !settable {
            e.control = None;
        }
        let e = e.in_group(v.id());
        match out.iter_mut().find(|x| x.key == e.key) {
            Some(x) => *x = e,
            None => out.push(e),
        }
    }
    out
}

fn humanize(key: &str) -> String {
    let s = key.replace('_', " ");
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The HA entity that shows a `Derived` field ([`outputs`]; not the rows that compare an
/// attribute or only whether the entity has a value).
pub fn ha_of_derived(field: &str) -> Option<&'static str> {
    outputs().into_iter().find(|o| o.field == field && o.attr.is_none() && o.kind != Kind::Available).map(|o| o.entity)
}

/// Every `Derived` field: group `derived`, key the field name, unit from its suffix.
pub fn derived_entities(d: &Derived) -> Vec<Entity> {
    static HA: OnceLock<BTreeMap<String, &'static str>> = OnceLock::new();
    let ha = HA.get_or_init(|| {
        let v = serde_json::to_value(Derived::default()).expect("Derived serialises");
        let keys = v.as_object().expect("Derived is a struct").keys();
        keys.filter_map(|k| ha_of_derived(k).map(|e| (k.clone(), e))).collect()
    });
    let v = serde_json::to_value(d).expect("Derived serialises");
    v.as_object()
        .expect("Derived is a struct")
        .iter()
        .map(|(k, v)| {
            let value = Value::from_json(v).unwrap_or_else(|| Value::Text(v.to_string()));
            let mut e = Entity::sensor(k.clone(), humanize(k), value);
            if let Some((class, unit)) = class_of(k) {
                e = e.with_unit(class, unit);
            }
            e.ha_entity = ha.get(k).map(|h| h.to_string());
            e.in_group("derived")
        })
        .collect()
}

/// Every setting: group `setting`, key the field name; select values are the option labels.
pub fn settings_entities(s: &Settings, editable: bool) -> Vec<Entity> {
    let cur = serde_json::to_value(s).expect("Settings serialise");
    fields()
        .into_iter()
        .map(|f| {
            let v = &cur[f.key];
            let (value, control) = match f.kind {
                "select" => (
                    f.options.iter().find(|o| &o.value == v).map_or(Value::Null, |o| Value::Text(o.label.clone())),
                    Control::Select { options: f.options.iter().map(|o| o.label.clone()).collect() },
                ),
                "bool" => (Value::from(v.as_bool()), Control::Switch),
                _ => (Value::from(v.as_f64()), Control::Number { min: f.min, max: f.max, step: f.step }),
            };
            let mut e = Entity::sensor(f.key, f.label, value);
            if !f.unit.is_empty() {
                e = e.with_unit("", f.unit);
            }
            e.control = editable.then_some(control);
            e.ha_entity = settings_bindings().into_iter().find(|b| b.field == f.key).map(|b| b.entity.to_string());
            e.in_group("setting")
        })
        .collect()
}

/// What kind of value a `Memory` field holds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemoryKind {
    /// A float, with its device class and unit (empty: none).
    Number(&'static str, &'static str),
    Flag,
    /// A counter (`counter.*`): a whole number ≥ 0.
    Count,
    Text,
    Time,
}

/// One `Memory` field as an entity.
pub struct MemoryField {
    pub key: &'static str,
    pub name: &'static str,
    pub kind: MemoryKind,
}

/// The `Memory` fields shown as entities: the hidden HA helpers and counters, in struct order
/// (not the meters and automation timestamps, which have their own entities or none).
pub fn memory_fields() -> &'static [MemoryField] {
    use MemoryKind::*;
    const KWH: MemoryKind = Number("energy", "kWh");
    const F: &[MemoryField] = &[
        MemoryField { key: "batterij_setpoint_bias", name: "Batterij setpoint-bias", kind: Number("power", "W") },
        MemoryField { key: "batterij_net_laden", name: "Batterij laden van het net", kind: Flag },
        MemoryField { key: "soluna_absorptie_bezig", name: "Batterijsturing bezig", kind: Flag },
        MemoryField { key: "ev_hoger_laadstil", name: "Hoger gerangschikte auto laadt niet", kind: Flag },
        MemoryField { key: "ev_hoger_laadstil_sinds", name: "Laadstilstand sinds (epoch s)", kind: Number("", "") },
        MemoryField { key: "airco_dump_sessie_bezig", name: "Airco-dump bezig", kind: Flag },
        MemoryField { key: "smartevse_plug_laatst_bekend", name: "SmartEVSE stekker laatst bekend", kind: Text },
        MemoryField { key: "batterij_naar_auto_start_kwh", name: "Batterij-dump startmeting", kind: KWH },
        MemoryField { key: "nacht_import_startmeting", name: "Nacht import startmeting", kind: KWH },
        MemoryField { key: "nacht_ontlaad_startmeting", name: "Nacht ontlading startmeting", kind: KWH },
        MemoryField { key: "nacht_ev_net_startmeting", name: "Nacht EV-net startmeting", kind: KWH },
        MemoryField { key: "nachtdekking", name: "Nachtdekking", kind: Number("", "%") },
        MemoryField { key: "nacht_batterij_kwh", name: "Nacht batterij", kind: KWH },
        MemoryField { key: "nacht_import_kwh", name: "Nacht import", kind: KWH },
        MemoryField { key: "nacht_duur_uren", name: "Nachtduur", kind: Number("", "u") },
        MemoryField { key: "capaciteit_piekhistoriek", name: "Piekhistoriek", kind: Text },
        MemoryField { key: "watchdog_laatste_herstart", name: "Watchdog laatste herstart", kind: Time },
        MemoryField { key: "peak_guard_interventies", name: "Peak guard interventies", kind: Count },
        MemoryField { key: "buzz_laadweigering", name: "Buzz laadweigeringen", kind: Count },
        MemoryField { key: "steering_volgende_update", name: "Volgende sturingscyclus", kind: Time },
        MemoryField { key: "capaciteitspiek_risico_sinds", name: "Piekrisico gewijzigd", kind: Time },
        MemoryField { key: "smartevse_modus_laatst_bekend", name: "SmartEVSE modus laatst bekend", kind: Text },
    ];
    F
}

/// The memory fields: group `memory`, read only (the controllers own them; the state dir
/// keeps them, the import fills them from HA).
pub fn memory_entities(m: &Memory) -> Vec<Entity> {
    let cur = serde_json::to_value(m).expect("Memory serialises");
    memory_fields()
        .iter()
        .map(|f| {
            let value = Value::from_json(&cur[f.key]).expect("a scalar field");
            let mut e = Entity::sensor(f.key, f.name, value);
            match f.kind {
                MemoryKind::Number(class, unit) if !unit.is_empty() => e = e.with_unit(class, unit),
                MemoryKind::Time => e.class = Some("timestamp".into()),
                _ => {}
            }
            e.ha_entity = memory_bindings().into_iter().find(|b| b.field == f.key).map(|b| b.entity.to_string());
            e.in_group("memory")
        })
        .collect()
}

/// What the registry is built from, once per tick.
pub struct Inputs<'a> {
    pub world: &'a World,
    pub derived: &'a Derived,
    pub settings: &'a Settings,
    pub settings_editable: bool,
    pub memory: &'a Memory,
    pub settable: &'a dyn Fn(Device) -> bool,
    pub devices: &'a [Arc<dyn DeviceView>],
    pub rules: &'a [PublishRule],
}

/// The whole registry: world, devices, derived, settings, memory, with the publish rules
/// applied.
pub fn collect(i: &Inputs) -> Vec<Entity> {
    let mut out = world_entities(i.world, i.settable);
    for d in i.devices {
        out.extend(device_entities(d.as_ref()));
    }
    out.extend(derived_entities(i.derived));
    out.extend(settings_entities(i.settings, i.settings_editable));
    out.extend(memory_entities(i.memory));
    apply_rules(&mut out, i.rules);
    out
}

/// What setting an entity does.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A partial settings change in the serde shape of `Settings`.
    Settings(serde_json::Value),
    Command(Command),
    /// A control switch (`control_enabled`, `control_<device>`).
    Control {
        key: String,
        on: bool,
    },
    /// One of a device instance's own entities.
    Device {
        id: String,
        key: String,
        value: Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetError {
    Unknown,
    /// The entity exists but cannot be set now (read only, shadow mode, lockstep).
    NotSettable,
    Rejected(String),
}

/// Resolve setting entity `id` to `v` against the current registry.
pub fn target(entities: &[Entity], id: &str, v: &Value) -> std::result::Result<Target, SetError> {
    let e = entities.iter().find(|e| e.id == id).ok_or(SetError::Unknown)?;
    let control = e.control.as_ref().ok_or(SetError::NotSettable)?;
    if !control.accepts(v) {
        return Err(SetError::Rejected(format!("{id}: {} does not fit {}", json!(v), json!(control))));
    }
    if e.group == "setting" {
        let f = fields().into_iter().find(|f| f.key == e.key).expect("a setting entity has a field");
        let value = match (f.kind, v) {
            ("select", Value::Text(t)) => f.options.into_iter().find(|o| &o.label == t).expect("accepted").value,
            _ => json!(v),
        };
        return Ok(Target::Settings(json!({ f.key: value })));
    }
    if e.group == "control" {
        let on = matches!(v, Value::Bool(true));
        return Ok(Target::Control { key: e.key.clone(), on });
    }
    if e.group != "world" {
        return Ok(Target::Device { id: e.group.clone(), key: e.key.clone(), value: v.clone() });
    }
    let f = world_fields()
        .iter()
        .find(|f| format!("{}_{}", f.section.slot().key(), f.key) == e.key)
        .expect("only world fields carry a control");
    let (_, to_command) = f.control.as_ref().expect("a settable field has a command");
    to_command(v).map(Target::Command).ok_or_else(|| SetError::Rejected(format!("{id}: no command for {}", json!(v))))
}

/// The values by id, for change detection (SSE, publishers).
pub fn values(es: &[Entity]) -> BTreeMap<String, Value> {
    es.iter().map(|e| (e.id.clone(), e.value.clone())).collect()
}

/// The entries of `now` that differ from `last` (all of them when `last` is empty).
pub fn changes(last: &BTreeMap<String, Value>, now: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    now.iter().filter(|(k, v)| last.get(*k) != Some(v)).map(|(k, v)| (k.clone(), v.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plug::DriverHealth;
    use energie_core::world::{EvseMode, RcMode};
    use energie_drivers::plugin::Slot;

    /// Every World field has one entity, with a topic-safe unique id; every World section is
    /// covered.
    #[test]
    fn every_world_field_is_an_entity() {
        let es = world_entities(&World::default(), &|_| false);
        let mut ids: Vec<_> = es.iter().map(|e| e.id.clone()).collect();
        assert!(ids.iter().all(|i| is_topic_safe(i)), "{ids:?}");
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), es.len());
        for s in Section::ALL {
            assert!(world_fields().iter().any(|f| f.section == s), "{s:?}");
        }
        // Every `Option` field of the World, plus the price-slot count and two flags.
        let json = serde_json::to_value(World::default()).unwrap();
        let n: usize = json.as_object().unwrap().values().map(|v| v.as_object().unwrap().len()).sum();
        assert_eq!(es.len(), n + 2, "phase_a is three entities");
        assert!(es.iter().all(|e| e.control.is_none()), "nothing settable without a controller");
        assert!(es.iter().all(|e| e.value == Value::Null || e.key.starts_with("price_feed")));
    }

    /// Values follow the World; a controlled device's fields carry their control.
    #[test]
    fn world_values_and_controls() {
        let mut w = World::default();
        w.p1.power_w = Some(-250.0);
        w.solis.rc_mode = Some(RcMode::ForceCharge);
        w.evse.mode = Some(EvseMode::Solar);
        w.evse.plug = Some("Connected".into());
        let es = world_entities(&w, &|d| d == Device::Evse);
        let get = |id: &str| es.iter().find(|e| e.id == id).unwrap();
        let p = get("world_grid_meter_power_w");
        assert_eq!(
            (p.value.clone(), p.class.as_deref(), p.unit.as_deref()),
            (Value::Number(-250.0), Some("power"), Some("W"))
        );
        assert_eq!(p.topic(), "world/grid_meter_power_w");
        assert_eq!(get("world_battery_inverter_rc_mode").value, Value::Text("ForceCharge".into()));
        assert!(get("world_battery_inverter_rc_mode").control.is_none(), "the battery has no controller");
        assert_eq!(get("world_charger_buzz_mode").value, Value::Text("Solar".into()));
        assert_eq!(get("world_charger_buzz_plug").value, Value::Text("Connected".into()));
        assert!(matches!(get("world_charger_buzz_override_a").control, Some(Control::Number { max: Some(32.0), .. })));
        assert_eq!(get("world_car_ev3_soc").class.as_deref(), Some("battery"));
        assert_eq!(get("world_charger_buzz_plug").unit, None);
    }

    /// LOGIC: every settable World field turns a value into the command HA's entity would get.
    #[test]
    fn settable_world_fields_become_commands() {
        let es = world_entities(&World::default(), &|_| true);
        let set = |id: &str, v: Value| target(&es, id, &v);
        let cmd = |c| Ok(Target::Command(c));
        let n = Value::Number;
        let t = |s: &str| Value::Text(s.into());
        assert_eq!(set("world_charger_buzz_override_a", n(14.5)), cmd(Command::EvseCurrentDeciA(145)));
        assert_eq!(set("world_charger_buzz_mode", t("Normal")), cmd(Command::EvseMode(EvseMode::Normal)));
        assert_eq!(set("world_charger_ev3_set_current_a", n(10.0)), cmd(Command::CableSetCurrent(10)));
        assert_eq!(set("world_battery_inverter_rc_mode", t("None")), cmd(Command::SolisRcMode(RcMode::None)));
        assert_eq!(set("world_battery_inverter_rc_charge_w", n(1500.0)), cmd(Command::SolisRcChargeW(1500)));
        assert_eq!(set("world_battery_inverter_rc_discharge_w", n(900.0)), cmd(Command::SolisRcDischargeW(900)));
        assert_eq!(set("world_battery_inverter_rc_timeout_min", n(10.0)), cmd(Command::SolisRcTimeoutMin(10)));
        assert_eq!(
            set("world_battery_inverter_allow_grid_charge", Value::Bool(true)),
            cmd(Command::SolisAllowGridCharge(true))
        );
        assert_eq!(
            set("world_battery_inverter_feed_in_limit_on", Value::Bool(false)),
            cmd(Command::SolisFeedInLimit(false))
        );
        assert_eq!(set("world_battery_inverter_backflow_w", n(0.0)), cmd(Command::SolisBackflowW(0)));
        assert_eq!(
            set("world_load_airco_beneden_on", Value::Bool(true)),
            cmd(Command::Airco { unit: AircoUnit::Beneden, on: true })
        );
        assert_eq!(
            set("world_load_airco_zolder_on", Value::Bool(false)),
            cmd(Command::Airco { unit: AircoUnit::Zolder, on: false })
        );
        assert_eq!(set("world_charger_ev3_set_current_a", n(17.0)).unwrap_err().clone(), SetError::Rejected(
            "world_charger_ev3_set_current_a: 17.0 does not fit {\"max\":16.0,\"min\":6.0,\"step\":1.0,\"type\":\"number\"}".into()
        ));
        assert_eq!(set("world_charger_buzz_mode", t("Turbo")).unwrap_err(), SetError::Rejected(
            "world_charger_buzz_mode: \"Turbo\" does not fit {\"options\":[\"Off\",\"Normal\",\"Smart\",\"Solar\",\"Pause\"],\"type\":\"select\"}".into()
        ));
        assert_eq!(set("world_grid_meter_power_w", n(1.0)), Err(SetError::NotSettable));
        assert_eq!(set("world_bestaat_niet", n(1.0)), Err(SetError::Unknown));
        // The select accepts only the options; an option that does not parse has no command.
        let mut odd = es.clone();
        let mode = odd.iter_mut().find(|e| e.id == "world_charger_buzz_mode").unwrap();
        mode.control = Some(select(&["Turbo"]));
        assert_eq!(
            target(&odd, "world_charger_buzz_mode", &t("Turbo")),
            Err(SetError::Rejected("world_charger_buzz_mode: no command for \"Turbo\"".into()))
        );
        let sw = odd.iter_mut().find(|e| e.id == "world_load_airco_zolder_on").unwrap();
        sw.control = Some(Control::Number { min: None, max: None, step: 1.0 });
        assert!(target(&odd, "world_load_airco_zolder_on", &n(1.0)).is_err(), "a switch needs a bool");
        let rc = odd.iter_mut().find(|e| e.id == "world_battery_inverter_rc_mode").unwrap();
        rc.control = Some(Control::Number { min: None, max: None, step: 1.0 });
        assert!(target(&odd, "world_battery_inverter_rc_mode", &n(1.0)).is_err(), "a select needs a text");
    }

    /// Settings are entities; selects show and take their labels, numbers and switches their
    /// values; read only when not editable.
    #[test]
    fn settings_are_settable_entities() {
        let s = Settings::default();
        let es = settings_entities(&s, true);
        assert_eq!(es.len(), fields().len());
        let get = |id: &str| es.iter().find(|e| e.id == id).unwrap();
        assert_eq!(get("setting_ev_net_niveau").value, Value::Text(s.ev_net_niveau.label().into()));
        assert_eq!(get("setting_peak_guard_actief").value, Value::Bool(true));
        assert_eq!(get("setting_goedkoop_laden_prijs_drempel").value, Value::Number(0.38));
        assert_eq!(get("setting_goedkoop_laden_prijs_drempel").unit.as_deref(), Some("€/kWh"));
        assert_eq!(get("setting_peak_guard_actief").unit, None);
        assert_eq!(
            target(&es, "setting_ev_net_niveau", &Value::Text("Uit".into())),
            Ok(Target::Settings(json!({"ev_net_niveau": "Uit"})))
        );
        assert_eq!(
            target(&es, "setting_peak_guard_actief", &Value::Bool(false)),
            Ok(Target::Settings(json!({"peak_guard_actief": false})))
        );
        assert_eq!(
            target(&es, "setting_batterij_reservering_w", &Value::Number(500.0)),
            Ok(Target::Settings(json!({"batterij_reservering_w": 500.0})))
        );
        let ro = settings_entities(&s, false);
        assert_eq!(target(&ro, "setting_peak_guard_actief", &Value::Bool(false)), Err(SetError::NotSettable));
        // A select value no option has is unavailable.
        let mut odd = serde_json::to_value(&s).unwrap();
        odd["zonprioriteit"] = json!(["Ev3", "Ev3", "Ev3"]);
        let odd: Settings = serde_json::from_value(odd).unwrap();
        assert_eq!(settings_entities(&odd, false)[0].value, Value::Null);
    }

    /// Derived fields are entities with units from their suffix.
    #[test]
    fn derived_fields_are_entities() {
        let d = Derived { pv_vermogen_totaal_w: 1234.0, capaciteitspiek_risico: true, ..Default::default() };
        let es = derived_entities(&d);
        let n = serde_json::to_value(&d).unwrap().as_object().unwrap().len();
        assert_eq!(es.len(), n);
        let get = |id: &str| es.iter().find(|e| e.id == id).unwrap();
        let pv = get("derived_pv_vermogen_totaal_w");
        assert_eq!(
            (pv.value.clone(), pv.unit.as_deref(), pv.name.as_str()),
            (Value::Number(1234.0), Some("W"), "Pv vermogen totaal w")
        );
        assert_eq!(get("derived_capaciteitspiek_risico").value, Value::Bool(true));
        assert_eq!(get("derived_ev_actieve_laadauto").value, Value::Text("Buzz".into()));
        assert_eq!(get("derived_kwartier_import_kwh").value, Value::Null);
        assert_eq!(humanize(""), "");
    }

    struct View(Vec<Slot>, World, Vec<Entity>);
    impl DeviceView for View {
        fn id(&self) -> &str {
            "pv_garage"
        }
        fn kind(&self) -> &str {
            "static"
        }
        fn slots(&self) -> &[Slot] {
            &self.0
        }
        fn world(&self) -> World {
            self.1.clone()
        }
        fn extra(&self) -> Vec<Entity> {
            self.2.clone()
        }
        fn health(&self) -> DriverHealth {
            DriverHealth::default()
        }
    }

    /// A device instance shows its own slot's fields and its extras in its own group; a
    /// device reading several slots prefixes the slot; extras with unsafe keys are dropped.
    #[test]
    fn device_entities_are_grouped_by_instance() {
        let mut w = World::default();
        w.omnik.power_w = Some(489.0);
        let extra = vec![Entity::sensor("raw", "Ruw", Value::Number(0.489)), Entity::sensor("Raw!", "x", Value::Null)];
        let v = View(vec![Slot::Pv], w.clone(), extra);
        assert_eq!((v.kind(), v.health()), ("static", DriverHealth::default()));
        let es = device_entities(&v);
        let ids: Vec<_> = es.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["pv_garage_power_w", "pv_garage_raw"]);
        assert_eq!(es[0].value, Value::Number(489.0));
        assert_eq!(es[1].group, "pv_garage");
        let two = View(vec![Slot::Pv, Slot::LoadVriezer, Slot::SolarForecast], w, vec![]);
        let ids: Vec<_> = device_entities(&two).into_iter().map(|e| e.id).collect();
        assert_eq!(ids, ["pv_garage_pv_power_w", "pv_garage_load_vriezer_power_w", "pv_garage_load_vriezer_switch_on"]);
    }

    /// A driver's extra with the key of a slot field takes that field's place; its control
    /// stays only while the instance can be written to, and setting it targets the device.
    #[test]
    fn device_extras_can_be_settable() {
        let mut limit = Entity::sensor("ac_limit", "AC-laadlimiet", Value::Number(80.0));
        limit.control = Some(Control::Number { min: Some(50.0), max: Some(100.0), step: 10.0 });
        let v = View(vec![Slot::CarEv3], World::default(), vec![limit]);
        assert!(!v.settable() && v.forecasts().is_empty());
        assert_eq!(v.set("ac_limit", &Value::Number(80.0)).unwrap_err().0, "pv_garage: entities cannot be set");
        let es = device_entities(&v);
        let lim: Vec<_> = es.iter().filter(|e| e.key == "ac_limit").collect();
        assert_eq!(lim.len(), 1, "one entity per key");
        assert_eq!((lim[0].value.clone(), lim[0].control.clone()), (Value::Number(80.0), None));
        assert_eq!(es.iter().position(|e| e.key == "ac_limit"), Some(2), "in the field's place");
        assert_eq!(target(&es, "pv_garage_ac_limit", &Value::Number(70.0)), Err(SetError::NotSettable));
        struct Writable(View);
        impl DeviceView for Writable {
            fn id(&self) -> &str {
                self.0.id()
            }
            fn kind(&self) -> &str {
                self.0.kind()
            }
            fn slots(&self) -> &[Slot] {
                self.0.slots()
            }
            fn world(&self) -> World {
                self.0.world()
            }
            fn extra(&self) -> Vec<Entity> {
                self.0.extra()
            }
            fn health(&self) -> DriverHealth {
                self.0.health()
            }
            fn settable(&self) -> bool {
                true
            }
        }
        let w = Writable(v);
        assert_eq!(w.health(), DriverHealth::default());
        assert_eq!(w.kind(), w.0.kind());
        let es = device_entities(&w);
        assert!(es.iter().any(|e| e.key == "ac_limit" && e.control.is_some()));
        assert_eq!(
            target(&es, "pv_garage_ac_limit", &Value::Number(70.0)),
            Ok(Target::Device { id: "pv_garage".into(), key: "ac_limit".into(), value: Value::Number(70.0) })
        );
    }

    /// Globs and publish rules: later rules win, unset parts are kept.
    #[test]
    fn publish_rules_by_glob() {
        assert!(glob("world_grid_meter_*", "world_grid_meter_power_w"));
        assert!(glob("*_power_w", "pv_garage_power_w"));
        assert!(glob("a*b*c", "aXXbYc") && !glob("a*b*c", "aXXcYb"));
        assert!(glob("exact", "exact") && !glob("exact", "exactly"));
        assert!(glob("*", "") && !glob("x*", ""));
        let views: Vec<Arc<dyn DeviceView>> = vec![Arc::new(View(vec![Slot::Pv], World::default(), vec![]))];
        let rules = vec![
            PublishRule {
                pattern: "world_grid_meter_*".into(),
                enabled: None,
                min_interval_s: Some(5.0),
                deadband: Some(10.0),
            },
            PublishRule {
                pattern: "world_grid_meter_import_kwh".into(),
                enabled: Some(false),
                min_interval_s: None,
                deadband: None,
            },
        ];
        let es = collect(&Inputs {
            world: &World::default(),
            derived: &Derived::default(),
            settings: &Settings::default(),
            settings_editable: false,
            memory: &Memory::default(),
            settable: &|_| false,
            devices: &views,
            rules: &rules,
        });
        let get = |id: &str| es.iter().find(|e| e.id == id).unwrap();
        assert_eq!(
            get("world_grid_meter_power_w").publish,
            Publish { enabled: true, min_interval_s: Some(5.0), deadband: Some(10.0) }
        );
        assert_eq!(
            get("world_grid_meter_import_kwh").publish,
            Publish { enabled: false, min_interval_s: Some(5.0), deadband: Some(10.0) }
        );
        assert_eq!(get("world_battery_inverter_soc").publish, Publish::default());
        assert!(get("pv_garage_power_w").control.is_none());
        let mut ids: Vec<_> = es.iter().map(|e| e.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), es.len(), "ids are unique across groups");
        for bad in [
            PublishRule { pattern: String::new(), enabled: None, min_interval_s: None, deadband: None },
            PublishRule { pattern: "x".into(), enabled: None, min_interval_s: Some(-1.0), deadband: None },
            PublishRule { pattern: "x".into(), enabled: None, min_interval_s: None, deadband: Some(f64::NAN) },
        ] {
            assert!(bad.check().is_err());
        }
        rules[0].check().unwrap();
    }

    /// Change detection: everything at first, then only what changed or appeared.
    #[test]
    fn changes_since_the_last_values() {
        let a = BTreeMap::from([("x".to_string(), Value::Number(1.0)), ("y".to_string(), Value::Null)]);
        assert_eq!(changes(&BTreeMap::new(), &a), a);
        let mut b = a.clone();
        b.insert("y".into(), Value::Bool(true));
        b.insert("z".into(), Value::Null);
        let c = changes(&a, &b);
        assert_eq!(c.keys().collect::<Vec<_>>(), ["y", "z"]);
        let es = derived_entities(&Derived::default());
        assert_eq!(values(&es).len(), es.len());
    }

    /// Every World field HA reads from an entity of its own names that entity (the phase
    /// currents by index); fields HA shows as an attribute name none.
    #[test]
    fn world_entities_name_their_ha_entity() {
        let es = world_entities(&World::default(), &|_| false);
        let ha = |id: &str| es.iter().find(|e| e.id == id).unwrap().ha_entity.clone();
        assert_eq!(ha("world_grid_meter_power_w").as_deref(), Some("sensor.p1_meter_vermogen"));
        assert_eq!(ha("world_grid_meter_phase_2_a").as_deref(), Some("sensor.p1_meter_stroom_fase_2"));
        assert_eq!(
            ha("world_charger_buzz_override_a").as_deref(),
            Some("number.smartevse_1234_charge_current_override")
        );
        assert_eq!(ha("world_charger_buzz_override_max_a"), None, "an attribute of the override");
        assert_eq!(ha("world_price_feed_afname_slots"), None);
        assert_eq!(ha("world_price_feed_backup_now").as_deref(), Some("input_number.prijs_backup_nu"));
        let named = es.iter().filter(|e| e.ha_entity.is_some()).count();
        assert_eq!(named, world_bindings().len() - HA_ATTRIBUTES.len(), "every binding but the attributes");
    }

    /// Derived values name the template sensor or meter that shows them, settings their
    /// helper; the rows that compare an attribute or availability name nothing.
    #[test]
    fn derived_and_settings_entities_name_their_ha_entity() {
        let es = derived_entities(&Derived::default());
        let ha = |id: &str| es.iter().find(|e| e.id == id).unwrap().ha_entity.clone();
        assert_eq!(ha("derived_pv_vermogen_totaal_w").as_deref(), Some("sensor.pv_vermogen_totaal"));
        assert_eq!(ha("derived_capaciteitspiek_risico").as_deref(), Some("binary_sensor.capaciteitspiek_risico"));
        assert_eq!(ha("derived_ev_laadvermogen_smartevse_kw").as_deref(), Some("sensor.ev_laadvermogen_smartevse"));
        assert_eq!(ha("derived_ev_laadvermogen_smartevse_beschikbaar"), None, "availability of the same entity");
        assert_eq!(ha("derived_kwartier_next_reset"), None, "an attribute");
        assert_eq!(ha_of_derived("bestaat_niet"), None);
        let s = settings_entities(&Settings::default(), false);
        assert!(s.iter().all(|e| e.ha_entity.as_deref().is_some_and(|h| h.starts_with("input_"))));
        let z = s.iter().find(|e| e.id == "setting_zonprioriteit").unwrap();
        assert_eq!(z.ha_entity.as_deref(), Some("input_select.zonprioriteit"));
    }

    /// The memory fields are read-only entities: values in their unit, timestamps as such;
    /// each names its HA helper or counter.
    #[test]
    fn memory_fields_are_entities() {
        let m = Memory {
            capaciteit_piekhistoriek: "2026-10:4.100".into(),
            peak_guard_interventies: 7,
            batterij_net_laden: true,
            nachtdekking: 62.0,
            steering_volgende_update: Some(crate::plug::tests::t0()),
            ..Default::default()
        };
        let es = memory_entities(&m);
        assert_eq!(es.len(), memory_fields().len());
        assert!(es.iter().all(|e| e.control.is_none()));
        let get = |id: &str| es.iter().find(|e| e.id == id).unwrap();
        let p = get("memory_capaciteit_piekhistoriek");
        assert_eq!(p.value, Value::Text("2026-10:4.100".into()));
        assert_eq!(p.ha_entity.as_deref(), Some("input_text.capaciteit_piekhistoriek"));
        assert_eq!(get("memory_peak_guard_interventies").value, Value::Number(7.0));
        assert_eq!(get("memory_peak_guard_interventies").ha_entity.as_deref(), Some("counter.peak_guard_interventies"));
        assert_eq!(get("memory_batterij_net_laden").value, Value::Bool(true));
        let n = get("memory_nachtdekking");
        assert_eq!((n.value.clone(), n.unit.as_deref()), (Value::Number(62.0), Some("%")));
        assert_eq!(get("memory_ev_hoger_laadstil_sinds").unit, None);
        let t = get("memory_steering_volgende_update");
        assert_eq!(t.class.as_deref(), Some("timestamp"));
        assert_eq!(t.value, Value::Text("2026-11-07T13:46:00+01:00".into()));
        assert_eq!(get("memory_capaciteitspiek_risico_sinds").ha_entity, None, "not an HA entity");
        // Every helper binding but the start script's timestamp has its entity.
        let bound: Vec<_> =
            memory_bindings().into_iter().map(|b| b.field).filter(|f| !f.starts_with("last_")).collect();
        assert!(bound.iter().all(|f| es.iter().any(|e| e.key == *f)), "{bound:?}");
        assert_eq!(target(&es, "memory_nachtdekking", &Value::Number(1.0)), Err(SetError::NotSettable));
    }
}
