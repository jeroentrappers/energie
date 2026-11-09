//! Device instances and role bindings: the `[[device]]` tables, resolved into who fills each
//! World section and who carries out each device's commands.
//!
//! ```toml
//! [[device]]
//! id = "p1"                 # [a-z0-9_]; the entity group and the driver's name (`driver:p1`)
//! kind = "homewizard"       # a registered kind (`crate::registry`)
//! role = "grid_meter"       # or a list; `charger:buzz` style for the named slots
//! address = "192.168.1.30" # ... the kind's own keys
//! ```
//!
//! Common keys: `role` (the slots it reads), `control` (the slots whose commands it carries
//! out; needs `read_only = false`), `read_only` (default `true`), `poll_ms` (default: the
//! kind's), `grace_polls` (default 3), `name`. A bare role name is enough when the kind fills
//! one slot of it (`role = "charger"` for `smartevse_mqtt`). The bridges `ha` and `sim` also take
//! `"*"`: every slot no other device reads (in `role`) or controls (in `control`).
//!
//! Every World section always has a reader: unbound sections fall back to Home Assistant
//! (the house model in simulate mode), as the old `[sources] default` did, or, without an HA
//! bridge (no `[ha]`, or `[ha] import_only`: standalone), to [`Provider::Unavailable`]: the
//! section stays unavailable, as HA's entities would without their integration. Several `pv`
//! readers are summed; any other slot has at most one reader. A device without a controller
//! gets the log actuator, except in simulate mode, where the house model takes it.

use std::collections::{BTreeMap, BTreeSet};

use energie_drivers::entity::is_topic_safe;
use energie_drivers::plugin::Slot;
use serde::{Deserialize, Deserializer};

use crate::config::Mode;
use crate::error::{Error, Result};
use crate::plug::{Device, Section};
use crate::registry::{Kind, Registry};

/// Entity groups the app uses itself; no device may take these ids.
pub const RESERVED: [&str; 5] = ["world", "derived", "setting", "memory", "control"];

fn one_or_many<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// One `[[device]]` table.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct DeviceConfig {
    pub id: String,
    pub kind: String,
    #[serde(default, deserialize_with = "one_or_many")]
    pub role: Vec<String>,
    #[serde(default, deserialize_with = "one_or_many")]
    pub control: Vec<String>,
    /// A display name for the dashboard.
    pub name: Option<String>,
    pub poll_ms: Option<u64>,
    pub grace_polls: Option<u32>,
    pub read_only: Option<bool>,
    /// The kind's own keys.
    #[serde(flatten)]
    pub table: toml::Table,
    /// How errors name it: `device <id>`, or `devices.<name>` for a translated legacy table.
    #[serde(skip)]
    pub label: Option<String>,
}

impl DeviceConfig {
    fn label(&self) -> String {
        self.label.clone().unwrap_or_else(|| format!("device {}", self.id))
    }
}

/// Who fills a section or carries out a device's commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    Ha,
    Sim,
    /// A native device instance, by id.
    Device(String),
    /// Nobody (standalone, no device for the slot): every field unavailable.
    Unavailable,
}

/// A native device instance, checked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Instance {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub name: Option<String>,
    pub reads: Vec<Slot>,
    pub controls: Vec<Slot>,
    pub poll_ms: u64,
    pub grace_polls: u32,
    pub read_only: bool,
    /// The kind's own table, as JSON for its factory.
    pub table: serde_json::Value,
}

/// The resolved bindings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// The native instances, in config order.
    pub devices: Vec<Instance>,
    /// Who fills each slot: every World section has at least one, `pv` may have several,
    /// `solar_forecast` only when a device reads it.
    pub readers: BTreeMap<Slot, Vec<Provider>>,
    /// Who carries out each device's commands; absent: the log actuator.
    pub controllers: BTreeMap<Device, Provider>,
}

