//! LG ThinQ Connect, the official API (`lg_thinq` integration with `thinqconnect`), the two
//! airco units: `switch.airco_beneden_power` and `switch.airco_zolder_power`.
//!
//! REST on `https://api-{kic|aic|eic}.lgthinq.com` (Belgium: `eic`), authenticated with the
//! personal access token (PAT) from the config entry. Every request carries
//! `Authorization: Bearer <PAT>`, `x-country`, `x-client-id` (`connect_client_id`), a fresh
//! `x-message-id`, the public `x-api-key` of the SDK and `x-service-phase: OP`. Responses are
//! `{"messageId", "timestamp", "response": …}`; errors `{"error": {"code", "message"}}`.
//!
//! * `GET /devices`: the device list, looked up once (aliases `Airco beneden` / `Airco zolder`).
//! * `GET /devices/{id}/state`: `operation.airConOperationMode` is `POWER_ON` / `POWER_OFF`.
//! * `POST /devices/{id}/control` with `x-conditional-control: true` and
//!   `{"operation": {"airConOperationMode": "POWER_ON"}}`. Like HA, `2302` ("not supported in
//!   this state") on power-on and `2304` ("not supported when off") on power-off count as done.
//!
//! The PAT does not expire or rotate, so nothing is persisted. HA gets pushes over MQTT; this
//! client polls instead (every 5 minutes by default, at least every minute), and after a command
//! keeps the commanded state until the next poll.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use energie_core::command::AircoUnit;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{CloudError, Result};
use crate::http;
use crate::poll::{Gate, Poller};
use crate::source::{
    Applied, CloudActuator, CloudCommand, CloudReading, CloudSource, EntityDescriptor, EntityKind, EntityState, flag,
};
use crate::store::{Secret, load_json};

/// The ThinQ Connect SDK's public API key.
const API_KEY: &str = "v6GFvkweNo7DK7yD3ylIZ9w52aKBU0eJ7wLXkSR3";

/// Floor of the poll interval.
pub const MIN_POLL: Duration = Duration::from_secs(60);

const KIC: &str = "AU BD CN HK ID IN JP KH KR LA LK MM MY NP NZ PH SG TH TW VN";
const AIC: &str =
    "AG AR AW BB BO BR BS BZ CA CL CO CR CU DM DO EC GD GT GY HN HT JM KN LC MX NI PA PE PR PY SR SV TT US UY VC VE";
const EIC: &str = "AE AF AL AM AO AT AZ BA BE BF BG BH BJ BY CD CF CG CH CI CM CV CY CZ DE DJ DK DZ EE EG ES ET FI FR GA GB GE GH GM GN GQ GR HR HU IE IL IQ IR IS IT JO KE KG KW KZ LB LR LT LU LV LY MA MD ME MK ML MR MT MU MW NE NG NL NO OM PK PL PS PT QA RO RS RU RW SA SD SE SI SK SL SN SO ST SY TD TG TN TR TZ UA UG UZ XK YE ZA ZM";

/// The API region of a country (`thinqconnect.country.get_region_from_country`).
pub fn region(country: &str) -> Option<&'static str> {
    [("kic", KIC), ("aic", AIC), ("eic", EIC)]
        .into_iter()
        .find(|(_, list)| list.split(' ').any(|c| c == country))
        .map(|(r, _)| r)
}

/// The HA config entry's `data`.
#[derive(Debug, Clone, Deserialize)]
pub struct ThinqCredentials {
    pub access_token: Secret,
    pub connect_client_id: Secret,
    pub country: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ThinqConfig {
    /// HA config-entry `data` as JSON (`/etc/energie/cloud-lgthinq.json`).
    pub credentials: PathBuf,
    /// Device alias of `switch.airco_beneden_power`.
    #[serde(default = "default_beneden")]
    pub beneden: String,
    /// Device alias of `switch.airco_zolder_power`.
    #[serde(default = "default_zolder")]
    pub zolder: String,
    /// Seconds between state polls; raised to [`MIN_POLL`] when lower.
    #[serde(default = "default_poll")]
    pub poll_interval_s: u64,
    /// API base; derived from the country when absent.
    #[serde(default)]
    pub base_url: Option<String>,
}

fn default_beneden() -> String {
    "Airco beneden".into()
}

fn default_zolder() -> String {
    "Airco zolder".into()
}

fn default_poll() -> u64 {
    300
}

/// The two units' power state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AircoReading {
    /// `switch.airco_beneden_power`.
    pub beneden_on: Option<bool>,
    /// `switch.airco_zolder_power`.
    pub zolder_on: Option<bool>,
}

