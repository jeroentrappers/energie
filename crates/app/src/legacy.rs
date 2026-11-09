//! The deprecated `[sources]` / `[actuators]` / `[devices.<name>]` tables, translated into
//! `[[device]]` instances with the same behaviour:
//!
//! - every section's source (`ha`, `sim`, `driver`; unset: `default`, else `ha`, `sim` in
//!   simulate mode) becomes a `role` of the `ha` bridge, the `sim` bridge or the native device;
//! - every device's actuator (unset: `default`, else `log`, `sim` in simulate mode) becomes a
//!   `control` of the same; shadow mode ignores `[actuators]`, as it always did;
//! - `[devices.homewizard|solis|omnik|smartevse|cable|prices]` become the instances `homewizard`,
//!   `solis`, `omnik`, `smartevse`, `cable`, `prices` of the kinds `homewizard`, `solis_modbus`,
//!   `omnik`, `smartevse_mqtt`, `tuya_cable`, `energy_charts` (the driver names on the dashboard
//!   stay `driver:<name>`), and only when a selection uses them.
//!
//! [`to_toml`] prints the result, which `--check-config` shows with the deprecation warning.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::config::{ActuatorKind, Config, Mode, SourceKind};
use crate::error::{Error, Result};
use crate::plan::DeviceConfig;
use crate::plug::{Device, Section};
use crate::registry::Registry;

/// Legacy name, kind, the section it read, the device it carried out.
const LEGACY: [(&str, &str, Section, Option<Device>); 6] = [
    ("homewizard", "homewizard", Section::P1, None),
    ("solis", "solis_modbus", Section::Solis, Some(Device::Solis)),
    ("omnik", "omnik", Section::Omnik, None),
    ("smartevse", "smartevse_mqtt", Section::Evse, Some(Device::Evse)),
    ("cable", "tuya_cable", Section::Cable, Some(Device::Cable)),
    ("prices", "energy_charts", Section::Prices, Some(Device::Prices)),
];

fn lower(x: impl std::fmt::Debug) -> String {
    format!("{x:?}").to_ascii_lowercase()
}

/// Whether the file uses any of the legacy tables.
pub fn present(cfg: &Config) -> bool {
    cfg.sources != Default::default() || cfg.actuators != Default::default() || !cfg.devices.is_empty()
}

/// The source of one section, as the legacy tables select it.
fn source(cfg: &Config, s: Section) -> SourceKind {
    let fallback = if cfg.mode == Mode::Simulate { SourceKind::Sim } else { SourceKind::Ha };
    cfg.sources.per_section.get(&s).copied().or(cfg.sources.default).unwrap_or(fallback)
}

/// The actuator of one device, as the legacy tables select it.
fn actuator(cfg: &Config, d: Device) -> ActuatorKind {
    let fallback = if cfg.mode == Mode::Simulate { ActuatorKind::Sim } else { ActuatorKind::Log };
    match cfg.mode {
        Mode::Shadow => ActuatorKind::Log,
        _ => cfg.actuators.per_device.get(&d).copied().or(cfg.actuators.default).unwrap_or(fallback),
    }
}

/// The keys every legacy device table had.
#[derive(Deserialize)]
struct Common {
    poll_ms: Option<u64>,
    grace_polls: Option<u32>,
    read_only: Option<bool>,
}

/// One `[devices.<name>]` table as an instance (without roles yet).
fn native(name: &str, kind: &str, t: &toml::Table) -> Result<DeviceConfig> {
    let label = format!("devices.{name}");
    let c: Common = toml::Value::Table(t.clone()).try_into().map_err(|e| Error::new(format!("{label}: {e}")))?;
    let writable = Registry::builtin().factory(kind).expect("legacy kinds are built in").writable();
    let mut table = t.clone();
    table.remove("poll_ms");
    table.remove("grace_polls");
    if writable {
        // The read-only kinds never had the key: left in, it is refused as unknown.
        table.remove("read_only");
    }
    Ok(DeviceConfig {
        id: name.to_string(),
        kind: kind.to_string(),
        poll_ms: c.poll_ms,
        grace_polls: c.grace_polls,
        read_only: c.read_only,
        table,
        label: Some(label),
        ..Default::default()
    })
}