/// Parse one `role`/`control` entry against the slots the kind can fill.
fn parse_slot(spec: &str, can: &[Slot]) -> std::result::Result<Slot, String> {
    let fills = || can.iter().map(|s| s.name()).collect::<Vec<_>>().join(", ");
    let slot = match Slot::parse(spec) {
        Some(s) => s,
        None => {
            let named: Vec<Slot> = can.iter().copied().filter(|s| s.role().name() == spec).collect();
            match named.as_slice() {
                [one] => *one,
                [] if Slot::ALL.iter().any(|s| s.role().name() == spec) => {
                    return Err(format!("this kind cannot fill {spec} (it fills: {})", fills()));
                }
                [] => {
                    let all: Vec<_> = Slot::ALL.iter().map(|s| s.name()).collect();
                    return Err(format!("unknown role {spec:?} (roles: {})", all.join(", ")));
                }
                many => {
                    let names: Vec<_> = many.iter().map(|s| s.name()).collect();
                    return Err(format!("role {spec:?} needs a slot: {}", names.join(" or ")));
                }
            }
        }
    };
    if can.contains(&slot) { Ok(slot) } else { Err(format!("this kind cannot fill {spec} (it fills: {})", fills())) }
}

/// A `role` or `control` list: the slots, and whether it had `"*"`.
fn specs(list: &[String], can: &[Slot], bridge: bool, label: &str, what: &str) -> Result<(Vec<Slot>, bool)> {
    let mut out = Vec::new();
    let mut star = false;
    for spec in list {
        if spec == "*" {
            if !bridge {
                return Err(Error::new(format!("{label}: {what} = \"*\" is only for kind ha or sim")));
            }
            star = true;
            continue;
        }
        let slot = parse_slot(spec, can).map_err(|e| Error::new(format!("{label}: {what}: {e}")))?;
        if !out.contains(&slot) {
            out.push(slot);
        }
    }
    Ok((out, star))
}

impl Plan {
    /// Check `devices` against `reg` and resolve the bindings for `mode`. `strict` (the config
    /// check) refuses a `control` on a `read_only` device; otherwise the binding stays and the
    /// device simply gets no actuator (the start-up then fails on the missing actuator).
    pub fn resolve(mode: Mode, devices: &[DeviceConfig], reg: &Registry, strict: bool) -> Result<Plan> {
        Plan::resolve_with(mode, devices, reg, strict, true)
    }

