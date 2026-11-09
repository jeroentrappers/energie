//! The clients as driver kinds of the plug-in contract (`energie_drivers::plugin`), so each can be
//! a `[[device]]` in the app's config:
//!
//! | Kind | Client | Slot | Writable | Poll |
//! | --- | --- | --- | --- | --- |
//! | `kia_connect` | [`Kia`] | `car:ev3` | yes (the AC charging limit, as an entity) | 60 s |
//! | `cupra_data_act` | [`Cupra`] | `car:buzz` | no | 60 s |
//! | `lg_thinq` | [`Thinq`] | `load:airco` | yes (`Command::Airco`) | 60 s |
//! | `fluvius` | [`Fluvius`] | `capacity_peak` | no | 60 s |
//! | `forecast_solar` | [`ForecastSolar`] | `solar_forecast` (entities only) | no | 60 s |
//!
//! The app polls every minute; the clients answer from their cache inside their own poll
//! interval (30 min for Kia, 15 min for the Data Act portal, 5 min for ThinQ, 6 h for Fluvius,
//! 1 h for forecast.solar) and refuse while backing off, which the app counts as a failed read.
//!
//! [`CloudDriver`] is the adapter: a [`CloudReading`] becomes the slot's typed `Reading` plus the
//! values the World has no field for as entities; a core `Command` becomes a [`CloudCommand`]
//! where one exists ([`CloudCommand::from_command`]), and the settable `ac_limit` entity becomes
//! [`CloudCommand::Ev3AcLimit`] (the logic has no command for it).

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use energie_core::command::Command;
use energie_drivers::driver::{Actuator, Reading, Source};
use energie_drivers::entity::{Control, Entity, StateClass, Value};
use energie_drivers::plugin::{BuildCx, Driver, DriverKind, Factory, Slot, factory, read_secret};
use energie_drivers::{Applied as DriverApplied, AuthStatus, DriverError};
use serde::Deserialize;

use crate::cupra::{Cupra, CupraConfig, CupraEndpoints};
use crate::fluvius::{Fluvius, FluviusConfig, FluviusEndpoints};
use crate::forecast_solar::{ForecastSolar, ForecastSolarConfig};
use crate::kia::{Kia, KiaConfig, KiaEndpoints};
use crate::lgthinq::{Thinq, ThinqConfig};
use crate::source::{Applied, CloudActuator, CloudCommand, CloudReading, CloudSource};
use crate::store::{Secret, StateFile};
use crate::{CloudError, Result};

/// Every kind in this crate, for the app's registry.
pub fn kinds() -> Vec<Box<dyn Factory>> {
    vec![
        factory::<KiaKind>(),
        factory::<CupraKind>(),
        factory::<ThinqKind>(),
        factory::<FluviusKind>(),
        factory::<ForecastSolarKind>(),
    ]
}

/// The app calls `read` this often; the clients throttle themselves.
const POLL_MS: u64 = 60_000;

fn driver_error(e: CloudError) -> DriverError {
    DriverError::Cloud(e.to_string())
}

/// The slot's typed reading (the forecast has none).
pub fn reading(r: &CloudReading) -> Reading {
    match r {
        CloudReading::Ev3(r) => Reading::Ev3(r.world()),
        CloudReading::Buzz(r) => Reading::Buzz(r.world()),
        CloudReading::Airco(r) => Reading::Airco(r.world()),
        CloudReading::Fluvius(r) => Reading::Fluvius(r.world()),
        CloudReading::Forecast(_) => Reading::Empty,
    }
}

fn time(t: Option<chrono::DateTime<chrono::Utc>>) -> Value {
    t.map_or(Value::Null, |t| Value::Text(t.to_rfc3339()))
}

fn stamp(key: &str, name: &str, t: Option<chrono::DateTime<chrono::Utc>>) -> Entity {
    let mut e = Entity::sensor(key, name, time(t));
    e.class = Some("timestamp".into());
    e
}

/// The EV3's AC charging limit as a settable entity (`number.ev3_ac_charging_limit`: 50..100 in
/// steps of 10). It has the key of the `car:ev3` field, so it stands in for that entity.
fn ac_limit(v: Option<f64>) -> Entity {
    let mut e = Entity::sensor("ac_limit", "AC-laadlimiet", v.into()).with_unit("", "%");
    e.control = Some(Control::Number { min: Some(50.0), max: Some(100.0), step: 10.0 });
    e
}

