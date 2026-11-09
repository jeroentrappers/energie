//! The TOML configuration file. See `crates/app/README.md` for the reference and
//! `deploy/energie.example.toml` for a commented example.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::entities::PublishRule;
use crate::error::{Error, Result};
use crate::plan::{DeviceConfig, Plan, Provider};
use crate::plug::{Device, Section};
use crate::registry::Registry;

/// What the binary does with its decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Next to the live HA: read everything from HA, decide, never actuate, compare.
    Shadow,
    /// In charge: actuate through the configured actuators.
    Control,
    /// Closed loop against the built-in house model, on a virtual clock.
    Simulate,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Shadow => "shadow",
            Mode::Control => "control",
            Mode::Simulate => "simulate",
        }
    }
}

/// Who owns the Settings (the `input_*` helpers) and the Memory (latches, counters,
/// measurements).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingsSource {
    /// This binary: kept in the state dir, changed from the dashboard, the API or MQTT.
    Local,
    /// Home Assistant's helpers, adopted every tick (shadow lockstep also adopts the Memory);
    /// the local controls are read only.
    Ha,
}

impl SettingsSource {
    pub fn label(self) -> &'static str {
        match self {
            SettingsSource::Local => "local",
            SettingsSource::Ha => "ha",
        }
    }
}

/// Where one section of the [`World`](energie_core::world::World) comes from (the legacy
/// `[sources]` values; also how [`Config::source`] reports a resolved binding).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// The Home Assistant WebSocket bridge.
    Ha,
    /// A native device driver (`energie-drivers`).
    Driver,
    /// The built-in house model.
    Sim,
}

/// Who carries out the commands for one device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActuatorKind {
    /// Log the command, do nothing (forced in shadow mode).
    Log,
    /// Home Assistant service calls over the WebSocket API.
    Ha,
    /// A native device driver.
    Driver,
    /// The built-in house model.
    Sim,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HaConfig {
    /// `http://host:8123`, `ws://host:8123` or the full `ws://host:8123/api/websocket`.
    pub url: String,
    /// A file holding the long-lived access token (only the token, whitespace trimmed).
    pub token_file: PathBuf,
    /// Seed Settings and Memory from the HA helpers when the state dir has none yet.
    #[serde(default = "yes")]
    pub seed: bool,
    /// Seconds to wait for HA's first snapshot at start before going on without it.
    #[serde(default = "default_ha_wait")]
    pub start_wait_s: u64,
    /// Use HA only to import Settings and Memory into an empty state dir (one read-only
    /// `get_states`); nothing reads from or writes to it otherwise (standalone).
    #[serde(default)]
    pub import_only: bool,
}

impl HaConfig {
    /// The WebSocket endpoint derived from [`HaConfig::url`].
    pub fn ws_url(&self) -> Result<String> {
        let u = self.url.trim_end_matches('/');
        let rest = if let Some(r) = u.strip_prefix("http://") {
            r
        } else if let Some(r) = u.strip_prefix("ws://") {
            r
        } else {
            return Err(Error::new(format!(
                "ha.url must start with http:// or ws:// (TLS is not supported, use the LAN address): {u}"
            )));
        };
        let rest = rest.trim_end_matches("/api/websocket");
        Ok(format!("ws://{rest}/api/websocket"))
    }
}

/// Per-section source selection; unset sections use `default`. Deprecated: `[[device]]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SourcesConfig {
    pub default: Option<SourceKind>,
    #[serde(flatten)]
    pub per_section: BTreeMap<Section, SourceKind>,
}

/// Per-device actuator selection; unset devices use `default`. Deprecated: `[[device]]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ActuatorsConfig {
    pub default: Option<ActuatorKind>,
    #[serde(flatten)]
    pub per_device: BTreeMap<Device, ActuatorKind>,
}