/// Check every `[devices.<name>]` table, used or not (as the old config check did).
fn check_tables(cfg: &Config) -> Result<()> {
    for (name, t) in &cfg.devices {
        let (_, kind, ..) = LEGACY.iter().find(|(n, ..)| n == name).ok_or_else(|| {
            let known: Vec<_> = LEGACY.iter().map(|(n, ..)| *n).collect();
            Error::new(format!("[devices.{name}]: unknown device (the legacy tables are {})", known.join(", ")))
        })?;
        let d = native(name, kind, t)?;
        if d.poll_ms == Some(0) {
            return Err(Error::new(format!("devices.{name}.poll_ms must be > 0")));
        }
        let table = serde_json::to_value(&d.table)?;
        let f = Registry::builtin().factory(kind).expect("legacy kinds are built in");
        f.check(&table).map_err(|e| Error::new(format!("devices.{name}: {e}")))?;
    }
    Ok(())
}

/// The instance of legacy device `i`, made on first use (the caller checked its table exists).
fn entry<'a>(natives: &'a mut BTreeMap<usize, DeviceConfig>, cfg: &Config, i: usize) -> Result<&'a mut DeviceConfig> {
    let (name, kind, ..) = LEGACY[i];
    if let std::collections::btree_map::Entry::Vacant(v) = natives.entry(i) {
        v.insert(native(name, kind, &cfg.devices[name])?);
    }
    Ok(natives.get_mut(&i).expect("inserted"))
}

fn bridge(kind: &str) -> DeviceConfig {
    DeviceConfig { id: kind.to_string(), kind: kind.to_string(), ..Default::default() }
}

/// The `[[device]]` instances the legacy tables stand for. `strict` refuses a `driver` actuator
/// on a read-only device, as the old config check did.
pub fn translate(cfg: &Config, strict: bool) -> Result<Vec<DeviceConfig>> {
    check_tables(cfg)?;
    let mut ha = bridge("ha");
    let mut sim = bridge("sim");
    let mut natives: BTreeMap<usize, DeviceConfig> = BTreeMap::new();
    for s in Section::ALL {
        let slot = s.slot().name().to_string();
        match source(cfg, s) {
            SourceKind::Ha => ha.role.push(slot),
            SourceKind::Sim => sim.role.push(slot),
            SourceKind::Driver => {
                let i = LEGACY.iter().position(|l| l.2 == s).ok_or_else(|| {
                    Error::new(format!("sources.{}: there is no native driver for this section", lower(s)))
                })?;
                if !cfg.devices.contains_key(LEGACY[i].0) {
                    return Err(Error::new(format!(
                        "sources.{} = \"driver\" needs [devices.{}]",
                        lower(s),
                        LEGACY[i].0
                    )));
                }
                entry(&mut natives, cfg, i)?.role.push(slot);
            }
        }
    }
    for d in Device::ALL {
        let slot = d.slot().name().to_string();
        match actuator(cfg, d) {
            ActuatorKind::Log => {}
            ActuatorKind::Ha => ha.control.push(slot),
            ActuatorKind::Sim => sim.control.push(slot),
            ActuatorKind::Driver => {
                let i = LEGACY.iter().position(|l| l.3 == Some(d)).ok_or_else(|| {
                    Error::new(format!("actuators.{}: there is no native driver for this device", lower(d)))
                })?;
                let name = LEGACY[i].0;
                if !cfg.devices.contains_key(name) {
                    return Err(Error::new(format!("actuators.{} = \"driver\" needs [devices.{name}]", lower(d))));
                }
                let n = entry(&mut natives, cfg, i)?;
                if strict && n.read_only != Some(false) {
                    return Err(Error::new(format!(
                        "actuators.{} = \"driver\" but devices.{name}.read_only is true (the default); set read_only = false to let energie write to it",
                        lower(d)
                    )));
                }
                n.control.push(slot);
            }
        }
    }
    let mut out = Vec::new();
    for mut b in [ha, sim] {
        if !b.control.is_empty() {
            b.read_only = Some(false);
        }
        if !b.role.is_empty() || !b.control.is_empty() {
            out.push(b);
        }
    }
    out.extend(natives.into_values());
    Ok(out)
}