/// What a reading holds beyond the slot's World fields, as entities keyed within the device.
/// Each names the HA entity the client's integration shows it as (the client's
/// `entity_states` ids), so an HA bridge can take over that entity id.
pub fn extras(r: &CloudReading) -> Vec<Entity> {
    let ids: Vec<String> = r.entity_states().into_iter().map(|(id, _)| id).collect();
    let ha = |i: usize| ids[i].as_str();
    match r {
        CloudReading::Ev3(r) => vec![
            ac_limit(r.ac_limit).with_ha(ha(3)),
            Entity::sensor("charging", "Laadt", r.charging.into()).with_ha(ha(1)),
            Entity::sensor("dc_limit", "DC-laadlimiet", r.dc_limit.into()).with_unit("", "%").with_ha(ha(4)),
            stamp("last_updated", "Laatste update", r.last_updated).with_ha(ha(6)),
        ],
        CloudReading::Buzz(r) => vec![stamp("captured_at", "Laatste voertuigupdate", r.captured_at).with_ha(ha(3))],
        CloudReading::Airco(_) => Vec::new(),
        CloudReading::Fluvius(r) => {
            let mut out = vec![Entity::sensor(
                "peak_start",
                "Piekkwartier",
                r.latest.as_ref().map_or(Value::Null, |p| Value::Text(p.spike_start.to_rfc3339())),
            )];
            // One entity per calendar month: the last 12 months hold each month once, so the
            // keys stay stable while the year in the name moves on.
            out.extend(r.monthly_kw.iter().map(|(month, kw)| {
                let key = format!("month_{}_peak_kw", month.get(5..7).unwrap_or("00"));
                Entity::sensor(key, format!("Piek {month}"), Value::Number(*kw)).with_unit("power", "kW")
            }));
            out
        }
        CloudReading::Forecast(f) => {
            // A forecast is not a meter: no statistics for the kWh values (as HA's integration).
            let kwh = |key: &str, name: &str, v: Option<f64>| {
                Entity::sensor(key, name, v.into())
                    .with_unit("energy", "kWh")
                    .with_state_class(StateClass::NoStatistics)
            };
            vec![
                Entity::sensor("power_now_w", "Forecast nu", f.power_now_w.into())
                    .with_unit("power", "W")
                    .with_state_class(StateClass::Measurement)
                    .with_ha(ha(0)),
                kwh("energy_today_kwh", "Forecast vandaag", f.energy_today_kwh).with_ha(ha(1)),
                kwh("energy_today_remaining_kwh", "Forecast rest van vandaag", f.energy_today_remaining_kwh)
                    .with_ha(ha(2)),
                kwh("energy_tomorrow_kwh", "Forecast morgen", f.energy_tomorrow_kwh).with_ha(ha(3)),
                kwh("energy_current_hour_kwh", "Forecast dit uur", f.energy_current_hour_kwh).with_ha(ha(4)),
                kwh("energy_next_hour_kwh", "Forecast volgend uur", f.energy_next_hour_kwh).with_ha(ha(5)),
            ]
        }
    }
}

/// The forecast power as a value every quarter hour, from the quarter hour of the first point to
/// the last point: the last point at or before each instant (the `forecast_solar` library's
/// `power_production_at_time`), so a chart of 15-minute buckets has no holes.
pub fn quarter_steps(watts: &[(chrono::DateTime<chrono::Utc>, f64)]) -> Vec<(i64, f64)> {
    let (Some(first), Some(last)) = (watts.first(), watts.last()) else { return Vec::new() };
    let first_t = first.0.timestamp();
    let mut out = Vec::new();
    let mut i = 0;
    let mut t = first_t - first_t.rem_euclid(900);
    while t <= last.0.timestamp() {
        while i + 1 < watts.len() && watts[i + 1].0.timestamp() <= t {
            i += 1;
        }
        out.push((t, if watts[i].0.timestamp() <= t { watts[i].1 } else { 0.0 }));
        t += 900;
    }
    out
}

/// The cloud command behind setting entity `key` to `value`, if any.
pub fn set_command(key: &str, value: &Value) -> Option<CloudCommand> {
    match (key, value) {
        ("ac_limit", Value::Number(n)) if n.fract() == 0.0 && (0.0..=255.0).contains(n) => {
            Some(CloudCommand::Ev3AcLimit(*n as u8))
        }
        _ => None,
    }
}

fn applied(a: Applied) -> DriverApplied {
    match a {
        Applied::Done => DriverApplied::Done,
        Applied::Unchanged => DriverApplied::Unchanged,
        Applied::NotMine => DriverApplied::NotMine,
    }
}

/// A cloud client as a driver: [`CloudSource`] → [`Source`], [`CloudActuator`] → [`Actuator`].
pub struct CloudDriver<C> {
    client: C,
    /// The last good reading, for [`Source::entities`].
    last: Option<CloudReading>,
}

impl<C> CloudDriver<C> {
    pub fn new(client: C) -> Self {
        CloudDriver { client, last: None }
    }
}

#[async_trait]
impl<C: CloudSource + CloudActuator> Source for CloudDriver<C> {
    fn name(&self) -> &'static str {
        self.client.name()
    }

    async fn read(&mut self) -> energie_drivers::Result<Reading> {
        let r = self.client.read().await.map_err(driver_error)?;
        let out = reading(&r);
        self.last = Some(r);
        Ok(out)
    }

    fn unavailable(&self) -> Reading {
        reading(&self.client.unavailable())
    }

    fn entities(&self) -> Vec<Entity> {
        self.last.as_ref().map(extras).unwrap_or_default()
    }

    fn auth(&self) -> Option<AuthStatus> {
        self.client.auth()
    }

    async fn set_credentials(
        &mut self,
        fields: &serde_json::Map<String, serde_json::Value>,
    ) -> energie_drivers::Result<AuthStatus> {
        self.client.set_credentials(fields).await.map_err(|e| match e {
            CloudError::Config(m) => DriverError::Config(m),
            e => driver_error(e),
        })
    }

    /// The solar forecast's power (`power_w`), in quarter-hour steps.
    fn forecasts(&self) -> Vec<(String, Vec<(i64, f64)>)> {
        match &self.last {
            Some(CloudReading::Forecast(f)) => vec![("power_w".to_string(), quarter_steps(&f.watts))],
            _ => Vec::new(),
        }
    }
}