/// An allowed difference between Rust and HA: `|rust − ha| <= max(abs, rel·|ha|)`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tolerance {
    #[serde(default)]
    pub abs: f64,
    #[serde(default)]
    pub rel: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShadowConfig {
    /// Adopt HA's Settings and Memory every tick, so only the decision logic is compared.
    pub lockstep: bool,
    /// A derived value matches if HA showed it within ±this many seconds.
    pub time_tolerance_s: u64,
    /// A command matches an HA actuation within ±this many seconds.
    pub command_window_s: u64,
    /// The JSONL mismatch log; default `<state_dir>/shadow-mismatches.jsonl`.
    pub log_file: Option<PathBuf>,
    /// Rotate the log (one `.1` generation) past this size.
    pub log_max_bytes: u64,
    /// Per-field overrides of the default tolerance, by `Derived` field name.
    pub tolerances: BTreeMap<String, Tolerance>,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        ShadowConfig {
            lockstep: true,
            time_tolerance_s: 5,
            command_window_s: 40,
            log_file: None,
            log_max_bytes: 256 * 1024,
            tolerances: BTreeMap::new(),
        }
    }
}

/// Passive MQTT subscription (never publishes) for the SmartEVSE set topics.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    /// A file holding the password.
    pub password_file: Option<PathBuf>,
    #[serde(default = "default_mqtt_topic")]
    pub topic: String,
    #[serde(default = "default_mqtt_client_id")]
    pub client_id: String,
}

/// The MQTT bridge (`crate::bridge`): every entity on its own retained topic, a summary for
/// small displays, `/set` topics for the settable entities, and HA MQTT discovery. Nothing is
/// ever published unless `enabled = true`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MqttPublishConfig {
    pub enabled: bool,
    /// The broker's address (IP or host name).
    pub broker: Option<String>,
    pub port: u16,
    pub username: Option<String>,
    /// A file holding the password.
    pub password_file: Option<PathBuf>,
    pub client_id: String,
    /// Topic prefix: `<prefix>/status`, `<prefix>/<group>/<key>`, `<prefix>/summary`.
    pub prefix: String,
    /// HA MQTT discovery; default on, except in shadow mode (HA still has the entities then).
    pub discovery: Option<bool>,
    pub discovery_prefix: String,
    /// Globs on entity ids: published when one of `include` and none of `exclude` match.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    /// Seconds between two `<prefix>/summary` messages.
    pub summary_interval_s: u64,
}

impl Default for MqttPublishConfig {
    fn default() -> Self {
        MqttPublishConfig {
            enabled: false,
            broker: None,
            port: 1883,
            username: None,
            password_file: None,
            client_id: "energie".into(),
            prefix: "energie".into(),
            discovery: None,
            discovery_prefix: "homeassistant".into(),
            include: vec!["*".into()],
            exclude: Vec::new(),
            summary_interval_s: 5,
        }
    }
}

/// A topic prefix: levels without wildcards, no empty level.
fn topic_prefix_ok(p: &str) -> bool {
    !p.is_empty() && p.split('/').all(|l| !l.is_empty() && !l.contains(['+', '#']))
}