    /// [`Plan::resolve`], with `ha` saying whether an HA bridge exists to fall back to; without
    /// one, an unbound section (outside simulate mode) is [`Provider::Unavailable`].
    pub fn resolve_with(mode: Mode, devices: &[DeviceConfig], reg: &Registry, strict: bool, ha: bool) -> Result<Plan> {
        let mut plan = Plan::default();
        let mut ids = BTreeSet::new();
        let mut readers: BTreeMap<Slot, Vec<(Provider, String)>> = BTreeMap::new();
        let mut controllers: BTreeMap<Device, (Provider, String)> = BTreeMap::new();
        let mut star_reader: Option<(Provider, String)> = None;
        let mut star_controller: Option<(Provider, String)> = None;
        for c in devices {
            let label = c.label();
            if !is_topic_safe(&c.id) {
                return Err(Error::new(format!("{label}: the id must be [a-z0-9_]+")));
            }
            if RESERVED.contains(&c.id.as_str()) {
                return Err(Error::new(format!("{label}: the id {:?} is reserved", c.id)));
            }
            if !ids.insert(c.id.clone()) {
                return Err(Error::new(format!("{label}: declared twice")));
            }
            let kind = reg.get(&c.kind).ok_or_else(|| {
                Error::new(format!("{label}: unknown kind {:?} (known: {})", c.kind, reg.names().join(", ")))
            })?;
            let provider = match kind {
                Kind::Ha => Provider::Ha,
                Kind::Sim => Provider::Sim,
                Kind::Native(_) => Provider::Device(c.id.clone()),
            };
            let can = kind.slots();
            let (reads, read_star) = specs(&c.role, &can, kind.is_bridge(), &label, "role")?;
            let (controls, control_star) = specs(&c.control, &can, kind.is_bridge(), &label, "control")?;
            let controlling = !controls.is_empty() || control_star;
            if let Some(s) = controls.iter().find(|s| Device::of_slot(**s).is_none()) {
                return Err(Error::new(format!("{label}: control: {} takes no commands", s.name())));
            }
            if controlling && !kind.writable() {
                return Err(Error::new(format!("{label}: control: kind {} takes no commands", c.kind)));
            }
            let read_only = c.read_only.unwrap_or(true);
            if strict && controlling && read_only {
                return Err(Error::new(format!(
                    "{label}: control needs read_only = false (the default is true) to let energie write to it"
                )));
            }
            if reads.is_empty() && !read_star && !controlling {
                return Err(Error::new(format!("{label}: no role; set role = \"<slot>\" (or control = ...)")));
            }
            if c.poll_ms == Some(0) {
                return Err(Error::new(format!("{label}: poll_ms must be > 0")));
            }
            let table = serde_json::to_value(&c.table)?;
            match kind {
                Kind::Native(f) => f.check(&table).map_err(|e| Error::new(format!("{label}: {e}")))?,
                _ => {
                    if let Some(k) = c.table.keys().next() {
                        return Err(Error::new(format!("{label}: unknown field `{k}` for kind {}", c.kind)));
                    }
                }
            }
            for s in &reads {
                let r = readers.entry(*s).or_default();
                if let Some((_, other)) = r.first().filter(|_| !s.summed()) {
                    return Err(Error::new(format!("{} is read by both {other} and {label}", s.name())));
                }
                r.push((provider.clone(), label.clone()));
            }
            for s in &controls {
                let d = Device::of_slot(*s).expect("checked above");
                if let Some((_, other)) = controllers.get(&d) {
                    return Err(Error::new(format!("{} is controlled by both {other} and {label}", s.name())));
                }
                controllers.insert(d, (provider.clone(), label.clone()));
            }
            for (star, set, what) in
                [(read_star, &mut star_reader, "role"), (control_star, &mut star_controller, "control")]
            {
                if !star {
                    continue;
                }
                if let Some((_, other)) = set {
                    return Err(Error::new(format!("{what} = \"*\" on both {other} and {label}")));
                }
                *set = Some((provider.clone(), label.clone()));
            }
            if let Kind::Native(f) = kind {
                plan.devices.push(Instance {
                    id: c.id.clone(),
                    kind: c.kind.clone(),
                    label,
                    name: c.name.clone(),
                    reads,
                    controls,
                    poll_ms: c.poll_ms.unwrap_or(f.poll_ms()),
                    grace_polls: c.grace_polls.unwrap_or(3),
                    read_only,
                    table,
                });
            }
        }
        let fallback = match mode {
            Mode::Simulate => Provider::Sim,
            _ if ha => Provider::Ha,
            _ => Provider::Unavailable,
        };
        for (slot, r) in readers {
            plan.readers.insert(slot, r.into_iter().map(|(p, _)| p).collect());
        }
        for s in Section::ALL {
            let r = plan.readers.entry(s.slot()).or_default();
            if r.is_empty() {
                r.push(star_reader.clone().map_or(fallback.clone(), |(p, _)| p));
            }
        }
        let default_controller = star_controller.map(|(p, _)| p).or((mode == Mode::Simulate).then_some(Provider::Sim));
        for d in Device::ALL {
            let p = controllers.remove(&d).map(|(p, _)| p).or(default_controller.clone());
            if let Some(p) = p {
                plan.controllers.insert(d, p);
            }
        }
        Ok(plan)
    }

    /// The World sections nobody reads (standalone): they stay unavailable.
    pub fn unread(&self) -> Vec<Section> {
        Section::ALL.into_iter().filter(|s| self.readers[&s.slot()] == [Provider::Unavailable]).collect()
    }