const BENEDEN: &str = "switch.airco_beneden_power";
const ZOLDER: &str = "switch.airco_zolder_power";

impl AircoReading {
    pub fn world(&self) -> energie_core::world::Airco {
        energie_core::world::Airco { beneden_on: self.beneden_on, zolder_on: self.zolder_on }
    }

    pub fn entity_states(&self) -> Vec<EntityState> {
        vec![flag(BENEDEN, self.beneden_on), flag(ZOLDER, self.zolder_on)]
    }

    /// The power state of one unit.
    pub fn unit(&self, unit: AircoUnit) -> Option<bool> {
        match unit {
            AircoUnit::Beneden => self.beneden_on,
            AircoUnit::Zolder => self.zolder_on,
        }
    }

    fn slot(&mut self, unit: AircoUnit) -> &mut Option<bool> {
        match unit {
            AircoUnit::Beneden => &mut self.beneden_on,
            AircoUnit::Zolder => &mut self.zolder_on,
        }
    }
}

/// `operation.airConOperationMode` of a state response.
pub fn parse_power(state: &Value) -> Option<bool> {
    match state["operation"]["airConOperationMode"].as_str()? {
        "POWER_ON" => Some(true),
        "POWER_OFF" => Some(false),
        _ => None,
    }
}

/// A ThinQ error code as a [`CloudError`].
fn api_error(status: u16, body: &str) -> CloudError {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let code = v["error"]["code"].as_str().unwrap_or_default().to_string();
    let message = v["error"]["message"].as_str().unwrap_or_default().to_string();
    match code.as_str() {
        "1103" | "1218" | "1302" | "1303" => CloudError::Unauthorized(format!("{code} {message}")),
        "1306" => CloudError::RateLimited(message),
        "" => CloudError::Status(status),
        _ => CloudError::Api { code, message },
    }
}

/// Cloud client for the two airco units.
pub struct Thinq {
    base: String,
    creds: ThinqCredentials,
    aliases: [(AircoUnit, String); 2],
    client: reqwest::Client,
    devices: Option<[String; 2]>,
    poller: Poller<AircoReading>,
}

impl Thinq {
    pub fn new(cfg: ThinqConfig) -> Result<Self> {
        let creds: ThinqCredentials = load_json(&cfg.credentials)?;
        Thinq::with_credentials(cfg, creds)
    }