impl MqttPublishConfig {
    fn check(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let bad = |m: &str| Err(Error::new(format!("mqtt_publish: {m}")));
        if self.broker.as_deref().is_none_or(str::is_empty) {
            return bad("enabled needs a broker");
        }
        if !topic_prefix_ok(&self.prefix) || !topic_prefix_ok(&self.discovery_prefix) {
            return bad("prefix and discovery_prefix are topic levels without wildcards or empty levels");
        }
        if self.client_id.is_empty() || self.summary_interval_s == 0 {
            return bad("needs a client_id and summary_interval_s > 0");
        }
        if self.include.iter().chain(&self.exclude).any(String::is_empty) {
            return bad("an empty glob in include/exclude");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyKind {
    #[default]
    Log,
    Ntfy,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NotifyConfig {
    pub kind: NotifyKind,
    /// The ntfy topic URL, e.g. `https://ntfy.sh/mijn-topic`.
    pub url: Option<String>,
    /// Optional file with an ntfy access token (sent as `Authorization: Bearer`).
    pub token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SimConfig {
    /// Seed of the deterministic noise (clouds, load spikes).
    pub seed: u64,
    /// Virtual seconds per wall-clock second.
    pub speed: f64,
    /// Virtual start time (RFC 3339); default: now.
    pub start: Option<String>,
    /// PV peak on the hybrid's DC strings, W.
    pub solis_pv_peak_w: f64,
    /// PV peak of the Omnik on the main roof, W.
    pub omnik_pv_peak_w: f64,
    /// Mean cloud cover, 0 (clear) .. 1 (overcast).
    pub cloudiness: f64,
    /// Battery capacity, kWh, and its start SoC, %.
    pub battery_kwh: f64,
    pub battery_soc: f64,
    pub buzz_plugged: bool,
    pub buzz_soc: f64,
    pub ev3_plugged: bool,
    pub ev3_soc: f64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            seed: 1,
            speed: 1.0,
            start: None,
            solis_pv_peak_w: 5000.0,
            omnik_pv_peak_w: 3000.0,
            cloudiness: 0.3,
            battery_kwh: 10.0,
            battery_soc: 50.0,
            buzz_plugged: true,
            buzz_soc: 40.0,
            ev3_plugged: true,
            ev3_soc: 55.0,
        }
    }
}

/// The history store behind the dashboard charts (`crate::history`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HistoryConfig {
    pub enabled: bool,
    /// Segment directory; default `<state_dir>/history`.
    pub dir: Option<PathBuf>,
    /// Days of 1-minute aggregates.
    pub minute_days: u32,
    /// Days of quarter-hour aggregates (≥ 13 months for the capacity tariff).
    pub quarter_days: u32,
    /// Days of daily totals.
    pub day_days: u32,
    /// Seed an empty store from HA's recorder statistics (read-only; needs `[ha]`).
    pub seed_from_ha: bool,
    /// Globs on entity ids (`fluvius_month_*`, `ev3_*`) whose numeric values are recorded too,
    /// besides the curated series.
    pub extra_series: Vec<String>,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        HistoryConfig {
            enabled: true,
            dir: None,
            minute_days: 14,
            quarter_days: 430,
            day_days: 1100,
            seed_from_ha: false,
            extra_series: Vec::new(),
        }
    }
}

impl HistoryConfig {
    pub fn retention(&self, state_dir: &Path) -> crate::history::Retention {
        let d = |n: u32| i64::from(n) * 86_400;
        crate::history::Retention {
            dir: Some(self.dir.clone().unwrap_or_else(|| state_dir.join("history"))),
            minute_s: d(self.minute_days),
            quarter_s: d(self.quarter_days),
            day_s: d(self.day_days),
        }
    }
}

/// `[site]`: degrees, north and east positive.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SiteConfig {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    /// A file holding the dashboard and API password (HTTP Basic, any user name). Without it
    /// anyone who can reach `listen` can read everything and change every setting.
    pub web_password_file: Option<PathBuf>,
    /// Where the installation is, for the sun position and the default solar forecast plane.
    pub site: Option<SiteConfig>,
    /// Tick period, ms (virtual seconds still advance by this much per tick in simulate).
    #[serde(default = "default_tick_ms")]
    pub tick_ms: u64,
    /// Persist Settings and Memory at most this often (settings changes are written at once).
    #[serde(default = "default_persist_s")]
    pub persist_interval_s: u64,
    /// Who owns Settings and Memory; default `ha` in shadow mode, `local` otherwise.
    pub settings_source: Option<SettingsSource>,
    pub ha: Option<HaConfig>,
    #[serde(default)]
    pub sources: SourcesConfig,
    #[serde(default)]
    pub actuators: ActuatorsConfig,
    #[serde(default)]
    pub shadow: ShadowConfig,
    pub mqtt: Option<MqttConfig>,
    /// The MQTT bridge (off by default).
    #[serde(default)]
    pub mqtt_publish: MqttPublishConfig,
    #[serde(default)]
    pub notify: NotifyConfig,
    #[serde(default)]
    pub sim: SimConfig,
    /// Deprecated: the connection settings of the native drivers for `driver` selections,
    /// `[devices.<name>]` (translated by [`crate::legacy`]).
    #[serde(default)]
    pub devices: BTreeMap<String, toml::Table>,
    /// The device instances and their roles (`[[device]]`, see [`crate::plan`]).
    #[serde(default)]
    pub device: Vec<DeviceConfig>,
    /// Which entities a publisher sends, and how often (`[[publish]]`, see
    /// [`crate::entities`]).
    #[serde(default)]
    pub publish: Vec<PublishRule>,
    #[serde(default)]
    pub history: HistoryConfig,
    /// How control mode carries out commands (`[control]`, see [`crate::control`]).
    #[serde(default)]
    pub control: crate::control::ControlConfig,
}

