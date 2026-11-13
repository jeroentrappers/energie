//! The [`DriverKind`]s of the drivers in this crate: their config tables and how each is built.
//!
//! | Kind | Slots | Writable | Poll |
//! | --- | --- | --- | --- |
//! | `homewizard` | `grid_meter` | no | 1 s |
//! | `solis_modbus` | `battery_inverter` | yes | 15 s (settings 60 s, energies 300 s) |
//! | `omnik` | `pv` | no | 30 s |
//! | `smartevse_mqtt` | `charger:buzz` | yes | 1 s |
//! | `tuya_cable` | `charger:ev3` | yes | 1 s |
//! | `tuya_evcharger` | `charger:ev3` | yes | 1 s |
//! | `energy_charts` | `price_feed` | yes (refresh) | 10 s |
//! | `http_json` | `pv`, `grid_meter` | no | 5 s |
//! | `static` | every slot | no | 1 s |

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::error::DriverError;
use crate::homewizard::{HomeWizard, HomeWizardConfig};
use crate::omnik::Omnik;
use crate::plugin::{BuildCx, Driver, DriverKind, Factory, Slot, factory, read_secret, socket};
use crate::prices::{PriceFeed, PricesConfig};
use crate::smartevse::{EvseConfig, SmartEvse};
use crate::solis::{Solis, SolisConfig};
use crate::tuya::{CableConfig, CableHandle, EvChargerHandle, Version, evcharger};
use crate::{error::Result, fixed, http_json};

/// Every kind in this crate, for the app's registry.
pub fn all() -> Vec<Box<dyn Factory>> {
    vec![
        factory::<HomeWizardKind>(),
        factory::<SolisKind>(),
        factory::<OmnikKind>(),
        factory::<SmartEvseKind>(),
        factory::<TuyaCableKind>(),
        factory::<TuyaEvChargerKind>(),
        factory::<EnergyChartsKind>(),
        factory::<http_json::HttpJsonKind>(),
        factory::<fixed::StaticKind>(),
    ]
}

/// `kind = "homewizard"`: the HomeWizard P1 meter (local API v2, HTTPS).
pub struct HomeWizardKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeWizardTable {
    /// `ip` or `ip:port` (port 443 by default).
    pub address: String,
    /// The certificate is checked for `<serial>.p1dongle.device.homewizard.energy`.
    pub serial: String,
    /// A file holding the local API v2 bearer token.
    pub token_file: PathBuf,
    /// A PEM file to trust instead of HomeWizard's built-in CA.
    pub ca_file: Option<PathBuf>,
}

impl DriverKind for HomeWizardKind {
    type Config = HomeWizardTable;
    const NAME: &'static str = "homewizard";
    const SLOTS: &'static [Slot] = &[Slot::GridMeter];

    fn check(c: &HomeWizardTable) -> Result<()> {
        socket(&c.address, 443).map(drop)
    }

    fn build(c: HomeWizardTable, _: &BuildCx) -> Result<Box<dyn Driver>> {
        let mut hw = HomeWizardConfig::p1(socket(&c.address, 443)?, read_secret(&c.token_file)?, &c.serial);
        if let Some(ca) = &c.ca_file {
            hw.ca_pem = read_secret(ca)?;
        }
        Ok(Box::new(HomeWizard::new(hw)?))
    }
}

fn one() -> u8 {
    1
}

/// `kind = "solis_modbus"`: the Solis hybrid over the Modbus TCP gateway.
pub struct SolisKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SolisTable {
    /// `ip` or `ip:port` (port 502 by default).
    pub address: String,
    /// Modbus unit id.
    #[serde(default = "one")]
    pub slave: u8,
    /// Minimum gap between Modbus frames (HA: `MIN_INTER_FRAME_MS = 310`).
    #[serde(default = "solis_frame_gap")]
    pub frame_gap_ms: u64,
    /// Per-request timeout (HA: 5 s).
    #[serde(default = "solis_timeout")]
    pub timeout_ms: u64,
    /// Read the settings registers every this many polls (and after every write).
    #[serde(default = "solis_settings_every")]
    pub settings_every: u32,
    /// RC timeout written with a forced mode when the inverter holds none in 1..30 min.
    #[serde(default = "solis_rc_fallback")]
    pub rc_timeout_fallback_min: u16,
    /// Re-select an RC mode the inverter already holds at most this often (a forced mode: only
    /// 43135, capped at half the RC timeout; `None`: never). 0 (the default) selects on every
    /// command, as HA. An S6-EH1P did not restart its RC timeout on a 43135-only write and fell
    /// back to self-use every 10 minutes, so only set this for an inverter checked to keep a
    /// forced mode past its timeout (CONTROL.md, checklist item 9).
    #[serde(default = "solis_rc_keepalive")]
    pub rc_keepalive_s: u64,
}