    /// Whether `p` reads anything, or (outside shadow mode) carries out anything.
    pub fn uses(&self, p: &Provider, mode: Mode) -> bool {
        self.readers.values().flatten().any(|x| x == p)
            || (mode != Mode::Shadow && self.controllers.values().any(|x| x == p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devices(toml_text: &str) -> Vec<DeviceConfig> {
        #[derive(Deserialize)]
        struct F {
            device: Vec<DeviceConfig>,
        }
        toml::from_str::<F>(toml_text).unwrap().device
    }

    fn plan(mode: Mode, t: &str) -> Result<Plan> {
        Plan::resolve(mode, &devices(t), Registry::builtin(), true)
    }

    const HW: &str = "[[device]]\nid = \"p1\"\nkind = \"homewizard\"\nrole = \"grid_meter\"\naddress = \"192.168.1.30\"\nserial = \"s\"\ntoken_file = \"t\"\n";

    /// A native grid meter fills its slot; every other section falls back to HA (simulate: the
    /// model, which also carries out every command); nothing is controlled by default.
    #[test]
    fn unbound_sections_fall_back_like_the_old_default() {
        let p = plan(Mode::Shadow, HW).unwrap();
        assert_eq!(p.readers[&Slot::GridMeter], [Provider::Device("p1".into())]);
        assert_eq!(p.readers[&Slot::CarEv3], [Provider::Ha]);
        assert!(!p.readers.contains_key(&Slot::SolarForecast));
        assert!(p.controllers.is_empty());
        let i = &p.devices[0];
        assert_eq!((i.poll_ms, i.grace_polls, i.read_only, i.label.as_str()), (1000, 3, true, "device p1"));
        assert_eq!(i.table["serial"], "s");
        let sim = plan(Mode::Simulate, HW).unwrap();
        assert_eq!(sim.readers[&Slot::PriceFeed], [Provider::Sim]);
        assert_eq!(sim.controllers[&Device::Cable], Provider::Sim);
        assert!(sim.uses(&Provider::Sim, Mode::Simulate));
        assert!(!sim.uses(&Provider::Ha, Mode::Simulate));
    }

    /// LOGIC: several PV readers are kept (and summed at start-up); any other slot refuses a
    /// second reader.
    #[test]
    fn pv_takes_several_readers_other_slots_one() {
        let pv = "[[device]]\nid = \"omnik\"\nkind = \"omnik\"\nrole = \"pv\"\nbase_url = \"http://x\"\n\
                  [[device]]\nid = \"garage\"\nkind = \"http_json\"\nrole = \"pv\"\nurl = \"http://y\"\npointer = \"/p\"\n\
                  [[device]]\nid = \"ha_pv\"\nkind = \"ha\"\nrole = \"pv\"\n";
        let p = plan(Mode::Control, pv).unwrap();
        assert_eq!(
            p.readers[&Slot::Pv],
            [Provider::Device("omnik".into()), Provider::Device("garage".into()), Provider::Ha]
        );
        let two = format!("{HW}{}", HW.replace("\"p1\"", "\"p1b\""));
        assert_eq!(plan(Mode::Control, &two).unwrap_err().0, "grid_meter is read by both device p1 and device p1b");
    }

    /// LOGIC: without an HA bridge (standalone) an unbound section has no reader and stays
    /// unavailable; simulate mode still falls back to the model.
    #[test]
    fn standalone_sections_without_a_device_are_unavailable() {
        let p = Plan::resolve_with(Mode::Control, &devices(HW), Registry::builtin(), true, false).unwrap();
        assert_eq!(p.readers[&Slot::GridMeter], [Provider::Device("p1".into())]);
        assert_eq!(p.readers[&Slot::CarEv3], [Provider::Unavailable]);
        assert!(!p.uses(&Provider::Ha, Mode::Control));
        assert_eq!(p.unread().len(), Section::ALL.len() - 1);
        assert!(!p.unread().contains(&Section::P1));
        let sim = Plan::resolve_with(Mode::Simulate, &devices(HW), Registry::builtin(), true, false).unwrap();
        assert!(sim.unread().is_empty());
        assert!(plan(Mode::Control, HW).unwrap().unread().is_empty(), "with HA nothing is unread");
    }

    /// Bridges take `*` for the rest; a native device names its slots; a bare role resolves to
    /// the kind's only slot of it.
    #[test]
    fn stars_bare_roles_and_controls() {
        let t = "[[device]]\nid = \"ha\"\nkind = \"ha\"\nrole = \"*\"\ncontrol = [\"*\"]\nread_only = false\n\
                 [[device]]\nid = \"evse\"\nkind = \"smartevse_mqtt\"\nrole = \"charger\"\ncontrol = \"charger\"\nread_only = false\nhost = \"h\"\n";
        let p = plan(Mode::Control, t).unwrap();
        assert_eq!(p.readers[&Slot::ChargerBuzz], [Provider::Device("evse".into())]);
        assert_eq!(p.readers[&Slot::GridMeter], [Provider::Ha]);
        assert_eq!(p.controllers[&Device::Evse], Provider::Device("evse".into()));
        assert_eq!(p.controllers[&Device::Solis], Provider::Ha);
        assert!(p.uses(&Provider::Ha, Mode::Control));
        let only_control = "[[device]]\nid = \"sim\"\nkind = \"sim\"\ncontrol = [\"battery_inverter\", \"battery_inverter\"]\nread_only = false\n";
        let p = plan(Mode::Control, only_control).unwrap();
        assert_eq!(p.controllers.len(), 1);
        assert!(p.uses(&Provider::Sim, Mode::Control));
        assert!(!p.uses(&Provider::Sim, Mode::Shadow), "shadow never actuates");
        // Lenient: a control on a read-only device stays bound.
        let ro = "[[device]]\nid = \"evse\"\nkind = \"smartevse_mqtt\"\nrole = \"charger\"\ncontrol = \"charger\"\nhost = \"h\"\n";
        let p = Plan::resolve(Mode::Control, &devices(ro), Registry::builtin(), false).unwrap();
        assert_eq!(p.controllers[&Device::Evse], Provider::Device("evse".into()));
        assert!(p.devices[0].read_only);
    }

    /// Mistakes are refused with a message that names the device and says what to fix.
    #[test]
    fn bad_devices_are_refused() {
        let err = |t: &str| plan(Mode::Control, t).unwrap_err().0;
        let dev = |extra: &str| format!("[[device]]\nid = \"x\"\n{extra}\n");
        let cases = [
            (
                dev("kind = \"wasmachine\"\nrole = \"pv\""),
                "device x: unknown kind \"wasmachine\" (known: cupra_data_act, energy_charts, fluvius,",
            ),
            (
                dev("kind = \"omnik\"\nrole = \"pv\"\nbase_url = \"u\"\nbase_ulr = \"u\""),
                "device x: config: unknown field `base_ulr`",
            ),
            (dev("kind = \"omnik\"\nrole = \"pv\""), "device x: config: missing field `base_url`"),
            (
                dev("kind = \"omnik\"\nrole = \"grid_meter\"\nbase_url = \"u\""),
                "device x: role: this kind cannot fill grid_meter (it fills: pv)",
            ),
            (
                dev("kind = \"omnik\"\nrole = \"charger\"\nbase_url = \"u\""),
                "device x: role: this kind cannot fill charger",
            ),
            (
                dev("kind = \"omnik\"\nrole = \"zonnepaneel\"\nbase_url = \"u\""),
                "device x: role: unknown role \"zonnepaneel\" (roles: grid_meter,",
            ),
            (
                dev("kind = \"ha\"\nrole = \"charger\""),
                "device x: role: role \"charger\" needs a slot: charger:buzz or charger:ev3",
            ),
            (
                dev("kind = \"omnik\"\nrole = \"*\"\nbase_url = \"u\""),
                "device x: role = \"*\" is only for kind ha or sim",
            ),
            (dev("kind = \"omnik\"\nbase_url = \"u\""), "device x: no role"),
            (
                dev("kind = \"omnik\"\ncontrol = \"pv\"\nbase_url = \"u\"\nread_only = false"),
                "device x: control: pv takes no commands",
            ),
            (
                dev("kind = \"static\"\ncontrol = \"price_feed\"\nread_only = false"),
                "device x: control: kind static takes no commands",
            ),
            (
                dev("kind = \"smartevse_mqtt\"\ncontrol = \"charger\"\nhost = \"h\""),
                "device x: control needs read_only = false",
            ),
            (dev("kind = \"omnik\"\nrole = \"pv\"\nbase_url = \"u\"\npoll_ms = 0"), "device x: poll_ms must be > 0"),
            (dev("kind = \"ha\"\nrole = \"pv\"\nurl = \"u\""), "device x: unknown field `url` for kind ha"),
            ("[[device]]\nid = \"P1\"\nkind = \"ha\"\nrole = \"pv\"".into(), "device P1: the id must be [a-z0-9_]+"),
            (
                "[[device]]\nid = \"world\"\nkind = \"ha\"\nrole = \"pv\"".into(),
                "device world: the id \"world\" is reserved",
            ),
            (format!("{HW}{}", HW.replace("grid_meter", "pv")), "device p1: declared twice"),
            (
                format!(
                    "{}{}",
                    dev("kind = \"ha\"\nrole = \"*\""),
                    dev("kind = \"sim\"\nrole = \"*\"").replace("\"x\"", "\"y\"")
                ),
                "role = \"*\" on both device x and device y",
            ),
            (
                format!(
                    "{}{}",
                    dev("kind = \"ha\"\ncontrol = \"price_feed\"\nread_only = false"),
                    dev("kind = \"sim\"\ncontrol = \"price_feed\"\nread_only = false").replace("\"x\"", "\"y\"")
                ),
                "price_feed is controlled by both device x and device y",
            ),
        ];
        for (t, want) in cases {
            let got = err(&t);
            assert!(got.starts_with(want), "{t}\n  got:  {got}\n  want: {want}");
        }
        let bad_role: toml::Table = toml::from_str("[[device]]\nid = \"x\"\nkind = \"ha\"\nrole = 5\n").unwrap();
        assert!(bad_role["device"].clone().try_into::<Vec<DeviceConfig>>().is_err(), "a role is a string or a list");
    }
}