fn yes() -> bool {
    true
}
fn default_ha_wait() -> u64 {
    30
}
fn default_listen() -> String {
    "127.0.0.1:8080".into()
}
fn default_state_dir() -> PathBuf {
    PathBuf::from("/var/lib/energie")
}
fn default_tick_ms() -> u64 {
    1000
}
fn default_persist_s() -> u64 {
    60
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_mqtt_topic() -> String {
    "SmartEVSE/1234/Set/#".into()
}
fn default_mqtt_client_id() -> String {
    "energie-shadow".into()
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let c: Config = toml::from_str(text)?;
        c.validate()?;
        Ok(c)
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
        Config::parse(&text)
    }

    /// The device instances: `[[device]]`, or the translation of the legacy tables.
    pub fn instances(&self, strict: bool) -> Result<Vec<DeviceConfig>> {
        if !crate::legacy::present(self) {
            return Ok(self.device.clone());
        }
        if !self.device.is_empty() {
            return Err(Error::new(
                "use either [[device]] or the deprecated [sources]/[actuators]/[devices.*] tables, not both",
            ));
        }
        crate::legacy::translate(self, strict)
    }

    /// The resolved role bindings. Lenient: a `control` on a read-only device stays bound (the
    /// start-up refuses it); [`Config::parse`] has refused it already for a file.
    pub fn plan(&self) -> Result<Plan> {
        Plan::resolve_with(self.mode, &self.instances(false)?, Registry::builtin(), false, self.ha_bridge())
    }

    /// Whether the MQTT bridge publishes HA discovery: `[mqtt_publish] enabled` and
    /// `discovery` (default: not in shadow mode).
    pub fn discovery(&self) -> bool {
        self.mqtt_publish.enabled && self.mqtt_publish.discovery.unwrap_or(self.mode != Mode::Shadow)
    }

    /// Whether HA is there as a live bridge: an `[ha]` section that is not `import_only`.
    pub fn ha_bridge(&self) -> bool {
        self.ha.as_ref().is_some_and(|h| !h.import_only)
    }

    /// Who owns Settings and Memory: as configured, else HA in shadow mode and this binary
    /// otherwise.
    pub fn settings_source(&self) -> SettingsSource {
        self.settings_source.unwrap_or(if self.mode == Mode::Shadow {
            SettingsSource::Ha
        } else {
            SettingsSource::Local
        })
    }

    /// What a config check should warn about: the legacy tables, with their `[[device]]` form;
    /// World sections nobody reads (standalone).
    pub fn warnings(&self) -> Vec<String> {
        let mut out = match self.instances(false) {
            Ok(d) if crate::legacy::present(self) => vec![format!(
                "[sources], [actuators] and [devices.*] are deprecated; this is the same setup as [[device]] tables:\n\n{}",
                crate::legacy::to_toml(&d)
            )],
            _ => Vec::new(),
        };
        if let Ok(p) = self.plan() {
            out.extend(
                p.unread().into_iter().map(|s| {
                    format!("no device reads {} (World section {s:?}): it stays unavailable", s.slot().name())
                }),
            );
        }
        out
    }

    /// The source of one World section (the first reader, for a summed PV section).
    pub fn source(&self, s: Section) -> SourceKind {
        let first = self.plan().ok().and_then(|p| p.readers.get(&s.slot()).and_then(|r| r.first().cloned()));
        match first {
            Some(Provider::Ha) => SourceKind::Ha,
            Some(Provider::Device(_)) => SourceKind::Driver,
            _ => SourceKind::Sim,
        }
    }

    /// The actuator of one device. Shadow mode never actuates: always [`ActuatorKind::Log`].
    pub fn actuator(&self, d: Device) -> ActuatorKind {
        if self.mode == Mode::Shadow {
            return ActuatorKind::Log;
        }
        match self.plan().ok().and_then(|p| p.controllers.get(&d).cloned()) {
            Some(Provider::Ha) => ActuatorKind::Ha,
            Some(Provider::Sim) => ActuatorKind::Sim,
            Some(Provider::Device(_)) => ActuatorKind::Driver,
            None | Some(Provider::Unavailable) => ActuatorKind::Log,
        }
    }

    /// Whether anything needs the Home Assistant connection.
    pub fn needs_ha(&self) -> bool {
        self.mode == Mode::Shadow
            || self.settings_source() == SettingsSource::Ha
            || self.plan().is_ok_and(|p| p.uses(&Provider::Ha, self.mode))
    }

    /// Whether anything needs the house model.
    pub fn needs_sim(&self) -> bool {
        self.plan().is_ok_and(|p| p.uses(&Provider::Sim, self.mode))
    }

    /// The shadow mismatch log path.
    pub fn shadow_log(&self) -> PathBuf {
        self.shadow.log_file.clone().unwrap_or_else(|| self.state_dir.join("shadow-mismatches.jsonl"))
    }

    fn validate(&self) -> Result<()> {
        if let Some(s) = self.site
            && !((-90.0..=90.0).contains(&s.latitude) && (-180.0..=180.0).contains(&s.longitude))
        {
            return Err(Error::new("[site]: latitude must be within ±90 and longitude within ±180"));
        }
        Plan::resolve(self.mode, &self.instances(true)?, Registry::builtin(), true)?;
        for r in &self.publish {
            r.check()?;
        }
        self.mqtt_publish.check()?;
        if self.needs_ha() {
            let ha =
                self.ha.as_ref().ok_or_else(|| Error::new("an [ha] section is required by this mode/selection"))?;
            if ha.import_only {
                return Err(Error::new(
                    "[ha] import_only = true, but this mode or settings_source = \"ha\" needs the HA connection",
                ));
            }
        }
        if let Some(ha) = &self.ha {
            ha.ws_url()?;
        }
        if self.notify.kind == NotifyKind::Ntfy && self.notify.url.is_none() {
            return Err(Error::new("notify.kind = \"ntfy\" needs notify.url"));
        }
        let h = &self.history;
        if h.minute_days == 0 || h.quarter_days == 0 || h.day_days == 0 {
            return Err(Error::new("history: minute_days, quarter_days and day_days must be > 0"));
        }
        if h.seed_from_ha && self.ha.is_none() {
            return Err(Error::new("history.seed_from_ha needs an [ha] section"));
        }
        if h.extra_series.iter().any(String::is_empty) {
            return Err(Error::new("history.extra_series: an empty glob"));
        }
        self.control.check()?;
        if self.tick_ms == 0 {
            return Err(Error::new("tick_ms must be > 0"));
        }
        if self.sim.speed.is_nan() || self.sim.speed <= 0.0 {
            return Err(Error::new("sim.speed must be > 0"));
        }
        if let Some(start) = &self.sim.start {
            crate::ha::entity::parse_time(start).ok_or_else(|| Error::new(format!("sim.start: bad time {start}")))?;
        }
        Ok(())
    }
}