    pub fn with_credentials(cfg: ThinqConfig, creds: ThinqCredentials) -> Result<Self> {
        let base = match cfg.base_url {
            Some(b) => b,
            None => {
                let r = region(&creds.country)
                    .ok_or_else(|| CloudError::Config(format!("country {:?} not supported", creds.country)))?;
                format!("https://api-{r}.lgthinq.com")
            }
        };
        http::parse_url(&base)?;
        let interval = Duration::from_secs(cfg.poll_interval_s).max(MIN_POLL);
        Ok(Thinq {
            base: base.trim_end_matches('/').to_string(),
            creds,
            aliases: [(AircoUnit::Beneden, cfg.beneden), (AircoUnit::Zolder, cfg.zolder)],
            client: http::client(Duration::from_secs(15), None, true)?,
            devices: None,
            poller: Poller::new(interval, Duration::from_secs(60), Duration::from_secs(3600)),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub fn poll_interval(&self) -> Duration {
        self.poller.interval()
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(rand::random::<[u8; 16]>());
        self.client
            .request(method, format!("{}/{path}", self.base))
            .bearer_auth(self.creds.access_token.expose())
            .header("x-country", &self.creds.country)
            .header("x-message-id", id)
            .header("x-client-id", self.creds.connect_client_id.expose())
            .header("x-api-key", API_KEY)
            .header("x-service-phase", "OP")
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value> {
        let resp = req.send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(api_error(status.as_u16(), &body));
        }
        let v: Value = serde_json::from_str(&body)?;
        Ok(v["response"].clone())
    }

    /// The device ids of the two units, by alias (case-insensitive), looked up once.
    pub async fn devices(&mut self) -> Result<[String; 2]> {
        if let Some(d) = &self.devices {
            return Ok(d.clone());
        }
        let list = self.send(self.request(reqwest::Method::GET, "devices")).await?;
        let list = list.as_array().cloned().unwrap_or_default();
        let find = |alias: &str| {
            list.iter()
                .find(|d| d["deviceInfo"]["alias"].as_str().is_some_and(|a| a.eq_ignore_ascii_case(alias)))
                .and_then(|d| d["deviceId"].as_str())
                .map(str::to_string)
                .ok_or_else(|| CloudError::NotReady(format!("no ThinQ device with alias {alias:?}")))
        };
        let ids = [find(&self.aliases[0].1)?, find(&self.aliases[1].1)?];
        self.devices = Some(ids.clone());
        Ok(ids)
    }

    fn device_of(ids: &[String; 2], unit: AircoUnit) -> &str {
        match unit {
            AircoUnit::Beneden => &ids[0],
            AircoUnit::Zolder => &ids[1],
        }
    }

    /// One poll of both units (no rate limit; [`CloudSource::read`] has one). A device the
    /// account no longer has drops the cached device list.
    pub async fn states(&mut self) -> Result<AircoReading> {
        let ids = self.devices().await?;
        let mut r = AircoReading::default();
        for (unit, _) in self.aliases.clone() {
            let path = format!("devices/{}/state", Self::device_of(&ids, unit));
            match self.send(self.request(reqwest::Method::GET, &path)).await {
                Ok(state) => *r.slot(unit) = parse_power(&state),
                Err(e @ CloudError::Api { .. }) => {
                    self.devices = None;
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(r)
    }

    /// `switch.airco_*_power` on or off.
    pub async fn set_power(&mut self, unit: AircoUnit, on: bool) -> Result<Applied> {
        if self.poller.last().is_some_and(|r| r.unit(unit) == Some(on)) {
            return Ok(Applied::Unchanged);
        }
        let ids = self.devices().await?;
        let path = format!("devices/{}/control", Self::device_of(&ids, unit));
        let mode = if on { "POWER_ON" } else { "POWER_OFF" };
        let req = self
            .request(reqwest::Method::POST, &path)
            .header("x-conditional-control", "true")
            .json(&json!({"operation": {"airConOperationMode": mode}}));
        match self.send(req).await {
            Ok(_) => {}
            Err(CloudError::Api { code, .. }) if (on && code == "2302") || (!on && code == "2304") => {}
            Err(e) => return Err(e),
        }
        self.poller.update(|r| *r.slot(unit) = Some(on));
        Ok(Applied::Done)
    }
}

#[async_trait]
impl CloudSource for Thinq {
    fn name(&self) -> &'static str {
        "lgthinq"
    }

    fn entities(&self) -> Vec<EntityDescriptor> {
        vec![
            EntityDescriptor::new(BENEDEN, EntityKind::Switch, None).feeds("airco.beneden_on").writable(),
            EntityDescriptor::new(ZOLDER, EntityKind::Switch, None).feeds("airco.zolder_on").writable(),
        ]
    }

    async fn read(&mut self) -> Result<CloudReading> {
        match self.poller.gate(Instant::now()) {
            Gate::Cached(r) => return Ok(CloudReading::Airco(r.clone())),
            Gate::Wait(d) => return Err(CloudError::Backoff(d)),
            Gate::Fetch => {}
        }
        let outcome = self.states().await;
        self.poller.record(Instant::now(), outcome).map(CloudReading::Airco)
    }

    fn unavailable(&self) -> CloudReading {
        CloudReading::Airco(AircoReading::default())
    }
}

#[async_trait]
impl CloudActuator for Thinq {
    async fn apply(&mut self, cmd: &CloudCommand) -> Result<Applied> {
        match cmd {
            CloudCommand::Airco { unit, on } => self.set_power(*unit, *on).await,
            _ => Ok(Applied::NotMine),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::{Log, Mock, Reply, Script};

    const DEVICES: &str = r#"{"messageId":"m","timestamp":"2026-11-07T12:00:00.000000","response":[
        {"deviceId":"dev-fridge","deviceInfo":{"deviceType":"DEVICE_REFRIGERATOR","modelName":"F","alias":"Koelkast","reportable":true}},
        {"deviceId":"dev-zolder","deviceInfo":{"deviceType":"DEVICE_AIR_CONDITIONER","modelName":"RAC","alias":"Airco zolder","reportable":true}},
        {"deviceId":"dev-beneden","deviceInfo":{"deviceType":"DEVICE_AIR_CONDITIONER","modelName":"RAC","alias":"airco Beneden","reportable":true}}]}"#;

    fn state(mode: &str) -> String {
        json!({"messageId": "m", "response": {
            "runState": {"currentState": "NORMAL"},
            "airConJobMode": {"currentJobMode": "COOL"},
            "operation": {"airConOperationMode": mode, "airCleanOperationMode": "STOP"},
            "temperature": {"currentTemperature": 21.5, "targetTemperature": 22, "unit": "C"},
            "powerSave": {"powerSaveEnabled": false}
        }})
        .to_string()
    }

    fn creds() -> ThinqCredentials {
        ThinqCredentials {
            access_token: "thinqpat_placeholder".into(),
            connect_client_id: "client-placeholder".into(),
            country: "BE".into(),
        }
    }

    async fn rig() -> (Mock, Script, Log, Thinq) {
        let (m, s, l) = Mock::scripted().await;
        s.set("GET /devices", Reply::json(200, DEVICES));
        s.set("GET /devices/dev-beneden/state", Reply::json(200, state("POWER_OFF")));
        s.set("GET /devices/dev-zolder/state", Reply::json(200, state("POWER_ON")));
        s.set("POST /devices/dev-beneden/control", Reply::json(200, r#"{"messageId":"m","response":{}}"#));
        s.set("POST /devices/dev-zolder/control", Reply::json(200, r#"{"messageId":"m","response":{}}"#));
        let cfg = ThinqConfig {
            credentials: "/nonexistent".into(),
            beneden: default_beneden(),
            zolder: default_zolder(),
            poll_interval_s: 300,
            base_url: Some(m.base.clone()),
        };
        let t = Thinq::with_credentials(cfg, creds()).unwrap();
        (m, s, l, t)
    }

    #[test]
    fn regions_and_config() {
        assert_eq!(region("BE"), Some("eic"));
        assert_eq!((region("US"), region("KR"), region("XX")), (Some("aic"), Some("kic"), None));
        let cfg: ThinqConfig =
            serde_json::from_value(json!({"credentials": "/etc/energie/cloud-lgthinq.json"})).unwrap();
        assert_eq!(
            (cfg.beneden.as_str(), cfg.zolder.as_str(), cfg.poll_interval_s),
            ("Airco beneden", "Airco zolder", 300)
        );
        assert!(matches!(Thinq::new(cfg.clone()), Err(CloudError::Config(_))));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-lgthinq.json");
        std::fs::write(&path, r#"{"access_token":"a","connect_client_id":"c","country":"BE"}"#).unwrap();
        assert!(Thinq::new(ThinqConfig { credentials: path, ..cfg.clone() }).is_ok());
        let t = Thinq::with_credentials(ThinqConfig { poll_interval_s: 1, ..cfg.clone() }, creds()).unwrap();
        assert_eq!(t.base_url(), "https://api-eic.lgthinq.com");
        assert_eq!(t.poll_interval(), MIN_POLL);
        let c = ThinqCredentials { country: "XX".into(), ..creds() };
        assert!(matches!(Thinq::with_credentials(cfg.clone(), c), Err(CloudError::Config(_))));
        let bad = ThinqConfig { base_url: Some("nope".into()), ..cfg };
        assert!(matches!(Thinq::with_credentials(bad, creds()), Err(CloudError::Config(_))));
        assert!(!format!("{:?}", creds()).contains("thinqpat"));
    }

    #[test]
    fn power_and_errors() {
        assert_eq!(parse_power(&json!({"operation": {"airConOperationMode": "POWER_ON"}})), Some(true));
        assert_eq!(parse_power(&json!({"operation": {"airConOperationMode": "POWER_OFF"}})), Some(false));
        assert_eq!(parse_power(&json!({"operation": {"airConOperationMode": "WHAT"}})), None);
        assert_eq!(parse_power(&json!({})), None);
        let e = |code: &str| api_error(400, &json!({"error": {"code": code, "message": "m"}}).to_string());
        assert!(matches!(e("1103"), CloudError::Unauthorized(_)));
        assert!(matches!(e("1306"), CloudError::RateLimited(_)));
        assert!(matches!(e("2207"), CloudError::Api { code, .. } if code == "2207"));
        assert!(matches!(api_error(502, "<html>"), CloudError::Status(502)));
    }

    /// End to end: device list once, both states per poll, the SDK's headers, one poll per
    /// interval.
    #[tokio::test]
    async fn reads_both_units() {
        let (_m, _s, log, mut t) = rig().await;
        assert_eq!(t.name(), "lgthinq");
        let CloudReading::Airco(r) = t.read().await.unwrap() else { panic!() };
        assert_eq!(r, AircoReading { beneden_on: Some(false), zolder_on: Some(true) });
        assert_eq!(r.world(), energie_core::world::Airco { beneden_on: Some(false), zolder_on: Some(true) });
        assert_eq!(r.entity_states()[1], (ZOLDER.into(), Some(crate::EntityValue::Bool(true))));
        t.read().await.unwrap();
        assert_eq!(log.count("GET /devices/dev-beneden/state"), 1);
        t.states().await.unwrap();
        assert_eq!(log.count("GET /devices"), 5);
        let (h, _) = log.last("GET /devices/dev-zolder/state");
        assert_eq!(h["authorization"], "Bearer thinqpat_placeholder");
        assert_eq!(
            (h["x-country"].to_str().unwrap(), h["x-client-id"].to_str().unwrap()),
            ("BE", "client-placeholder")
        );
        assert_eq!((h["x-api-key"].to_str().unwrap(), h["x-service-phase"].to_str().unwrap()), (API_KEY, "OP"));
        assert_eq!(h["x-message-id"].len(), 22);
        assert_eq!(t.entities().iter().filter(|e| e.writable).count(), 2);
        assert_eq!(t.unavailable(), CloudReading::Airco(AircoReading::default()));
    }

    /// Commands: sent with `x-conditional-control`, skipped when the state is known, HA's
    /// tolerated error codes, other errors passed on.
    #[tokio::test]
    async fn switches_units() {
        let (_m, s, log, mut t) = rig().await;
        assert_eq!(t.apply(&CloudCommand::Airco { unit: AircoUnit::Beneden, on: true }).await.unwrap(), Applied::Done);
        let (h, body) = log.last("POST /devices/dev-beneden/control");
        assert_eq!(h["x-conditional-control"], "true");
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({"operation": {"airConOperationMode": "POWER_ON"}})
        );
        t.read().await.unwrap();
        assert_eq!(t.set_power(AircoUnit::Zolder, true).await.unwrap(), Applied::Unchanged);
        assert_eq!(t.set_power(AircoUnit::Beneden, true).await.unwrap(), Applied::Done);
        let CloudReading::Airco(r) = t.read().await.unwrap() else { panic!() };
        assert_eq!(r.beneden_on, Some(true), "commanded state kept until the next poll");
        s.once(
            "POST /devices/dev-zolder/control",
            Reply::json(400, r#"{"error":{"code":"2304","message":"Command not supported in power off"}}"#),
        );
        assert_eq!(t.set_power(AircoUnit::Zolder, false).await.unwrap(), Applied::Done);
        s.once("POST /devices/dev-zolder/control", Reply::json(400, r#"{"error":{"code":"2302","message":"x"}}"#));
        assert_eq!(t.set_power(AircoUnit::Zolder, true).await.unwrap(), Applied::Done);
        s.once("POST /devices/dev-beneden/control", Reply::json(400, r#"{"error":{"code":"2302","message":"x"}}"#));
        assert!(matches!(t.set_power(AircoUnit::Beneden, false).await, Err(CloudError::Api { .. })));
        assert_eq!(t.apply(&CloudCommand::Ev3AcLimit(80)).await.unwrap(), Applied::NotMine);
    }

    /// Unknown aliases, vanished devices, bad tokens and broken bodies.
    #[tokio::test]
    async fn failures() {
        let (m, s, _log, mut t) = rig().await;
        s.once(
            "GET /devices/dev-beneden/state",
            Reply::json(400, r#"{"error":{"code":"1205","message":"Not exist device"}}"#),
        );
        assert!(matches!(t.states().await, Err(CloudError::Api { .. })));
        assert!(t.devices.is_none(), "device list dropped");
        s.once(
            "GET /devices/dev-zolder/state",
            Reply::json(401, r#"{"error":{"code":"1103","message":"Invalid token"}}"#),
        );
        assert!(matches!(t.read().await, Err(CloudError::Unauthorized(_))));
        assert!(matches!(t.read().await, Err(CloudError::Backoff(d)) if d >= Duration::from_secs(3000)));
        let cfg = ThinqConfig {
            credentials: "/nonexistent".into(),
            beneden: "Kelder".into(),
            zolder: default_zolder(),
            poll_interval_s: 300,
            base_url: Some(m.base.clone()),
        };
        let mut t = Thinq::with_credentials(cfg, creds()).unwrap();
        assert!(matches!(t.states().await, Err(CloudError::NotReady(_))));
        s.set("GET /devices", Reply::json(200, "not json"));
        assert!(matches!(t.devices().await, Err(CloudError::Malformed(_))));
    }
}