fn solis_frame_gap() -> u64 {
    310
}
fn solis_timeout() -> u64 {
    5000
}
fn solis_settings_every() -> u32 {
    4
}
fn solis_rc_fallback() -> u16 {
    10
}
fn solis_rc_keepalive() -> u64 {
    0
}

impl DriverKind for SolisKind {
    type Config = SolisTable;
    const NAME: &'static str = "solis_modbus";
    const SLOTS: &'static [Slot] = &[Slot::BatteryInverter];
    const WRITABLE: bool = true;
    /// HA's fast poll group (15 s); the settings every `settings_every` polls (60 s).
    const POLL_MS: u64 = 15_000;

    fn check(c: &SolisTable) -> Result<()> {
        if c.settings_every == 0 || c.timeout_ms == 0 || !(1..=30).contains(&c.rc_timeout_fallback_min) {
            return Err(DriverError::Config(
                "settings_every and timeout_ms must be > 0, rc_timeout_fallback_min 1..30".into(),
            ));
        }
        socket(&c.address, 502).map(drop)
    }

    fn build(c: SolisTable, _: &BuildCx) -> Result<Box<dyn Driver>> {
        Ok(Box::new(Solis::tcp(SolisConfig {
            timeout: Duration::from_millis(c.timeout_ms),
            frame_gap: Duration::from_millis(c.frame_gap_ms),
            settings_every: c.settings_every,
            rc_timeout_fallback_min: c.rc_timeout_fallback_min,
            rc_keepalive: (c.rc_keepalive_s > 0).then(|| Duration::from_secs(c.rc_keepalive_s)),
            ..SolisConfig::new(socket(&c.address, 502)?, c.slave)
        })))
    }
}

/// `kind = "omnik"`: an Omnik string inverter (`js/status.js`).
pub struct OmnikKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OmnikTable {
    /// `http://192.168.1.40`.
    pub base_url: String,
}

impl DriverKind for OmnikKind {
    type Config = OmnikTable;
    const NAME: &'static str = "omnik";
    const SLOTS: &'static [Slot] = &[Slot::Pv];
    const POLL_MS: u64 = 30_000;

    fn build(c: OmnikTable, _: &BuildCx) -> Result<Box<dyn Driver>> {
        Ok(Box::new(Omnik::new(c.base_url.trim_end_matches('/'), Duration::from_secs(5))?))
    }
}

fn mqtt_port() -> u16 {
    1883
}
fn evse_prefix() -> String {
    "SmartEVSE/1234".into()
}
fn evse_client_id() -> String {
    "energie-smartevse".into()
}

/// `kind = "smartevse_mqtt"`: a SmartEVSE v3 over MQTT.
pub struct SmartEvseKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmartEvseTable {
    pub host: String,
    #[serde(default = "mqtt_port")]
    pub port: u16,
    /// Topic prefix, `SmartEVSE/1234`.
    #[serde(default = "evse_prefix")]
    pub prefix: String,
    pub username: Option<String>,
    /// A file holding the broker password (used with `username`).
    pub password_file: Option<PathBuf>,
    #[serde(default = "evse_client_id")]
    pub client_id: String,
}

impl DriverKind for SmartEvseKind {
    type Config = SmartEvseTable;
    const NAME: &'static str = "smartevse_mqtt";
    const SLOTS: &'static [Slot] = &[Slot::ChargerBuzz];
    const WRITABLE: bool = true;

    fn build(c: SmartEvseTable, _: &BuildCx) -> Result<Box<dyn Driver>> {
        let password = c.password_file.as_deref().map(read_secret).transpose()?;
        Ok(Box::new(SmartEvse::spawn(EvseConfig {
            credentials: c.username.clone().map(|u| (u, password.unwrap_or_default())),
            client_id: c.client_id.clone(),
            ..EvseConfig::new(&c.host, c.port, &c.prefix)
        })))
    }
}