fn list(v: &[String]) -> String {
    match v {
        [one] => toml::Value::String(one.clone()).to_string(),
        many => toml::Value::Array(many.iter().cloned().map(toml::Value::String).collect()).to_string(),
    }
}

/// The instances as `[[device]]` TOML.
pub fn to_toml(devices: &[DeviceConfig]) -> String {
    let mut out = String::new();
    for d in devices {
        out.push_str(&format!("[[device]]\nid = {:?}\nkind = {:?}\n", d.id, d.kind));
        if !d.role.is_empty() {
            out.push_str(&format!("role = {}\n", list(&d.role)));
        }
        if !d.control.is_empty() {
            out.push_str(&format!("control = {}\n", list(&d.control)));
        }
        if let Some(r) = d.read_only {
            out.push_str(&format!("read_only = {r}\n"));
        }
        if let Some(p) = d.poll_ms {
            out.push_str(&format!("poll_ms = {p}\n"));
        }
        if let Some(g) = d.grace_polls {
            out.push_str(&format!("grace_polls = {g}\n"));
        }
        for (k, v) in &d.table {
            out.push_str(&format!("{k} = {v}\n"));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Plan, Provider};
    use energie_drivers::plugin::Slot;

    fn cfg(t: &str) -> Config {
        Config::parse(t).unwrap()
    }

    /// The deployed shadow config (everything from HA, actuators `log`) becomes one `ha`
    /// instance reading every slot; shadow ignores `[actuators]`.
    #[test]
    fn deployed_shadow_config_reads_everything_from_ha() {
        let c = cfg(
            "mode = \"shadow\"\n[ha]\nurl = \"http://h:8123\"\ntoken_file = \"t\"\n[sources]\ndefault = \"ha\"\n[actuators]\ndefault = \"ha\"\n",
        );
        assert!(present(&c));
        let d = translate(&c, true).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].id.as_str(), d[0].role.len(), d[0].control.len()), ("ha", 11, 0));
        assert_eq!(
            to_toml(&d[..1]).lines().take(3).collect::<Vec<_>>(),
            ["[[device]]", "id = \"ha\"", "kind = \"ha\""]
        );
    }

    /// A mixed control config: a native meter, a native writable charger, the rest from HA
    /// with HA service calls; the translation reads and writes exactly as the old selection.
    #[test]
    fn mixed_selection_translates_to_the_same_bindings() {
        let c = cfg(
            "mode = \"control\"\n[ha]\nurl = \"http://h:8123\"\ntoken_file = \"t\"\n[sources]\np1 = \"driver\"\nevse = \"driver\"\n\
             [actuators]\ndefault = \"ha\"\nevse = \"driver\"\nprices = \"log\"\n\
             [devices.homewizard]\naddress = \"192.168.1.30\"\nserial = \"s\"\ntoken_file = \"t\"\npoll_ms = 500\n\
             [devices.smartevse]\nhost = \"192.168.1.10\"\nread_only = false\ngrace_polls = 1\n",
        );
        let d = translate(&c, true).unwrap();
        let ids: Vec<_> = d.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["ha", "homewizard", "smartevse"]);
        let text = to_toml(&d);
        assert!(
            text.contains("control = [\"charger:ev3\", \"battery_inverter\", \"load:airco\"]\nread_only = false\n"),
            "{text}"
        );
        assert!(text.contains("id = \"homewizard\"\nkind = \"homewizard\"\nrole = \"grid_meter\"\npoll_ms = 500\naddress = \"192.168.1.30\""), "{text}");
        assert!(text.contains("role = \"charger:buzz\"\ncontrol = \"charger:buzz\"\nread_only = false\ngrace_polls = 1\nhost = \"192.168.1.10\""), "{text}");
        let p = Plan::resolve(c.mode, &d, Registry::builtin(), true).unwrap();
        assert_eq!(p.readers[&Slot::GridMeter], [Provider::Device("homewizard".into())]);
        assert_eq!(p.readers[&Slot::PriceFeed], [Provider::Ha]);
        assert_eq!(p.controllers[&Device::Evse], Provider::Device("smartevse".into()));
        assert_eq!(p.controllers[&Device::Solis], Provider::Ha);
        assert!(!p.controllers.contains_key(&Device::Prices));
        assert_eq!(p.devices[0].label, "devices.homewizard");
        // The parsed `[[device]]` form of the printed TOML resolves to the same plan.
        let reparsed =
            Config::parse(&format!("mode = \"control\"\n[ha]\nurl = \"http://h:8123\"\ntoken_file = \"t\"\n{text}"))
                .unwrap();
        let mut again = reparsed.plan().unwrap();
        for i in &mut again.devices {
            i.label = format!("devices.{}", i.id);
        }
        assert_eq!(again, p);
    }

    /// Simulate defaults to the house model for reads and writes; unused tables are checked but
    /// not instantiated.
    #[test]
    fn simulate_defaults_and_unused_tables() {
        let c = cfg("mode = \"simulate\"\n[sources]\nomnik = \"sim\"\n[devices.omnik]\nbase_url = \"http://x\"\n");
        let d = translate(&c, true).unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].kind.as_str(), d[0].control.len(), d[0].read_only), ("sim", 5, Some(false)));
        let bad = Config {
            devices: BTreeMap::from([("omnik".to_string(), toml::from_str("poll_ms = \"vaak\"").unwrap())]),
            ..c
        };
        assert!(translate(&bad, true).unwrap_err().0.starts_with("devices.omnik: "));
    }

    /// A typical shadow-mode config: HA for everything but
    /// the P1 meter, the Omnik, the SmartEVSE and the prices, which come from native drivers.
    /// After the upgrade it starts the same instances under the same driver names.
    const DEPLOYED: &str = "mode = \"shadow\"\nlisten = \"0.0.0.0:8080\"\nstate_dir = \"/var/lib/energie\"\n\
        [ha]\nurl = \"http://192.168.1.10:8123\"\ntoken_file = \"/run/credentials/energie.service/ha-token\"\nstart_wait_s = 60\n\
        [sources]\ndefault = \"ha\"\np1 = \"driver\"\nomnik = \"driver\"\nevse = \"driver\"\nprices = \"driver\"\n\
        [actuators]\ndefault = \"log\"\n\
        [mqtt]\nhost = \"192.168.1.10\"\nusername = \"homeassistant\"\npassword_file = \"/run/credentials/energie.service/mqtt-password\"\nclient_id = \"energie-shadow-2\"\n\
        [devices.homewizard]\naddress = \"192.168.1.30\"\nserial = \"aabbccddeeff\"\ntoken_file = \"/run/credentials/energie.service/homewizard-token\"\npoll_ms = 1000\n\
        [devices.omnik]\nbase_url = \"http://192.168.1.40\"\npoll_ms = 4000\n\
        [devices.smartevse]\nhost = \"192.168.1.10\"\nport = 1883\nprefix = \"SmartEVSE/1234\"\nusername = \"homeassistant\"\npassword_file = \"/run/credentials/energie.service/mqtt-password\"\nclient_id = \"energie-smartevse-2\"\npoll_ms = 1000\nread_only = true\n\
        [devices.prices]\nentsoe_token_file = \"/run/credentials/energie.service/entsoe-token\"\npoll_ms = 10000\nread_only = true\n";

    #[test]
    fn the_deployed_shadow_config_keeps_working() {
        let c = cfg(DEPLOYED);
        assert_eq!(c.warnings().len(), 1);
        let p = c.plan().unwrap();
        let ids: Vec<_> = p.devices.iter().map(|d| (d.id.as_str(), d.kind.as_str(), d.poll_ms)).collect();
        assert_eq!(
            ids,
            [
                ("homewizard", "homewizard", 1000),
                ("omnik", "omnik", 4000),
                ("smartevse", "smartevse_mqtt", 1000),
                ("prices", "energy_charts", 10_000)
            ]
        );
        assert!(p.devices.iter().all(|d| d.controls.is_empty() && d.read_only));
        assert_eq!(p.readers[&Slot::GridMeter], [Provider::Device("homewizard".into())]);
        assert_eq!(p.readers[&Slot::BatteryInverter], [Provider::Ha]);
        assert_eq!(p.readers[&Slot::Pv], [Provider::Device("omnik".into())]);
        assert!(p.controllers.is_empty());
        assert!(c.needs_ha() && !c.needs_sim());
        assert_eq!(c.source(Section::Evse), SourceKind::Driver);
        assert_eq!(c.actuator(Device::Evse), ActuatorKind::Log);
    }
}