#[async_trait]
impl<C: CloudSource + CloudActuator> Actuator for CloudDriver<C> {
    async fn apply(&mut self, cmd: &Command) -> energie_drivers::Result<DriverApplied> {
        match CloudCommand::from_command(cmd) {
            Some(c) => self.client.apply(&c).await.map(applied).map_err(driver_error),
            None => Ok(DriverApplied::NotMine),
        }
    }

    async fn set(&mut self, key: &str, value: &Value) -> energie_drivers::Result<DriverApplied> {
        match set_command(key, value) {
            Some(c) => self.client.apply(&c).await.map(applied).map_err(driver_error),
            None => Ok(DriverApplied::NotMine),
        }
    }
}

/// The read-only clients take no commands.
macro_rules! read_only {
    ($($t:ty),*) => {$(
        #[async_trait]
        impl CloudActuator for $t {
            async fn apply(&mut self, _: &CloudCommand) -> Result<Applied> {
                Ok(Applied::NotMine)
            }
        }
    )*};
}
read_only!(Cupra, Fluvius, ForecastSolar);

/// A state dir for rotated tokens: refused under `/etc` (where the credentials stay as written).
fn state_file(dir: &Path, name: &str) -> energie_drivers::Result<StateFile> {
    StateFile::new(dir.join(name)).map_err(|e| DriverError::Config(e.to_string()))
}

/// Where the credentials captured for device `id` go (`<state_dir>/credentials/<id>.json`).
fn credentials_store(state_dir: &Path, id: &str) -> PathBuf {
    state_dir.join("credentials").join(format!("{id}.json"))
}

fn config_error(e: CloudError) -> DriverError {
    DriverError::Config(e.to_string())
}

fn boxed<C: CloudSource + CloudActuator + 'static>(c: Result<C>) -> energie_drivers::Result<Box<dyn Driver>> {
    Ok(Box::new(CloudDriver::new(c.map_err(config_error)?)))
}

fn kia_poll() -> u64 {
    crate::kia::MIN_POLL.as_secs()
}

/// `kind = "kia_connect"`: the EV3 through Kia Connect EU (the cached vehicle state; the car is
/// never woken).
pub struct KiaKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KiaTable {
    /// The HA config entry's `data` as JSON (`/etc/energie/cloud-kia.json`), read only.
    pub credentials_file: PathBuf,
    /// Where a refreshed token goes (`<state_dir>/kia-token.json`); not under `/etc`.
    pub state_dir: PathBuf,
    /// Nickname or VIN; the first vehicle on the account when absent.
    pub vehicle: Option<String>,
    /// Seconds between fetches, at least 1800.
    #[serde(default = "kia_poll")]
    pub poll_interval_s: u64,
    /// Service URLs (tests point them at a mock).
    #[serde(default)]
    pub endpoints: KiaEndpoints,
}

impl DriverKind for KiaKind {
    type Config = KiaTable;
    const NAME: &'static str = "kia_connect";
    const SLOTS: &'static [Slot] = &[Slot::CarEv3];
    const WRITABLE: bool = true;
    const POLL_MS: u64 = POLL_MS;

    fn check(c: &KiaTable) -> energie_drivers::Result<()> {
        state_file(&c.state_dir, "kia-token.json").map(drop)
    }

    fn build(c: KiaTable, cx: &BuildCx) -> energie_drivers::Result<Box<dyn Driver>> {
        let dir = cx.state_dir.unwrap_or(&c.state_dir);
        boxed(Kia::new(KiaConfig {
            credentials: c.credentials_file,
            credentials_store: Some(credentials_store(dir, cx.id)),
            state_file: c.state_dir.join("kia-token.json"),
            vehicle: c.vehicle,
            poll_interval_s: c.poll_interval_s,
            endpoints: c.endpoints,
        }))
    }
}

fn cupra_poll() -> u64 {
    15 * 60
}

/// `kind = "cupra_data_act"`: the ID.Buzz through the VW Group EU Data Act portal.
pub struct CupraKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CupraTable {
    /// The HA config entry's `data` as JSON (`/etc/energie/cloud-cupra.json`), read only. The
    /// portal login is redone when needed, so nothing is stored.
    pub credentials_file: PathBuf,
    /// Seconds between listings, at least 300.
    #[serde(default = "cupra_poll")]
    pub poll_interval_s: u64,
    #[serde(default)]
    pub endpoints: CupraEndpoints,
}

impl DriverKind for CupraKind {
    type Config = CupraTable;
    const NAME: &'static str = "cupra_data_act";
    const SLOTS: &'static [Slot] = &[Slot::CarBuzz];
    const POLL_MS: u64 = POLL_MS;

    fn build(c: CupraTable, cx: &BuildCx) -> energie_drivers::Result<Box<dyn Driver>> {
        boxed(Cupra::new(CupraConfig {
            credentials: c.credentials_file,
            credentials_store: cx.state_dir.map(|d| credentials_store(d, cx.id)),
            poll_interval_s: c.poll_interval_s,
            endpoints: c.endpoints,
        }))
    }
}

fn thinq_poll() -> u64 {
    300
}
fn beneden() -> String {
    "Airco beneden".into()
}
fn zolder() -> String {
    "Airco zolder".into()
}