/// `kind = "tuya_cable"`: the Kolanky Tuya charging cable (local protocol 3.4).
pub struct TuyaCableKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuyaCableTable {
    /// `ip` or `ip:port` (port 6668 by default).
    pub address: String,
    pub device_id: String,
    /// A file holding the 16-character local key.
    pub local_key_file: PathBuf,
    /// The wait between `Start charging` and the current (`script.laadkabel_start_laden`: 3 s).
    #[serde(default = "cable_start_delay")]
    pub start_delay_ms: u64,
}

fn cable_start_delay() -> u64 {
    3000
}

impl DriverKind for TuyaCableKind {
    type Config = TuyaCableTable;
    const NAME: &'static str = "tuya_cable";
    const SLOTS: &'static [Slot] = &[Slot::ChargerEv3];
    const WRITABLE: bool = true;

    fn check(c: &TuyaCableTable) -> Result<()> {
        socket(&c.address, 6668).map(drop)
    }

    fn build(c: TuyaCableTable, _: &BuildCx) -> Result<Box<dyn Driver>> {
        let cfg = CableConfig {
            start_delay: Duration::from_millis(c.start_delay_ms),
            ..CableConfig::new(socket(&c.address, 6668)?, &c.device_id, read_secret(&c.local_key_file)?)
        };
        Ok(Box::new(CableHandle::spawn(cfg)?))
    }
}

/// `kind = "tuya_evcharger"`: a Tuya EV charger of category `qccdz` (standard charging-pile
/// DPs), local protocol 3.5 by default: the EV3's charger since the Kolanky cable was replaced.
pub struct TuyaEvChargerKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuyaEvChargerTable {
    /// `ip` or `ip:port` (port 6668 by default).
    pub address: String,
    pub device_id: String,
    /// A file holding the 16-character local key.
    pub local_key_file: PathBuf,
    /// The local protocol, `"3.5"` (default) or `"3.4"`.
    #[serde(default = "evcharger_protocol")]
    pub protocol: String,
    /// How often the phase meter (DP 6) is requested, s (default 5); `0`: never, and the reading
    /// estimates the current from the set current while charging.
    #[serde(default = "evcharger_meter_refresh_s")]
    pub meter_refresh_s: u64,
}

fn evcharger_protocol() -> String {
    "3.5".into()
}

fn evcharger_meter_refresh_s() -> u64 {
    5
}

impl DriverKind for TuyaEvChargerKind {
    type Config = TuyaEvChargerTable;
    const NAME: &'static str = "tuya_evcharger";
    const SLOTS: &'static [Slot] = &[Slot::ChargerEv3];
    const WRITABLE: bool = true;

    fn check(c: &TuyaEvChargerTable) -> Result<()> {
        socket(&c.address, 6668)?;
        Version::parse(&c.protocol).map(drop)
    }

    fn build(c: TuyaEvChargerTable, _: &BuildCx) -> Result<Box<dyn Driver>> {
        let cfg = CableConfig {
            version: Version::parse(&c.protocol)?,
            refresh: Duration::from_secs(c.meter_refresh_s),
            ..evcharger::config(socket(&c.address, 6668)?, &c.device_id, &read_secret(&c.local_key_file)?)
        };
        Ok(Box::new(EvChargerHandle::spawn(cfg)?))
    }
}

/// `kind = "energy_charts"`: day-ahead prices from energy-charts with the ENTSO-E fallback.
pub struct EnergyChartsKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnergyChartsTable {
    /// A file holding the ENTSO-E security token; without it there is no fallback.
    pub entsoe_token_file: Option<PathBuf>,
    /// Override of `https://api.energy-charts.info`.
    pub energy_charts_url: Option<String>,
    /// Override of `https://web-api.tp.entsoe.eu/api`.
    pub entsoe_url: Option<String>,
}

impl DriverKind for EnergyChartsKind {
    type Config = EnergyChartsTable;
    const NAME: &'static str = "energy_charts";
    const SLOTS: &'static [Slot] = &[Slot::PriceFeed];
    const WRITABLE: bool = true;
    const POLL_MS: u64 = 10_000;