/// Read a secret file (token, password) and trim surrounding whitespace.
pub fn read_secret(path: &Path) -> Result<String> {
    energie_drivers::plugin::read_secret(path).map_err(|e| Error::new(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HA: &str = "[ha]\nurl = \"http://192.168.1.10:8123\"\ntoken_file = \"/etc/energie/token\"\n";

    /// A minimal shadow config: everything from HA, nothing actuated, lockstep on by default.
    #[test]
    fn shadow_defaults_read_from_ha_and_never_actuate() {
        let c = Config::parse(&format!("mode = \"shadow\"\n{HA}")).unwrap();
        assert_eq!(c.mode, Mode::Shadow);
        assert_eq!(c.source(Section::P1), SourceKind::Ha);
        assert_eq!(c.actuator(Device::Evse), ActuatorKind::Log);
        assert!(c.shadow.lockstep);
        assert_eq!(c.shadow.command_window_s, 40);
        assert_eq!(c.listen, "127.0.0.1:8080");
        assert_eq!(c.state_dir, PathBuf::from("/var/lib/energie"));
        assert_eq!(c.shadow_log(), PathBuf::from("/var/lib/energie/shadow-mismatches.jsonl"));
        assert!(c.needs_ha());
        assert!(!c.needs_sim());
        assert!(c.ha.as_ref().unwrap().seed);
        assert_eq!(c.ha.as_ref().unwrap().start_wait_s, 30);
        assert_eq!(Mode::Shadow.label(), "shadow");
    }

    /// Shadow mode ignores actuator selections: it must never touch a device.
    #[test]
    fn shadow_forces_log_actuators_even_when_configured_otherwise() {
        let c =
            Config::parse(&format!("mode = \"shadow\"\n{HA}[actuators]\ndefault = \"ha\"\nevse = \"sim\"\n")).unwrap();
        for d in Device::ALL {
            assert_eq!(c.actuator(d), ActuatorKind::Log);
        }
    }

    /// Simulate mode defaults every source and actuator to the house model and needs no HA.
    #[test]
    fn simulate_defaults_to_the_house_model() {
        let c =
            Config::parse("mode = \"simulate\"\n[sim]\nspeed = 10.0\nstart = \"2026-06-21T06:00:00+02:00\"\n").unwrap();
        assert_eq!(c.source(Section::Solis), SourceKind::Sim);
        assert_eq!(c.actuator(Device::Solis), ActuatorKind::Sim);
        assert!(!c.needs_ha());
        assert!(c.needs_sim());
        assert_eq!(c.sim.speed, 10.0);
        assert_eq!(Mode::Simulate.label(), "simulate");
        assert!(c.history.enabled && !c.history.seed_from_ha);
        let r = c.history.retention(Path::new("/s"));
        assert_eq!(r.dir, Some(PathBuf::from("/s/history")));
        assert_eq!((r.minute_s, r.quarter_s, r.day_s), (14 * 86_400, 430 * 86_400, 1100 * 86_400));
    }

    /// Per-section and per-device selections override the defaults.
    #[test]
    fn per_section_and_per_device_selection() {
        let c = Config::parse(&format!(
            "mode = \"control\"\n{HA}[sources]\ndefault = \"ha\"\np1 = \"driver\"\n[actuators]\nevse = \"driver\"\ndefault = \"ha\"\n\
             [devices.homewizard]\naddress = \"192.168.1.30\"\nserial = \"s\"\ntoken_file = \"t\"\n\
             [devices.smartevse]\nhost = \"192.168.1.10\"\nread_only = false\n"
        ))
        .unwrap();
        assert_eq!(c.source(Section::P1), SourceKind::Driver);
        assert_eq!(c.source(Section::Prices), SourceKind::Ha);
        assert_eq!(c.actuator(Device::Evse), ActuatorKind::Driver);
        assert_eq!(c.actuator(Device::Cable), ActuatorKind::Ha);
        let bare = Config::parse(&format!("mode = \"control\"\n{HA}")).unwrap();
        assert_eq!(bare.actuator(Device::Cable), ActuatorKind::Log);
        assert_eq!(Mode::Control.label(), "control");
    }

    /// Control mode with only a sim source still counts as needing the model.
    #[test]
    fn control_with_a_sim_actuator_needs_the_model_but_not_ha() {
        let c =
            Config::parse("mode = \"control\"\n[sources]\ndefault = \"sim\"\n[actuators]\nsolis = \"sim\"\n").unwrap();
        assert!(!c.needs_ha());
        assert!(c.needs_sim());
    }

    /// Misconfigurations are rejected with a message that says what to fix.
    #[test]
    fn invalid_configs_are_rejected() {
        let err = |t: &str| Config::parse(t).unwrap_err().0;
        assert!(err("mode = \"shadow\"\n").contains("[ha]"));
        assert!(err("mode = \"shadow\"\n[ha]\nurl = \"https://x\"\ntoken_file = \"t\"\n").contains("TLS"));
        assert!(err("mode = \"simulate\"\n[notify]\nkind = \"ntfy\"\n").contains("notify.url"));
        assert!(err("mode = \"simulate\"\ntick_ms = 0\n").contains("tick_ms"));
        assert!(err("mode = \"simulate\"\n[sim]\nspeed = 0.0\n").contains("speed"));
        assert!(err("mode = \"simulate\"\n[sim]\nstart = \"gisteren\"\n").contains("sim.start"));
        assert!(err("mode = \"simulate\"\nbogus = 1\n").contains("bogus"));
        assert!(err("mode = \"simulate\"\n[sources]\nwasmachine = \"ha\"\n").contains("wasmachine"));
        assert!(err("mode = \"simulate\"\n[history]\nminute_days = 0\n").contains("minute_days"));
        assert!(err("mode = \"simulate\"\n[history]\nseed_from_ha = true\n").contains("[ha]"));
        assert!(err("mode = \"simulate\"\n[history]\nextra_series = [\"\"]\n").contains("extra_series"));
    }

    /// The WebSocket URL is derived from any of the usual HA address spellings.
    #[test]
    fn ws_url_is_derived_from_http_or_ws_addresses() {
        let mk = |u: &str| HaConfig {
            url: u.into(),
            token_file: "t".into(),
            seed: true,
            start_wait_s: 1,
            import_only: false,
        };
        assert_eq!(mk("http://h:8123").ws_url().unwrap(), "ws://h:8123/api/websocket");
        assert_eq!(mk("http://h:8123/").ws_url().unwrap(), "ws://h:8123/api/websocket");
        assert_eq!(mk("ws://h:8123/api/websocket").ws_url().unwrap(), "ws://h:8123/api/websocket");
    }

    /// Config files load from disk; secrets are trimmed; a missing file names its path.
    #[test]
    fn load_from_disk_and_read_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(
            &p,
            "mode = \"simulate\"\n[mqtt]\nhost = \"127.0.0.1\"\n[notify]\nkind = \"ntfy\"\nurl = \"http://x/t\"\n",
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        let m = c.mqtt.unwrap();
        assert_eq!((m.port, m.topic.as_str(), m.client_id.as_str()), (1883, "SmartEVSE/1234/Set/#", "energie-shadow"));
        assert_eq!(c.notify.kind, NotifyKind::Ntfy);
        assert!(Config::load(&dir.path().join("nope.toml")).unwrap_err().0.contains("nope.toml"));
        let s = dir.path().join("token");
        std::fs::write(&s, "  abc\n").unwrap();
        assert_eq!(read_secret(&s).unwrap(), "abc");
        assert!(read_secret(&dir.path().join("x")).is_err());
    }
    /// The MQTT bridge is off by default; enabled it needs a broker and sane topics; discovery
    /// follows the mode unless set.
    #[test]
    fn mqtt_publish_section() {
        let off = Config::parse("mode = \"control\"\n").unwrap();
        assert!(!off.mqtt_publish.enabled && !off.discovery());
        assert_eq!((off.mqtt_publish.prefix.as_str(), off.mqtt_publish.port), ("energie", 1883));
        let on = "mode = \"control\"\n[mqtt_publish]\nenabled = true\nbroker = \"127.0.0.1\"\n";
        let c = Config::parse(on).unwrap();
        assert!(c.discovery());
        assert!(!Config::parse(&format!("{on}discovery = false\n")).unwrap().discovery());
        let shadow =
            Config::parse(&format!("mode = \"shadow\"\n{HA}[mqtt_publish]\nenabled = true\nbroker = \"127.0.0.1\"\n"))
                .unwrap();
        assert!(!shadow.discovery(), "HA still has the entities in shadow mode");
        let err = |t: &str| Config::parse(t).unwrap_err().0;
        assert!(err("mode = \"control\"\n[mqtt_publish]\nenabled = true\n").contains("broker"));
        for bad in ["prefix = \"a/#\"", "prefix = \"/a\"", "discovery_prefix = \"\"", "prefix = \"a//b\""] {
            assert!(err(&format!("{on}{bad}\n")).contains("prefix"), "{bad}");
        }
        assert!(err(&format!("{on}summary_interval_s = 0\n")).contains("summary_interval_s"));
        assert!(err(&format!("{on}exclude = [\"\"]\n")).contains("glob"));
        assert!(err(&format!("{on}bogus = 1\n")).contains("bogus"));
        assert!(Config::parse("mode = \"control\"\n[mqtt_publish]\nprefix = \"#\"\n").is_ok(), "off: not checked");
        assert!(Config::parse(&format!("{on}prefix = \"huis/energie\"\n")).is_ok());
    }

    /// Settings belong to HA in shadow mode and to the binary otherwise, unless configured;
    /// `ha` needs a live `[ha]`.
    #[test]
    fn settings_source_defaults_and_requirements() {
        let shadow = Config::parse(&format!("mode = \"shadow\"\n{HA}")).unwrap();
        assert_eq!(shadow.settings_source(), SettingsSource::Ha);
        let local = Config::parse(&format!("mode = \"shadow\"\nsettings_source = \"local\"\n{HA}")).unwrap();
        assert_eq!(local.settings_source(), SettingsSource::Local);
        let control = Config::parse(&format!("mode = \"control\"\n{HA}")).unwrap();
        assert_eq!((control.settings_source(), control.settings_source().label()), (SettingsSource::Local, "local"));
        assert_eq!(SettingsSource::Ha.label(), "ha");
        let from_ha = Config::parse(&format!("mode = \"control\"\nsettings_source = \"ha\"\n{HA}")).unwrap();
        assert!(from_ha.needs_ha());
        let err = |t: &str| Config::parse(t).unwrap_err().0;
        assert!(err("mode = \"control\"\nsettings_source = \"ha\"\n").contains("[ha]"));
        assert!(err(&format!("mode = \"shadow\"\n{HA}import_only = true\n")).contains("import_only"));
        assert!(err("mode = \"simulate\"\n[ha]\nurl = \"https://x\"\ntoken_file = \"t\"\n").contains("TLS"));
    }

    /// Standalone: control mode without `[ha]` (or with `import_only`) needs no HA; the
    /// sections no device reads stay unavailable, and the config check says which.
    #[test]
    fn standalone_needs_no_ha_and_warns_about_unread_sections() {
        let bare = Config::parse("mode = \"control\"\n").unwrap();
        assert!(!bare.needs_ha() && !bare.ha_bridge());
        let w = bare.warnings();
        assert_eq!(w.len(), Section::ALL.len());
        assert_eq!(w[0], "no device reads grid_meter (World section P1): it stays unavailable");
        let import = Config::parse(&format!("mode = \"control\"\n{HA}import_only = true\n")).unwrap();
        assert!(!import.needs_ha() && !import.ha_bridge());
        assert_eq!(import.warnings().len(), Section::ALL.len());
        let bridged = Config::parse(&format!("mode = \"control\"\n{HA}")).unwrap();
        assert!(bridged.ha_bridge() && bridged.warnings().is_empty());
        assert_eq!(bare.actuator(Device::Evse), ActuatorKind::Log);
    }

    /// `[[device]]` tables configure the same selections; mixing them with the legacy tables
    /// is refused; only the legacy tables warn.
    #[test]
    fn device_tables_and_legacy_tables() {
        let c = Config::parse(&format!(
            "mode = \"control\"\n{HA}[[device]]\nid = \"ha\"\nkind = \"ha\"\nrole = \"*\"\ncontrol = \"charger:ev3\"\nread_only = false\n\
             [[device]]\nid = \"evse\"\nkind = \"smartevse_mqtt\"\nrole = \"charger\"\ncontrol = \"charger\"\nread_only = false\nhost = \"h\"\n"
        ))
        .unwrap();
        assert_eq!(c.source(Section::Evse), SourceKind::Driver);
        assert_eq!(c.source(Section::P1), SourceKind::Ha);
        assert_eq!(c.actuator(Device::Evse), ActuatorKind::Driver);
        assert_eq!(c.actuator(Device::Cable), ActuatorKind::Ha);
        assert_eq!(c.actuator(Device::Solis), ActuatorKind::Log);
        assert!(c.warnings().is_empty());
        let mixed = format!(
            "mode = \"control\"\n{HA}[sources]\ndefault = \"ha\"\n[[device]]\nid = \"x\"\nkind = \"ha\"\nrole = \"pv\"\n"
        );
        assert!(Config::parse(&mixed).unwrap_err().0.starts_with("use either [[device]] or the deprecated"));
        let legacy = Config::parse(&format!("mode = \"shadow\"\n{HA}[sources]\ndefault = \"ha\"\n")).unwrap();
        let w = legacy.warnings();
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("deprecated") && w[0].contains("[[device]]\nid = \"ha\"\nkind = \"ha\""), "{}", w[0]);
        // A config that no longer resolves (changed after parsing) reports no warning and no HA.
        let mut broken = legacy.clone();
        broken.device.push(crate::plan::DeviceConfig::default());
        assert!(broken.warnings().is_empty());
        assert!(broken.plan().is_err());
        assert!(!broken.needs_sim());
        assert_eq!(broken.source(Section::P1), SourceKind::Sim, "no plan: no reader");
        let control = Config { mode: Mode::Control, ..broken };
        assert_eq!(control.actuator(Device::Evse), ActuatorKind::Log);
        assert!(!control.needs_ha());
        let bad_publish = "mode = \"simulate\"\n[[publish]]\nmatch = \"\"\n";
        assert!(Config::parse(bad_publish).unwrap_err().0.contains("[[publish]]"));
    }
}