/// `kind = "lg_thinq"`: the two aircos through LG ThinQ Connect (personal access token).
pub struct ThinqKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThinqTable {
    /// The HA config entry's `data` as JSON (`/etc/energie/cloud-lgthinq.json`), read only. The
    /// token does not rotate, so nothing is stored.
    pub credentials_file: PathBuf,
    /// Device aliases of the two units.
    #[serde(default = "beneden")]
    pub beneden: String,
    #[serde(default = "zolder")]
    pub zolder: String,
    /// Seconds between state polls, at least 60.
    #[serde(default = "thinq_poll")]
    pub poll_interval_s: u64,
    /// API base; from the country in the credentials when absent.
    pub base_url: Option<String>,
}

impl DriverKind for ThinqKind {
    type Config = ThinqTable;
    const NAME: &'static str = "lg_thinq";
    const SLOTS: &'static [Slot] = &[Slot::LoadAirco];
    const WRITABLE: bool = true;
    const POLL_MS: u64 = POLL_MS;

    fn build(c: ThinqTable, _: &BuildCx) -> energie_drivers::Result<Box<dyn Driver>> {
        boxed(Thinq::new(ThinqConfig {
            credentials: c.credentials_file,
            beneden: c.beneden,
            zolder: c.zolder,
            poll_interval_s: c.poll_interval_s,
            base_url: c.base_url,
        }))
    }
}

fn fluvius_poll() -> u64 {
    6 * 3600
}

/// `kind = "fluvius"`: the monthly capacity peaks from Mijn Fluvius.
pub struct FluviusKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FluviusTable {
    /// The config entries as a JSON array (`/etc/energie/cloud-fluvius.json`), read only.
    pub credentials_file: PathBuf,
    /// Where the rotating token set goes (`<state_dir>/fluvius-token.json`); not under `/etc`.
    pub state_dir: PathBuf,
    /// The EAN to read; the first electricity entry when absent.
    pub ean: Option<String>,
    /// Seconds between reads, at least 3600.
    #[serde(default = "fluvius_poll")]
    pub poll_interval_s: u64,
    #[serde(default)]
    pub endpoints: FluviusEndpoints,
}

impl DriverKind for FluviusKind {
    type Config = FluviusTable;
    const NAME: &'static str = "fluvius";
    const SLOTS: &'static [Slot] = &[Slot::CapacityPeak];
    const POLL_MS: u64 = POLL_MS;

    fn check(c: &FluviusTable) -> energie_drivers::Result<()> {
        state_file(&c.state_dir, "fluvius-token.json").map(drop)
    }

    fn build(c: FluviusTable, _: &BuildCx) -> energie_drivers::Result<Box<dyn Driver>> {
        boxed(Fluvius::new(FluviusConfig {
            credentials: c.credentials_file,
            state_file: c.state_dir.join("fluvius-token.json"),
            ean: c.ean,
            poll_interval_s: c.poll_interval_s,
            endpoints: c.endpoints,
        }))
    }
}

/// `kind = "forecast_solar"`: the PV forecast of one plane from forecast.solar. Without plane
/// keys it is one plane at the configured site (30° tilt, south, 8.8 kWp): set your own.
pub struct ForecastSolarKind;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForecastSolarTable {
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Tilt, degrees from horizontal.
    pub declination: Option<f64>,
    /// The API's convention: -180..180, 0 = south, -90 = east (HA's azimuth minus 180).
    pub azimuth: Option<f64>,
    pub kwp: Option<f64>,
    pub damping: Option<f64>,
    /// A file holding a personal API key (none on the public tier).
    pub api_key_file: Option<PathBuf>,
    pub base_url: Option<String>,
    /// Seconds between fetches (HA: 3600 without a key).
    pub poll_interval_s: Option<u64>,
}

impl ForecastSolarTable {
    fn config(&self, api_key: Option<Secret>) -> ForecastSolarConfig {
        let home = ForecastSolarConfig::home();
        ForecastSolarConfig {
            latitude: self.latitude.unwrap_or(home.latitude),
            longitude: self.longitude.unwrap_or(home.longitude),
            declination: self.declination.unwrap_or(home.declination),
            azimuth: self.azimuth.unwrap_or(home.azimuth),
            kwp: self.kwp.unwrap_or(home.kwp),
            damping: self.damping.unwrap_or(home.damping),
            api_key,
            base_url: self.base_url.clone().unwrap_or(home.base_url),
            poll_interval_s: self.poll_interval_s.unwrap_or(home.poll_interval_s),
        }
    }
}

impl DriverKind for ForecastSolarKind {
    type Config = ForecastSolarTable;
    const NAME: &'static str = "forecast_solar";
    const SLOTS: &'static [Slot] = &[Slot::SolarForecast];
    const POLL_MS: u64 = POLL_MS;

    fn check(c: &ForecastSolarTable) -> energie_drivers::Result<()> {
        c.config(None).estimate_url().map(drop).map_err(config_error)
    }