    fn build(c: EnergyChartsTable, _: &BuildCx) -> Result<Box<dyn Driver>> {
        let token = c.entsoe_token_file.as_deref().map(read_secret).transpose()?;
        let base = PricesConfig::belgium(token);
        let cfg = PricesConfig {
            energy_charts_url: c.energy_charts_url.clone().unwrap_or(base.energy_charts_url.clone()),
            entsoe_url: c.entsoe_url.clone().unwrap_or(base.entsoe_url.clone()),
            ..base
        };
        Ok(Box::new(PriceFeed::new(cfg)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The registry list: unique names, the documented slots, poll periods and writability.
    #[test]
    fn kinds_and_their_contract() {
        let all = all();
        let mut names: Vec<_> = all.iter().map(|f| f.name()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), all.len());
        let get = |n: &str| all.iter().find(|f| f.name() == n).unwrap();
        let table = [
            ("homewizard", Slot::GridMeter, false, 1000),
            ("solis_modbus", Slot::BatteryInverter, true, 15_000),
            ("omnik", Slot::Pv, false, 30_000),
            ("smartevse_mqtt", Slot::ChargerBuzz, true, 1000),
            ("tuya_cable", Slot::ChargerEv3, true, 1000),
            ("tuya_evcharger", Slot::ChargerEv3, true, 1000),
            ("energy_charts", Slot::PriceFeed, true, 10_000),
        ];
        for (name, slot, writable, poll) in table {
            let f = get(name);
            assert_eq!((f.slots(), f.writable(), f.poll_ms()), (&[slot][..], writable, poll), "{name}");
        }
    }

    /// The tables have the documented defaults; addresses are checked without I/O.
    #[test]
    fn tables_parse_with_defaults_and_check_addresses() {
        let solis: SolisTable = serde_json::from_value(json!({"address": "192.168.1.20:502"})).unwrap();
        assert_eq!(
            (
                solis.slave,
                solis.frame_gap_ms,
                solis.timeout_ms,
                solis.settings_every,
                solis.rc_timeout_fallback_min,
                solis.rc_keepalive_s
            ),
            (1, 310, 5000, 4, 10, 0)
        );
        let cable: TuyaCableTable =
            serde_json::from_value(json!({"address": "a", "device_id": "d", "local_key_file": "k"})).unwrap();
        assert_eq!(cable.start_delay_ms, 3000, "script.laadkabel_start_laden: delay 3 s");
        let evse: SmartEvseTable = serde_json::from_value(json!({"host": "h"})).unwrap();
        assert_eq!(
            (evse.port, evse.prefix.as_str(), evse.client_id.as_str()),
            (1883, "SmartEVSE/1234", "energie-smartevse")
        );
        let all = all();
        let get = |n: &str| all.iter().find(|f| f.name() == n).unwrap();
        let addr = |n: &str, t| get(n).check(&t).unwrap_err().to_string();
        assert!(addr("homewizard", json!({"address": "meter", "serial": "s", "token_file": "t"})).contains("address"));
        assert!(addr("solis_modbus", json!({"address": "x"})).contains("address"));
        for bad in [json!({"settings_every": 0}), json!({"rc_timeout_fallback_min": 0}), json!({"timeout_ms": 0})] {
            let mut t = json!({"address": "192.168.1.20"});
            t.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
            assert!(addr("solis_modbus", t).contains("settings_every"));
        }
        assert!(
            addr("tuya_cable", json!({"address": "x", "device_id": "d", "local_key_file": "k"})).contains("address")
        );
        let charger: TuyaEvChargerTable =
            serde_json::from_value(json!({"address": "a", "device_id": "d", "local_key_file": "k"})).unwrap();
        assert_eq!(charger.protocol, "3.5");
        let ev = |extra: serde_json::Value| {
            let mut t = json!({"address": "192.168.1.179", "device_id": "d", "local_key_file": "k"});
            t.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            get("tuya_evcharger").check(&t)
        };
        ev(json!({})).unwrap();
        ev(json!({"protocol": "3.4"})).unwrap();
        assert!(ev(json!({"protocol": "3.3"})).unwrap_err().to_string().contains("3.3"));
        assert!(ev(json!({"address": "x"})).unwrap_err().to_string().contains("address"));
        get("omnik").check(&json!({"base_url": "http://192.168.1.40"})).unwrap();
        get("energy_charts").check(&json!({})).unwrap();
    }
}