    fn build(c: ForecastSolarTable, _: &BuildCx) -> energie_drivers::Result<Box<dyn Driver>> {
        let key = c.api_key_file.as_deref().map(read_secret).transpose()?.map(Secret::new);
        boxed(ForecastSolar::new(c.config(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cupra::BuzzReading;
    use crate::fluvius::{FluviusReading, Peak};
    use crate::forecast_solar::SolarForecast;
    use crate::http::mock::{Mock, Reply};
    use crate::kia::Ev3Reading;
    use crate::lgthinq::AircoReading;
    use energie_core::command::AircoUnit;
    use serde_json::json;

    fn get(name: &str) -> Box<dyn Factory> {
        kinds().into_iter().find(|f| f.name() == name).unwrap()
    }

    fn write(dir: &Path, name: &str, text: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p.display().to_string()
    }

    /// The documented slots, writability and poll period of each kind.
    #[test]
    fn kinds_and_their_contract() {
        let table = [
            ("kia_connect", Slot::CarEv3, true),
            ("cupra_data_act", Slot::CarBuzz, false),
            ("lg_thinq", Slot::LoadAirco, true),
            ("fluvius", Slot::CapacityPeak, false),
            ("forecast_solar", Slot::SolarForecast, false),
        ];
        assert_eq!(kinds().len(), table.len());
        for (name, slot, writable) in table {
            let f = get(name);
            assert_eq!((f.slots(), f.writable(), f.poll_ms()), (&[slot][..], writable, 60_000), "{name}");
        }
    }

    /// The tables: defaults, unknown keys refused, state dirs under /etc refused at the check
    /// (no I/O), a bad forecast URL refused.
    #[test]
    fn tables_check_without_io() {
        let kia = json!({"credentials_file": "/etc/energie/cloud-kia.json", "state_dir": "/var/lib/energie"});
        get("kia_connect").check(&kia).unwrap();
        let t: KiaTable = serde_json::from_value(kia).unwrap();
        assert_eq!((t.poll_interval_s, t.vehicle), (1800, None));
        let etc = json!({"credentials_file": "/etc/energie/cloud-kia.json", "state_dir": "/etc/energie"});
        assert!(get("kia_connect").check(&etc).unwrap_err().to_string().contains("must not be under /etc"));
        assert!(get("fluvius").check(&etc).unwrap_err().to_string().contains("must not be under /etc"));
        let typo = json!({"credentials_file": "c", "state_dir": "/s", "vehicel": "EV3"});
        assert!(get("kia_connect").check(&typo).unwrap_err().to_string().contains("unknown field `vehicel`"));
        let f: FluviusTable = serde_json::from_value(json!({"credentials_file": "c", "state_dir": "/s"})).unwrap();
        assert_eq!(f.poll_interval_s, 6 * 3600);
        let c: CupraTable = serde_json::from_value(json!({"credentials_file": "c"})).unwrap();
        assert_eq!(c.poll_interval_s, 900);
        get("cupra_data_act").check(&json!({"credentials_file": "c"})).unwrap();
        let l: ThinqTable = serde_json::from_value(json!({"credentials_file": "c"})).unwrap();
        assert_eq!((l.beneden.as_str(), l.zolder.as_str(), l.poll_interval_s), ("Airco beneden", "Airco zolder", 300));
        get("forecast_solar").check(&json!({})).unwrap();
        assert!(get("forecast_solar").check(&json!({"base_url": "nope"})).unwrap_err().to_string().contains("config"));
        let plane = ForecastSolarTable { kwp: Some(4.0), azimuth: Some(-90.0), ..Default::default() }.config(None);
        let home = ForecastSolarConfig::home();
        assert_eq!((plane.kwp, plane.azimuth, plane.latitude), (4.0, -90.0, home.latitude));
        assert_eq!(ForecastSolarTable::default().config(None), home);
    }

    /// Every kind builds from its credentials file (no network); a missing file names it.
    #[tokio::test]
    async fn kinds_build_from_their_files() {
        let d = tempfile::tempdir().unwrap();
        let state = d.path().join("state").display().to_string();
        let kia = write(d.path(), "kia.json", r#"{"username":"u","password":"p","region":1,"brand":1}"#);
        let cupra = write(
            d.path(),
            "cupra.json",
            r#"{"brand":"volkswagen_commercial","email":"e","password":"p","vin":"V","identifier":"i"}"#,
        );
        let thinq = write(d.path(), "thinq.json", r#"{"access_token":"a","connect_client_id":"c","country":"BE"}"#);
        let fluvius =
            write(d.path(), "fluvius.json", r#"[{"email":"e","password":"p","ean":"541","meter_serial":"1SAG"}]"#);
        let key = write(d.path(), "fs-key", "made-up-key\n");
        let cases = [
            ("kia_connect", json!({"credentials_file": kia, "state_dir": state}), "kia"),
            ("cupra_data_act", json!({"credentials_file": cupra}), "cupra"),
            ("lg_thinq", json!({"credentials_file": thinq}), "lgthinq"),
            ("fluvius", json!({"credentials_file": fluvius, "state_dir": state}), "fluvius"),
            ("forecast_solar", json!({"api_key_file": key}), "forecast_solar"),
        ];
        for (kind, table, name) in cases {
            let f = get(kind);
            let d = f.build(&table, &BuildCx { id: "x", slots: f.slots(), state_dir: None }).unwrap();
            assert_eq!(Source::name(d.as_ref()), name);
            assert_eq!(d.unavailable(), f.slots()[0].unavailable(), "{kind}");
            assert!(d.entities().is_empty(), "no entities before the first read");
        }
        let missing = json!({"credentials_file": "/nonexistent/kia.json", "state_dir": state});
        let e = get("kia_connect")
            .build(&missing, &BuildCx { id: "x", slots: &[Slot::CarEv3], state_dir: None })
            .err()
            .unwrap();
        assert!(e.to_string().contains("/nonexistent/kia.json"), "{e}");
        let no_key = json!({"api_key_file": "/nonexistent/key"});
        let e = get("forecast_solar")
            .build(&no_key, &BuildCx { id: "x", slots: &[Slot::SolarForecast], state_dir: None })
            .err();
        assert!(e.unwrap().to_string().contains("/nonexistent/key"));
    }

    fn peak(month: &str) -> Peak {
        let t = chrono::DateTime::parse_from_rfc3339(&format!("{month}-04T18:15:00+01:00")).unwrap();
        Peak { period_start: t, period_end: t, spike_start: t, spike_end: t, kw: 3.892 }
    }

    /// Each reading becomes its slot's typed reading plus the values the World has no field for.
    #[test]
    fn readings_and_their_extras() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-07T13:20:00Z").unwrap().to_utc();
        let ev3 = CloudReading::Ev3(Ev3Reading {
            soc: Some(54.5),
            charging: Some(false),
            ac_limit: Some(80.0),
            dc_limit: Some(100.0),
            last_updated: Some(at),
            ..Ev3Reading::default()
        });
        let Reading::Ev3(w) = reading(&ev3) else { panic!() };
        assert_eq!((w.soc, w.ac_limit), (Some(54.5), Some(80.0)));
        let e = extras(&ev3);
        let keys: Vec<_> = e.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["ac_limit", "charging", "dc_limit", "last_updated"]);
        assert_eq!(e[0].control, Some(Control::Number { min: Some(50.0), max: Some(100.0), step: 10.0 }));
        assert_eq!((e[1].value.clone(), e[2].value.clone()), (Value::Bool(false), Value::Number(100.0)));
        assert_eq!(e[3].value, Value::Text("2026-11-07T13:20:00+00:00".into()));
        assert_eq!(e[3].class.as_deref(), Some("timestamp"));
        assert_eq!(extras(&CloudReading::Ev3(Ev3Reading::default()))[3].value, Value::Null);
        let ha: Vec<_> = e.iter().map(|e| e.ha_entity.as_deref().unwrap()).collect();
        assert_eq!(
            ha,
            [
                "number.ev3_ac_charging_limit",
                "binary_sensor.ev3_ev_battery_charge",
                "number.ev3_dc_charging_limit",
                "sensor.ev3_last_updated_at"
            ]
        );

        let buzz = CloudReading::Buzz(BuzzReading { soc: Some(43.0), captured_at: Some(at), ..Default::default() });
        assert!(matches!(reading(&buzz), Reading::Buzz(b) if b.soc == Some(43.0)));
        assert_eq!(extras(&buzz)[0].key, "captured_at");
        assert_eq!(extras(&buzz)[0].ha_entity.as_deref(), Some("sensor.de_buzz_laatste_voertuigupdate"));

        let airco = CloudReading::Airco(AircoReading { beneden_on: Some(true), zolder_on: Some(false) });
        assert!(matches!(reading(&airco), Reading::Airco(a) if a.beneden_on == Some(true)));
        assert!(extras(&airco).is_empty());

        let fl = CloudReading::Fluvius(FluviusReading {
            peak_kw: Some(3.892),
            latest: Some(peak("2026-11")),
            monthly_kw: vec![("2026-10".into(), 4.1), ("2026-11".into(), 3.892)],
            meter: "1sag".into(),
        });
        assert!(matches!(reading(&fl), Reading::Fluvius(f) if f.peak_kw == Some(3.892)));
        let e = extras(&fl);
        let ids: Vec<_> = e.iter().map(|e| (e.key.as_str(), e.name.as_str())).collect();
        assert_eq!(
            ids,
            [
                ("peak_start", "Piekkwartier"),
                ("month_10_peak_kw", "Piek 2026-10"),
                ("month_11_peak_kw", "Piek 2026-11")
            ]
        );
        assert_eq!((e[1].value.clone(), e[1].unit.as_deref()), (Value::Number(4.1), Some("kW")));
        assert_eq!(e[0].value, Value::Text("2026-11-04T18:15:00+01:00".into()));
        assert_eq!(extras(&CloudReading::Fluvius(FluviusReading::default()))[0].value, Value::Null);
        assert!(e.iter().all(|e| e.ha_entity.is_none()), "HA has no entity for these");

        let fc = CloudReading::Forecast(SolarForecast { power_now_w: Some(1200.0), ..Default::default() });
        assert_eq!(reading(&fc), Reading::Empty);
        let e = extras(&fc);
        assert_eq!(e.len(), 6);
        assert_eq!((e[0].key.as_str(), e[0].value.clone()), ("power_now_w", Value::Number(1200.0)));
        assert!(e[1..].iter().all(|e| e.unit.as_deref() == Some("kWh") && e.value == Value::Null));
        assert!(e[1..].iter().all(|e| e.state_class == Some(StateClass::NoStatistics)));
        assert_eq!(e[0].state_class, Some(StateClass::Measurement));
        let ha: Vec<_> = e.iter().map(|e| e.ha_entity.as_deref().unwrap()).collect();
        assert_eq!(
            ha,
            [
                "sensor.power_production_now",
                "sensor.energy_production_today",
                "sensor.energy_production_today_remaining",
                "sensor.energy_production_tomorrow",
                "sensor.energy_current_hour",
                "sensor.energy_next_hour"
            ]
        );
    }

    /// The forecast's points become quarter-hour steps: 0 before the first point, each point's
    /// value until the next one.
    #[test]
    fn forecast_power_in_quarter_hours() {
        let at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let watts = vec![
            (at("2026-11-08T07:41:00Z"), 0.0),
            (at("2026-11-08T08:00:00Z"), 400.0),
            (at("2026-11-08T09:00:00Z"), 1200.0),
            (at("2026-11-08T09:20:00Z"), 0.0),
        ];
        let q = quarter_steps(&watts);
        let t0 = at("2026-11-08T07:30:00Z").timestamp();
        let v: Vec<f64> = q.iter().map(|(_, v)| *v).collect();
        assert_eq!(q[0].0, t0);
        assert!(q.windows(2).all(|w| w[1].0 - w[0].0 == 900));
        assert_eq!(v, [0.0, 0.0, 400.0, 400.0, 400.0, 400.0, 1200.0, 1200.0]);
        assert!(quarter_steps(&[]).is_empty());
    }

    /// Only a whole-number `ac_limit` is a cloud command.
    #[test]
    fn settable_entities_become_cloud_commands() {
        assert_eq!(set_command("ac_limit", &Value::Number(80.0)), Some(CloudCommand::Ev3AcLimit(80)));
        assert_eq!(set_command("ac_limit", &Value::Number(80.5)), None);
        assert_eq!(set_command("ac_limit", &Value::Number(300.0)), None);
        assert_eq!(set_command("ac_limit", &Value::Bool(true)), None);
        assert_eq!(set_command("dc_limit", &Value::Number(80.0)), None);
        assert_eq!(applied(Applied::Unchanged), DriverApplied::Unchanged);
    }

    /// The read-only clients answer every command with `NotMine`.
    #[tokio::test]
    async fn read_only_clients_take_no_commands() {
        let d = tempfile::tempdir().unwrap();
        let cmd = CloudCommand::Ev3AcLimit(80);
        let mut fs = ForecastSolar::new(ForecastSolarConfig::home()).unwrap();
        assert_eq!(fs.apply(&cmd).await.unwrap(), Applied::NotMine);
        let cupra = write(d.path(), "c.json", r#"{"email":"e","password":"p","vin":"V","identifier":"i"}"#);
        let mut c = Cupra::new(CupraConfig {
            credentials: cupra.into(),
            credentials_store: None,
            poll_interval_s: 900,
            endpoints: CupraEndpoints::default(),
        })
        .unwrap();
        assert_eq!(c.apply(&cmd).await.unwrap(), Applied::NotMine);
        let fl = write(d.path(), "f.json", r#"[{"email":"e","password":"p","ean":"541","meter_serial":"1SAG"}]"#);
        let mut f = Fluvius::new(FluviusConfig {
            credentials: fl.into(),
            state_file: d.path().join("fluvius-token.json"),
            ean: None,
            poll_interval_s: 3600,
            endpoints: FluviusEndpoints::default(),
        })
        .unwrap();
        assert_eq!(f.apply(&cmd).await.unwrap(), Applied::NotMine);
    }

    const VEHICLES: &str = r#"{"retCode":"S","resCode":"0000","resMsg":{"vehicles":[
        {"vehicleId":"veh-ev3","nickname":"EV3","vehicleName":"EV3","regDate":"2025-05-01","vin":"VINPLACEHOLDER001","type":"EV","ccuCCS2ProtocolSupport":1}]}}"#;

    /// `kia_connect` end to end against a mock: the EV3's reading and extras, the AC limit set
    /// through the entity (keeping the DC limit), the controller's commands not taken, and a
    /// failed read as a driver error.
    #[tokio::test]
    async fn kia_connect_reads_and_sets_the_ac_limit() {
        let (mock, script, log) = Mock::scripted().await;
        script.set("GET /api/v1/spa/vehicles", Reply::json(200, VEHICLES));
        script.set(
            "GET /api/v1/spa/vehicles/veh-ev3/ccs2/carstatus/latest",
            Reply::json(200, include_str!("../fixtures/kia_ccs2_status.json")),
        );
        script.set(
            "POST /api/v1/spa/vehicles/veh-ev3/charge/target",
            Reply::json(200, r#"{"retCode":"S","resCode":"0000"}"#),
        );
        let d = tempfile::tempdir().unwrap();
        let creds = json!({"username": "u", "password": "p", "region": 1, "brand": 1, "token": {
            "access_token": "Bearer made-up", "refresh_token": "r", "device_id": "dev-1",
            "valid_until": "2099-01-01T00:00:00+00:00"}});
        let table = json!({
            "credentials_file": write(d.path(), "kia.json", &creds.to_string()),
            "state_dir": d.path().join("state"),
            "poll_interval_s": 1800,
            "endpoints": {"api_base": mock.base, "login_host": mock.base, "cci_api": mock.base},
        });
        let mut drv =
            get("kia_connect").build(&table, &BuildCx { id: "ev3", slots: &[Slot::CarEv3], state_dir: None }).unwrap();
        let Reading::Ev3(w) = drv.read().await.unwrap() else { panic!() };
        assert_eq!(w.ac_limit, Some(100.0));
        let e = drv.entities();
        assert_eq!((e[0].key.as_str(), e[0].value.clone()), ("ac_limit", Value::Number(100.0)));
        assert_eq!(drv.set("ac_limit", &Value::Number(80.0)).await.unwrap(), DriverApplied::Done);
        let (_, body) = log.last("POST /api/v1/spa/vehicles/veh-ev3/charge/target");
        assert!(body.contains("\"targetSOClevel\":80") && body.contains("\"targetSOClevel\":100"), "{body}");
        assert_eq!(drv.set("ac_limit", &Value::Number(80.0)).await.unwrap(), DriverApplied::Unchanged);
        assert_eq!(drv.set("charging", &Value::Bool(true)).await.unwrap(), DriverApplied::NotMine);
        let airco = Command::Airco { unit: AircoUnit::Zolder, on: true };
        assert_eq!(drv.apply(&airco).await.unwrap(), DriverApplied::NotMine);
        assert_eq!(drv.apply(&Command::RefreshPrices).await.unwrap(), DriverApplied::NotMine);
        let e = drv.set("ac_limit", &Value::Number(45.0)).await.unwrap_err();
        assert!(matches!(e, DriverError::Cloud(m) if m.contains("out of range")));
        // A Kia `poll_interval_s` below 30 min is raised, so the cache answers; a new driver
        // against a broken service fails its read.
        script.set("GET /api/v1/spa/vehicles", Reply::json(500, "{}"));
        let mut broken =
            get("kia_connect").build(&table, &BuildCx { id: "ev3", slots: &[Slot::CarEv3], state_dir: None }).unwrap();
        assert!(matches!(broken.read().await, Err(DriverError::Cloud(_))));
        assert!(broken.entities().is_empty());
        assert!(drv.forecasts().is_empty(), "only the solar forecast has one");

        // Sign-in: the store goes to `<state_dir>/credentials/<id>.json` (the app's state dir
        // when given); bad fields are a config error, a store that cannot be written a cloud one.
        assert_eq!(drv.auth().unwrap().credentials, "etc");
        let fields = |v: serde_json::Value| v.as_object().unwrap().clone();
        let e = drv.set_credentials(&fields(json!({"username": "u"}))).await.unwrap_err();
        assert!(matches!(e, DriverError::Config(m) if m.contains("password")));
        let blocked = d.path().join("blocked");
        let cx = BuildCx { id: "ev3", slots: &[Slot::CarEv3], state_dir: Some(&blocked) };
        let mut drv2 = get("kia_connect").build(&table, &cx).unwrap();
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join("credentials"), "a file where the directory goes").unwrap();
        let e = drv2.set_credentials(&fields(json!({"username": "u", "password": "p"}))).await.unwrap_err();
        assert!(matches!(&e, DriverError::Cloud(m) if m.starts_with("io:")), "{e}");
        let app_dir = d.path().join("app");
        let cx = BuildCx { id: "ev3", slots: &[Slot::CarEv3], state_dir: Some(&app_dir) };
        let mut drv3 = get("kia_connect").build(&table, &cx).unwrap();
        let st = drv3.set_credentials(&fields(json!({"username": "u", "password": "p"}))).await.unwrap();
        assert_eq!(st.credentials, "state");
        assert!(app_dir.join("credentials/ev3.json").exists());
    }

    /// `lg_thinq` end to end against a mock: both units' power as `Reading::Airco`, and the
    /// controller's `Command::Airco` carried out on the right unit.
    #[tokio::test]
    async fn lg_thinq_reads_and_switches_the_aircos() {
        let (mock, script, log) = Mock::scripted().await;
        script.set(
            "GET /devices",
            Reply::json(
                200,
                r#"{"response":[{"deviceId":"dev-z","deviceInfo":{"deviceType":"DEVICE_AIR_CONDITIONER","alias":"Airco zolder"}},
                    {"deviceId":"dev-b","deviceInfo":{"deviceType":"DEVICE_AIR_CONDITIONER","alias":"Airco beneden"}}]}"#,
            ),
        );
        let state = |m: &str| json!({"response": {"operation": {"airConOperationMode": m}}}).to_string();
        script.set("GET /devices/dev-b/state", Reply::json(200, state("POWER_OFF")));
        script.set("GET /devices/dev-z/state", Reply::json(200, state("POWER_ON")));
        script.set("POST /devices/dev-b/control", Reply::json(200, r#"{"response":{}}"#));
        let d = tempfile::tempdir().unwrap();
        let creds = r#"{"access_token":"made-up","connect_client_id":"c","country":"BE"}"#;
        let table = json!({"credentials_file": write(d.path(), "lg.json", creds), "base_url": mock.base});
        let mut drv = get("lg_thinq")
            .build(&table, &BuildCx { id: "airco", slots: &[Slot::LoadAirco], state_dir: None })
            .unwrap();
        let r = drv.read().await.unwrap();
        assert_eq!(r, Reading::Airco(energie_core::world::Airco { beneden_on: Some(false), zolder_on: Some(true) }));
        assert!(drv.entities().is_empty());
        let on = Command::Airco { unit: AircoUnit::Beneden, on: true };
        assert_eq!(drv.apply(&on).await.unwrap(), DriverApplied::Done);
        assert!(log.last("POST /devices/dev-b/control").1.contains("POWER_ON"));
        let off = Command::Airco { unit: AircoUnit::Zolder, on: true };
        assert_eq!(drv.apply(&off).await.unwrap(), DriverApplied::Unchanged);
        assert_eq!(drv.set("ac_limit", &Value::Number(80.0)).await.unwrap(), DriverApplied::NotMine);
    }
}
