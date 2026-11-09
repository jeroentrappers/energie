//! Kia Connect EU (`kia_uvo` integration with `hyundai_kia_connect_api`, region 1 / brand 1),
//! the EV3: `sensor.ev3_ev_battery_level`, `binary_sensor.ev3_ev_battery_plug`,
//! `binary_sensor.ev3_ev_battery_charge`, `number.ev3_ac_charging_limit`, `sensor.ev3_odometer`.
//!
//! Only the cached vehicle state is read (`GET /api/v1/spa/vehicles/{id}/ccs2/carstatus/latest`):
//! it never wakes the car, so it costs no 12 V battery and no remote-command quota. The poll
//! interval is at least 30 minutes, HA's `scan_interval`. HA's daily "force refresh" (which wakes
//! the car) is deliberately not done.
//!
//! Auth. The HA config entry holds a `token` object (`Token.to_dict()`): `access_token`
//! (`"Bearer …"`, about a day), `refresh_token`, `device_id`, `valid_until`. When the access
//! token is due the client refreshes it:
//! * legacy token (48-character refresh token, what the home's entry holds):
//!   `POST idpconnect-eu.kia.com/auth/api/v2/user/oauth2/token`, form
//!   `grant_type=refresh_token&refresh_token=…&client_id=<CCSP service id>&client_secret=secret`;
//! * OneApp/CCI token set (`cci_access_token`/`non_ccs_token` present, after a password login
//!   in a recent `hyundai_kia_connect_api`): `POST cci-api-eu.kia.com/auth/v2/token-refresh`
//!   signed with `X-Fingerprint`, then `POST /auth/v1/token-exchange?serviceType=CCS`.
//!
//! The refreshed token is written to the state file (never to `/etc`). When the refresh token
//! itself is rejected (or there is none) and the credentials hold a plaintext password, the
//! client signs in again with the OneApp/CCI password login (`_login_with_password_cci`):
//! `GET /auth/api/v2/user/oauth2/authorize` with the OneApp client id (not on the IDPConnect WAF
//! block list), the RSA key from `GET /auth/api/v1/accounts/certs` (a JWK), `POST
//! /auth/account/signin` with the password encrypted (PKCS#1 v1.5, hex), the authorization code
//! from its `302`, `POST cci-api-eu.kia.com/auth/v1/token?code=…` (signed) for the CCI token set,
//! and the CCS exchange as for a refresh. The new token set goes to the state file. Logins go
//! through a [`LoginGate`] (one per 30 min, none after three rejections until the credentials
//! change), so the account is never locked. Credentials set with `energie credentials set`
//! ([`Kia::set_credentials`]) are kept in the state dir and win over the file.
//!
//! Requests carry `ccsp-service-id`, `ccsp-application-id`, `ccsp-device-id`, a time-based
//! `Stamp` (the app id and unix time XOR-ed with a fixed key, base64) and
//! `Ccuccs2protocolsupport`. Errors come back as `{"retCode":"F","resCode":…}`: `4002` (stale
//! device id) re-registers the device and retries once, `7501` or HTTP 401 refreshes the token
//! and retries once, `5091` is the rate limit.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::error::{CloudError, Result};
use crate::http;
use crate::login::{self, CredentialStore, Fields, LoginGate};
use crate::poll::{Gate, Poller};
use crate::source::{
    Applied, CloudActuator, CloudCommand, CloudReading, CloudSource, EntityDescriptor, EntityKind, EntityState, flag,
    num, text,
};
use crate::store::{Secret, StateFile};
use energie_drivers::AuthStatus;
use serde_json::Map;

pub const LOGIN_HOST: &str = "https://idpconnect-eu.kia.com";
pub const API_BASE: &str = "https://prd.eu-ccapi.kia.com:8080";
pub const CCI_API: &str = "https://cci-api-eu.kia.com";

const CCSP_SERVICE_ID: &str = "fdc85c00-0a2f-4c64-bcb4-2cfb1500730a";
const CCS_SERVICE_SECRET: &str = "secret";
const APP_ID: &str = "a2b8469b-30a3-4361-8e13-6fceea8fbe74";
/// The XOR key of the `Stamp` header (Kia EU app constant).
const CFB_B64: &str = "wLTVxwidmH8CfJYBWSnHD6E0huk0ozdiuygB4hLkM5XCgzAL1Dk5sE36d/bx5PFMbZs=";
const USER_AGENT: &str = "okhttp/3.12.0";
/// The OneApp client of the password login and where its code goes.
const ONEAPP_CLIENT_ID: &str = "01b36c86-79e8-486c-8009-15f2ad88d670";
const ONEAPP_REDIRECT_URI: &str = "https://oneapp.kia.com/redirect";
/// The library's `USER_AGENT_MOZILLA + "_CCS_APP_AOS"` for the sign-in pages.
const LOGIN_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) \
                                Chrome/120.0.0.0 Mobile Safari/537.36_CCS_APP_AOS";

/// OneApp identity the CCI fingerprint gate checks (Android build 1.4.0).
const CCI_CLIENT_ID: &str = "com.kia.oneapp.eu";
const CCI_CLIENT_NAME: &str = "KiaEU";
const CCI_CLIENT_VERSION: &str = "1.4.0";
const CCI_OS_CODE: &str = "android";
const CCI_OS_VERSION: &str = "14";
const CCI_USER_AGENT: &str = "okhttp/4.12.0";
const CCI_FP_KEY: [&str; 3] = ["yb4Z1n2nlKe", "ukNilE7QEdb", "KB34nStPVa"];

/// HA's `scan_interval` default and the floor of this client's poll interval.
pub const MIN_POLL: Duration = Duration::from_secs(30 * 60);

/// Where the client talks to (tests point these at a local mock).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct KiaEndpoints {
    pub api_base: String,
    pub login_host: String,
    pub cci_api: String,
}

impl Default for KiaEndpoints {
    fn default() -> Self {
        KiaEndpoints { api_base: API_BASE.into(), login_host: LOGIN_HOST.into(), cci_api: CCI_API.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct KiaConfig {
    /// HA config-entry `data` as JSON (`/etc/energie/cloud-kia.json`).
    pub credentials: PathBuf,
    /// Where a refreshed token is kept (`/var/lib/energie/kia-token.json`).
    pub state_file: PathBuf,
    /// Credentials captured at run time (`<state_dir>/credentials/<id>.json`); they win over
    /// `credentials` when present.
    #[serde(default)]
    pub credentials_store: Option<PathBuf>,
    /// Vehicle nickname or VIN; the first vehicle on the account when absent.
    #[serde(default)]
    pub vehicle: Option<String>,
    /// Seconds between reads; raised to [`MIN_POLL`] when lower.
    #[serde(default = "default_poll")]
    pub poll_interval_s: u64,
    #[serde(default)]
    pub endpoints: KiaEndpoints,
}

fn default_poll() -> u64 {
    MIN_POLL.as_secs()
}

/// The HA config entry's `data` (and the shape of a captured credentials file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiaCredentials {
    pub username: Secret,
    pub password: Secret,
    #[serde(default)]
    pub pin: Secret,
    pub region: u8,
    pub brand: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<KiaToken>,
}

impl KiaCredentials {
    /// The password is a legacy 48-character refresh token (the way the integration took one
    /// before the password login worked again), not a password.
    pub fn password_is_refresh_token(&self) -> bool {
        let pw = self.password.expose();
        pw.len() == 48 && pw.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    }

    /// A password login is possible.
    pub fn can_login(&self) -> bool {
        !self.username.is_empty() && !self.password.is_empty() && !self.password_is_refresh_token()
    }
}

/// `hyundai_kia_connect_api`'s `Token.to_dict()`, field for field.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KiaToken {
    pub username: Option<Secret>,
    pub password: Option<Secret>,
    pub access_token: Option<Secret>,
    pub refresh_token: Option<Secret>,
    pub device_id: Option<String>,
    pub valid_until: Option<String>,
    pub stamp: Option<String>,
    pub pin: Option<Secret>,
    pub control_token: Option<Secret>,
    pub control_token_expiry: Option<f64>,
    pub cci_access_token: Option<Secret>,
    pub exchangeable_token: Option<Secret>,
    pub exchangeable_refresh_token: Option<Secret>,
    pub non_ccs_token: Option<Secret>,
    pub non_ccs_refresh_token: Option<Secret>,
    pub id_token: Option<Secret>,
    pub user_id: Option<String>,
}

impl KiaToken {
    /// `valid_until` as an instant; naive values are UTC (the library's `datetime.min` default
    /// and older entries).
    pub fn valid_until(&self) -> Option<DateTime<Utc>> {
        let s = self.valid_until.as_deref()?;
        DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .ok()
            .or_else(|| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|t| t.and_utc()))
    }

    /// The access token is usable at `now` (10 s grace, like the library).
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.access_token.as_ref().is_some_and(|t| !t.is_empty())
            && self.valid_until().is_some_and(|v| v - chrono::Duration::seconds(10) > now)
    }

    fn is_cci(&self) -> bool {
        self.cci_access_token.as_ref().or(self.non_ccs_token.as_ref()).is_some_and(|t| !t.is_empty())
    }

    fn set_valid_for(&mut self, secs: i64) {
        let until = Utc::now() + chrono::Duration::seconds(secs);
        self.valid_until = Some(until.to_rfc3339_opts(SecondsFormat::Micros, false));
    }
}

fn secret(v: &Value) -> Option<Secret> {
    v.as_str().filter(|s| !s.is_empty()).map(Secret::new)
}

/// The EV3 as the logic and the dashboard see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ev3Reading {
    /// `binary_sensor.ev3_ev_battery_plug` (`Green.ChargingInformation.ConnectorFastening.State`).
    pub plugged: Option<bool>,
    /// `binary_sensor.ev3_ev_battery_charge` (`Charging.RemainTime > 0`).
    pub charging: Option<bool>,
    /// `sensor.ev3_ev_battery_level`, % (`Green.BatteryManagement.BatteryRemain.Ratio`).
    pub soc: Option<f64>,
    /// `number.ev3_ac_charging_limit`, % (`TargetSoC.Standard`).
    pub ac_limit: Option<f64>,
    /// `number.ev3_dc_charging_limit`, % (`TargetSoC.Quick`).
    pub dc_limit: Option<f64>,
    /// `sensor.ev3_odometer`, km (`Drivetrain.Odometer`).
    pub odometer_km: Option<f64>,
    /// `sensor.ev3_last_updated_at`: when the car last reported (`Date`, UTC).
    pub last_updated: Option<DateTime<Utc>>,
}

const PLUG: &str = "binary_sensor.ev3_ev_battery_plug";
const CHARGE: &str = "binary_sensor.ev3_ev_battery_charge";
const SOC: &str = "sensor.ev3_ev_battery_level";
const AC_LIMIT: &str = "number.ev3_ac_charging_limit";
const DC_LIMIT: &str = "number.ev3_dc_charging_limit";
const ODOMETER: &str = "sensor.ev3_odometer";
const UPDATED: &str = "sensor.ev3_last_updated_at";

impl Ev3Reading {
    pub fn world(&self) -> energie_core::world::Ev3 {
        energie_core::world::Ev3 {
            plugged: self.plugged,
            soc: self.soc,
            ac_limit: self.ac_limit,
            odometer_km: self.odometer_km,
        }
    }

    pub fn entity_states(&self) -> Vec<EntityState> {
        vec![
            flag(PLUG, self.plugged),
            flag(CHARGE, self.charging),
            num(SOC, self.soc),
            num(AC_LIMIT, self.ac_limit),
            num(DC_LIMIT, self.dc_limit),
            num(ODOMETER, self.odometer_km),
            text(UPDATED, self.last_updated.map(|t| t.to_rfc3339())),
        ]
    }
}

/// Parse a `ccs2/carstatus/latest` response (`resMsg.state.Vehicle`).
pub fn parse_ccs2_status(v: &Value) -> Result<Ev3Reading> {
    let s = &v["resMsg"]["state"]["Vehicle"];
    if !s.is_object() {
        return Err(CloudError::Malformed("no resMsg.state.Vehicle".into()));
    }
    let ci = &s["Green"]["ChargingInformation"];
    let last_updated = s["Date"]
        .as_str()
        .and_then(|d| d.get(..14))
        .and_then(|d| NaiveDateTime::parse_from_str(d, "%Y%m%d%H%M%S").ok())
        .map(|t| t.and_utc());
    Ok(Ev3Reading {
        plugged: ci["ConnectorFastening"]["State"].as_f64().map(|x| x != 0.0),
        charging: ci["Charging"]["RemainTime"].as_f64().map(|x| x > 0.0),
        soc: s["Green"]["BatteryManagement"]["BatteryRemain"]["Ratio"].as_f64(),
        ac_limit: ci["TargetSoC"]["Standard"].as_f64(),
        dc_limit: ci["TargetSoC"]["Quick"].as_f64(),
        odometer_km: s["Drivetrain"]["Odometer"].as_f64(),
        last_updated,
    })
}

/// Why a Kia response failed.
#[derive(Debug)]
enum Fail {
    /// The access token was rejected: refresh and retry once.
    Token,
    /// The device id was invalidated (`4002`): re-register and retry once.
    DeviceId,
    Other(CloudError),
}

/// `_check_response_for_errors`, plus the HTTP status.
fn check(status: u16, v: &Value) -> std::result::Result<(), Fail> {
    if v["retCode"] == "F" {
        let code = v["resCode"].as_str().unwrap_or_default().to_string();
        let message = v["resMsg"].as_str().map(str::to_string).unwrap_or_else(|| v["resMsg"].to_string());
        return Err(match code.as_str() {
            "4002" => Fail::DeviceId,
            "7501" => Fail::Token,
            "5091" => Fail::Other(CloudError::RateLimited(message)),
            _ => Fail::Other(CloudError::Api { code, message }),
        });
    }
    if let Some(e) = v["error"].as_str() {
        let expired = e.to_ascii_lowercase().contains("token") && e.contains("expired");
        return Err(if expired {
            Fail::Token
        } else {
            Fail::Other(CloudError::Api { code: "error".into(), message: e.into() })
        });
    }
    match status {
        401 => Err(Fail::Token),
        200..=299 if ["retCode", "resCode", "resMsg"].iter().any(|k| v.get(k).is_some()) => Ok(()),
        200..=299 => Err(Fail::Other(CloudError::Malformed("unknown response shape".into()))),
        s => Err(Fail::Other(CloudError::Status(s))),
    }
}

/// The `Stamp` header at unix time `ts`.
pub fn stamp_at(ts: i64) -> String {
    let cfb = base64::engine::general_purpose::STANDARD.decode(CFB_B64).expect("constant");
    let raw = format!("{APP_ID}:{ts}");
    let x: Vec<u8> = cfb.iter().zip(raw.as_bytes()).map(|(a, b)| a ^ b).collect();
    base64::engine::general_purpose::STANDARD.encode(x)
}

/// `X-Fingerprint` over the OneApp identity, the timestamp and the body without ASCII
/// whitespace (HMAC-SHA256, hex).
pub fn fingerprint(timestamp_ms: &str, body: &str) -> String {
    let identity = format!("{CCI_CLIENT_ID}{CCI_CLIENT_NAME}{CCI_CLIENT_VERSION}{CCI_OS_CODE}");
    let stripped: String = body.chars().filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')).collect();
    let key = format!("{identity}{}", CCI_FP_KEY.concat());
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("any key length");
    mac.update(format!("{identity}{CCI_OS_VERSION}{timestamp_ms}{stripped}").as_bytes());
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// The password RSA-encrypted (PKCS#1 v1.5) with the JWK's modulus and exponent
/// (base64url), hex-encoded.
pub fn encrypt_password(n: &str, e: &str, password: &str) -> Result<String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let b = |s: &str| {
        URL_SAFE_NO_PAD
            .decode(s.trim_end_matches('='))
            .map_err(|_| CloudError::Malformed("RSA key is not base64url".into()))
    };
    let key = rsa::RsaPublicKey::new(rsa::BigUint::from_bytes_be(&b(n)?), rsa::BigUint::from_bytes_be(&b(e)?))
        .map_err(|e| CloudError::Malformed(format!("RSA key: {e}")))?;
    let mut rng = rsa::rand_core::OsRng;
    let out = key
        .encrypt(&mut rng, rsa::Pkcs1v15Encrypt, password.as_bytes())
        .map_err(|e| CloudError::Malformed(format!("RSA encryption: {e}")))?;
    Ok(out.iter().map(|b| format!("{b:02x}")).collect())
}

/// The authorization code from the sign-in's redirect, or why there is none. `base` resolves a
/// relative `location`.
fn signin_code(base: &reqwest::Url, location: &str) -> Result<String> {
    let url = base.join(location).map_err(|_| CloudError::Malformed("sign-in redirect is not a URL".into()))?;
    if let Some(code) = http::query_param(&url, "code").filter(|c| !c.is_empty()) {
        return Ok(code);
    }
    let lower = location.to_ascii_lowercase();
    Err(CloudError::Unauthorized(if lower.contains("error") {
        let why = http::query_param(&url, "error_description").unwrap_or_else(|| "unknown".into());
        format!("sign-in rejected: {why}; check username and password")
    } else if location.contains("/web/v1/user/authorization") {
        "the account must accept new terms: log in once in the Kia app or a browser, then set the \
         credentials again"
            .into()
    } else if location.contains("authorize") {
        "sign-in returned to the login page: check username and password".into()
    } else {
        format!("unexpected redirect after the sign-in (to {})", url.path())
    }))
}

/// Current UTC offset of Europe/Berlin as `+HH:MM` (the app's data timezone).
fn berlin_offset() -> String {
    Utc::now().with_timezone(&chrono_tz::Europe::Berlin).format("%:z").to_string()
}

/// The vehicle the client reads.
#[derive(Debug, Clone, PartialEq)]
pub struct Vehicle {
    pub id: String,
    pub name: String,
    pub ccs2: i64,
}

/// Cloud client for one Kia Connect EU vehicle.
pub struct Kia {
    endpoints: KiaEndpoints,
    want_vehicle: Option<String>,
    creds: KiaCredentials,
    creds_source: login::Source,
    store: Option<CredentialStore>,
    token: KiaToken,
    state: StateFile,
    client: reqwest::Client,
    vehicle: Option<Vehicle>,
    poller: Poller<Ev3Reading>,
    gate: LoginGate,
}

impl Kia {
    /// Load the credentials (the captured ones when present, else the file) and the state file.
    pub fn new(cfg: KiaConfig) -> Result<Self> {
        let store = cfg.credentials_store.as_ref().map(CredentialStore::new).transpose()?;
        let (creds, source) = login::load_credentials::<KiaCredentials>(store.as_ref(), &cfg.credentials)?;
        let mut kia = Kia::with_credentials(cfg, creds)?;
        kia.creds_source = source;
        Ok(kia)
    }

    /// Use already-loaded credentials. The token with the later `valid_until` wins between the
    /// credentials' and the state file's.
    pub fn with_credentials(cfg: KiaConfig, creds: KiaCredentials) -> Result<Self> {
        if creds.region != 1 || creds.brand != 1 {
            return Err(CloudError::Config(format!(
                "only Kia (brand 1) in Europe (region 1) is supported, not brand {} region {}",
                creds.brand, creds.region
            )));
        }
        let state = StateFile::new(&cfg.state_file)?;
        let stored: Option<KiaToken> = state.load()?;
        let from_creds = creds.token.clone().unwrap_or_default();
        let token = match stored {
            Some(s) if s.valid_until() >= from_creds.valid_until() => s,
            _ => from_creds,
        };
        let interval = Duration::from_secs(cfg.poll_interval_s).max(MIN_POLL);
        Ok(Kia {
            endpoints: cfg.endpoints,
            want_vehicle: cfg.vehicle,
            creds,
            creds_source: login::Source::Etc,
            store: cfg.credentials_store.map(CredentialStore::new).transpose()?,
            token,
            state,
            client: http::client(http::TIMEOUT, None, false)?,
            vehicle: None,
            poller: Poller::new(interval, Duration::from_secs(60), Duration::from_secs(6 * 3600)),
            gate: LoginGate::default(),
        })
    }

    /// Replace the login gate (tests use a shorter gap).
    pub fn with_gate(mut self, gate: LoginGate) -> Self {
        self.gate = gate;
        self
    }

    /// The sign-in state.
    pub fn auth_status(&self) -> AuthStatus {
        self.gate.status(self.creds_source.label(), self.creds.can_login(), Instant::now())
    }

    pub fn token(&self) -> &KiaToken {
        &self.token
    }

    pub fn poll_interval(&self) -> Duration {
        self.poller.interval()
    }

    fn spa(&self, path: &str) -> String {
        format!("{}/api/v1/spa/{path}", self.endpoints.api_base.trim_end_matches('/'))
    }

    fn persist(&self) -> Result<()> {
        self.state.save(&self.token)
    }

    /// Refresh the access token if it is not fresh.
    async fn ensure_token(&mut self) -> Result<()> {
        if self.token.is_fresh(Utc::now()) { Ok(()) } else { self.renew().await }
    }

    /// A new access token: a refresh, and a password login when the refresh is rejected and
    /// the credentials allow one.
    async fn renew(&mut self) -> Result<()> {
        match self.refresh().await {
            Err(CloudError::Unauthorized(_)) if self.creds.can_login() => {
                self.gate.session(false);
                self.password_login().await
            }
            Err(e) => {
                if login::is_rejection(&e) {
                    self.gate.session(false);
                }
                Err(e)
            }
            Ok(()) => {
                self.gate.session(true);
                Ok(())
            }
        }
    }

    /// A password login through the gate; the new token set is persisted.
    pub async fn password_login(&mut self) -> Result<()> {
        self.gate.check(Instant::now())?;
        let outcome = self.login().await;
        self.gate.record(Instant::now(), Utc::now(), &outcome);
        outcome?;
        self.persist()
    }

    /// The OneApp/CCI password login (no gate, nothing persisted): a fresh device id, the
    /// sign-in, the CCI token set and its CCS token.
    pub async fn login(&mut self) -> Result<()> {
        if !self.creds.can_login() {
            return Err(CloudError::Unauthorized("no username and password to log in with".into()));
        }
        // A failed legacy register does not stop the login: a random device identity works too.
        if self.register_device().await.is_err() {
            self.token.device_id = Some(http::uuid4());
        }
        let code = self.signin().await?;
        let cci = self.endpoints.cci_api.trim_end_matches('/').to_string();
        let resp = self
            .client
            .post(format!("{cci}/auth/v1/token"))
            .query(&[("code", code.as_str())])
            .headers(self.cci_headers(""))
            .send()
            .await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        if status != 200 {
            return Err(CloudError::Api {
                code: "cci-token".into(),
                message: format!("CCI token exchange: HTTP {status} {}", http::snippet(&text, 200)),
            });
        }
        let v: Value = serde_json::from_str(&text)?;
        self.token = KiaToken {
            device_id: self.token.device_id.take(),
            cci_access_token: secret(&v["accessToken"]),
            refresh_token: secret(&v["refreshToken"]),
            non_ccs_token: secret(&v["nonCcsToken"]),
            exchangeable_token: secret(&v["exchangeableAccessToken"]),
            exchangeable_refresh_token: secret(&v["exchangeableRefreshToken"]),
            non_ccs_refresh_token: secret(&v["nonCcsRefreshToken"]),
            id_token: secret(&v["idToken"]),
            ..KiaToken::default()
        };
        // The credentials were accepted; a failing exchange is not a rejected password.
        self.exchange_ccs().await.map_err(|e| match e {
            CloudError::Unauthorized(m) => CloudError::Api { code: "ccs-exchange".into(), message: m },
            e => e,
        })
    }

    /// Authorize, fetch the RSA key, sign in: the authorization code.
    async fn signin(&self) -> Result<String> {
        let host = self.endpoints.login_host.trim_end_matches('/');
        let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
        let follow = http::client(http::TIMEOUT, Some(jar.clone()), true)?;
        let manual = http::client(http::TIMEOUT, Some(jar), false)?;
        let mut auth = http::parse_url(&format!("{host}/auth/api/v2/user/oauth2/authorize"))?;
        auth.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", ONEAPP_CLIENT_ID)
            .append_pair("redirect_uri", ONEAPP_REDIRECT_URI)
            .append_pair("lang", "en")
            .append_pair("state", "ccsp")
            .append_pair("country", "de");
        let resp = follow.get(auth).header("User-Agent", LOGIN_USER_AGENT).send().await?;
        let landed = resp.url().clone();
        let page = resp.text().await?;
        if page.to_lowercase().contains("abusing") || landed.as_str().contains("/error?status=400") {
            return Err(CloudError::Api {
                code: "waf".into(),
                message: "the IDPConnect authorize was blocked by the WAF ('abusing request'): a block on \
                          the server side, not the credentials"
                    .into(),
            });
        }
        let resp = follow
            .get(format!("{host}/auth/api/v1/accounts/certs"))
            .header("User-Agent", LOGIN_USER_AGENT)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let jwk: Value = serde_json::from_str(&resp.text().await?).unwrap_or_default();
        let jwk = &jwk["retValue"];
        let (Some(n), Some(e), 200) = (jwk["n"].as_str(), jwk["e"].as_str(), status) else {
            return Err(CloudError::Api { code: "certs".into(), message: format!("no RSA key (HTTP {status})") });
        };
        let password = encrypt_password(n, e, self.creds.password.expose())?;
        let kid = jwk["kid"].as_str().unwrap_or_default();
        let form = [
            ("client_id", ONEAPP_CLIENT_ID),
            ("encryptedPassword", "true"),
            ("password", &password),
            ("redirect_uri", ONEAPP_REDIRECT_URI),
            ("scope", ""),
            ("nonce", ""),
            ("state", "ccsp"),
            ("username", self.creds.username.expose()),
            ("connector_session_key", ""),
            ("kid", kid),
            ("_csrf", ""),
        ];
        let resp = manual
            .post(format!("{host}/auth/account/signin"))
            .header("User-Agent", LOGIN_USER_AGENT)
            .form(&form)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        if status != 302 {
            return Err(CloudError::Unauthorized(format!(
                "sign-in failed (HTTP {status}): check username and password"
            )));
        }
        signin_code(&landed, &location)
    }

    /// Keep new credentials in the store and sign in with them now. The gate and the backoff
    /// start over; the outcome is in the returned status.
    pub async fn set_credentials(&mut self, fields: &Map<String, Value>) -> Result<AuthStatus> {
        let f = Fields::new(fields, &["username", "password"], &["pin"])?;
        let store =
            self.store.clone().ok_or_else(|| CloudError::Config("no state dir for captured credentials".into()))?;
        let creds = KiaCredentials {
            username: f.secret("username").unwrap_or_default(),
            password: f.secret("password").unwrap_or_default(),
            pin: f.secret("pin").unwrap_or_default(),
            region: 1,
            brand: 1,
            token: None,
        };
        store.save(&creds)?;
        self.creds = creds;
        self.creds_source = login::Source::State;
        self.gate.reset();
        self.poller.reset();
        if self.creds.can_login() {
            // The status reports the outcome.
            let _ = self.password_login().await;
        } else {
            // A legacy refresh token entered as the password: start a token set from it.
            self.token = KiaToken {
                device_id: self.token.device_id.take(),
                refresh_token: Some(self.creds.password.clone()),
                ..KiaToken::default()
            };
            let _ = self.renew().await;
        }
        Ok(self.auth_status())
    }

    /// Refresh the access token (CCI token set or legacy refresh token) and persist it.
    pub async fn refresh(&mut self) -> Result<()> {
        if self.token.is_cci() {
            self.refresh_cci().await?;
        } else {
            let mut candidates: Vec<String> = self.token.refresh_token.iter().map(|t| t.expose().to_string()).collect();
            let pw = self.creds.password.expose();
            if pw.len() == 48 && pw.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
                candidates.push(pw.to_string());
            }
            candidates.dedup();
            let mut outcome = Err(CloudError::Unauthorized("no refresh token in the credentials or state file".into()));
            for rt in candidates {
                outcome = self.legacy_refresh(&rt).await;
                if outcome.is_ok() {
                    break;
                }
            }
            outcome?;
        }
        self.persist()
    }

    /// `grant_type=refresh_token` at the IDPConnect token endpoint.
    async fn legacy_refresh(&mut self, refresh_token: &str) -> Result<()> {
        let url = format!("{}/auth/api/v2/user/oauth2/token", self.endpoints.login_host.trim_end_matches('/'));
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CCSP_SERVICE_ID),
            ("client_secret", CCS_SERVICE_SECRET),
        ];
        let resp = self.client.post(url).form(&form).send().await?;
        let status = resp.status().as_u16();
        let v: Value = serde_json::from_str(&resp.text().await?).unwrap_or_default();
        let (Some(kind), Some(access)) = (v["token_type"].as_str(), v["access_token"].as_str()) else {
            let why = v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no access_token");
            return Err(CloudError::Unauthorized(format!("refresh token rejected (HTTP {status}): {why}")));
        };
        self.token.access_token = Some(Secret::new(format!("{kind} {access}")));
        self.token.refresh_token = Some(secret(&v["refresh_token"]).unwrap_or_else(|| Secret::new(refresh_token)));
        self.token.set_valid_for(v["expires_in"].as_i64().unwrap_or(3600));
        Ok(())
    }

    /// Headers of the fingerprint-gated OneApp endpoints, signed over `body`.
    fn cci_headers(&self, body: &str) -> reqwest::header::HeaderMap {
        let device = self.token.device_id.clone().unwrap_or_default();
        let ts = Utc::now().timestamp_millis().to_string();
        let request_id = format!("{}_{}_AOS", http::random_hex(12).to_uppercase(), device.replace('-', ""));
        let pairs = [
            ("client-id", CCI_CLIENT_ID.to_string()),
            ("client-name", CCI_CLIENT_NAME.to_string()),
            ("client-version", CCI_CLIENT_VERSION.to_string()),
            ("client-os-code", CCI_OS_CODE.to_string()),
            ("client-os-version", CCI_OS_VERSION.to_string()),
            ("client-device-id", device),
            ("client-device-model", "SM-S918B".to_string()),
            ("client-notification-provider-type", "FCM".to_string()),
            ("locale", "EN".to_string()),
            ("timezone", berlin_offset()),
            ("App-Request-Id", request_id),
            ("X-Brand", "Kia".to_string()),
            ("X-Region", "eu".to_string()),
            ("User-Agent", CCI_USER_AGENT.to_string()),
            ("Accept", "*/*".to_string()),
            ("Accept-Language", "en".to_string()),
            ("X-Fingerprint", fingerprint(&ts, body)),
            ("X-Timestamp", ts),
        ];
        let mut h = reqwest::header::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).expect("header name"),
                v.parse().expect("header value"),
            );
        }
        h
    }

    /// Refresh the OneApp token set, then exchange it for a CCS token.
    async fn refresh_cci(&mut self) -> Result<()> {
        let t = &self.token;
        let get = |s: &Option<Secret>| s.as_ref().map(|s| s.expose().to_string()).unwrap_or_default();
        let body = json!({
            "accessToken": get(&t.cci_access_token).trim_start_matches("Bearer ").to_string(),
            "refreshToken": get(&t.refresh_token),
            "exchangeableAccessToken": get(&t.exchangeable_token),
            "exchangeableRefreshToken": get(&t.exchangeable_refresh_token),
            "nonCcsToken": get(&t.non_ccs_token),
            "nonCcsRefreshToken": get(&t.non_ccs_refresh_token),
            "idToken": get(&t.id_token),
        })
        .to_string();
        let cci = self.endpoints.cci_api.trim_end_matches('/').to_string();
        let resp = self
            .client
            .post(format!("{cci}/auth/v2/token-refresh"))
            .headers(self.cci_headers(&body))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let cookie = resp.headers().get("set-cookie").and_then(|v| v.to_str().ok()).map(str::to_string);
        let text = resp.text().await?;
        if status != 200 {
            return Err(CloudError::Unauthorized(format!(
                "CCI token refresh: HTTP {status} {}",
                http::snippet(&text, 200)
            )));
        }
        let v: Value = serde_json::from_str(&text)?;
        let keep = |new: &Value, old: &Option<Secret>| secret(new).or_else(|| old.clone());
        let t = &mut self.token;
        t.cci_access_token = keep(&v["accessToken"], &t.cci_access_token);
        t.refresh_token = keep(&v["refreshToken"], &t.refresh_token);
        t.non_ccs_token = keep(&v["nonCcsToken"], &t.non_ccs_token);
        t.exchangeable_token = keep(&v["exchangeableAccessToken"], &t.exchangeable_token);
        t.exchangeable_refresh_token = keep(&v["exchangeableRefreshToken"], &t.exchangeable_refresh_token);
        t.non_ccs_refresh_token = keep(&v["nonCcsRefreshToken"], &t.non_ccs_refresh_token);
        t.id_token = keep(&v["idToken"], &t.id_token);
        if let Some(x) = cookie.as_deref().and_then(|c| c.split(';').find_map(|p| p.trim().strip_prefix("t="))) {
            t.exchangeable_token = Some(Secret::new(x)).filter(|s| !s.is_empty()).or(t.exchangeable_token.take());
        }
        self.exchange_ccs().await
    }

    /// `token-exchange?serviceType=CCS`: the OneApp access token for a CCS token the ccapi
    /// endpoints accept.
    async fn exchange_ccs(&mut self) -> Result<()> {
        let get = |s: &Option<Secret>| s.as_ref().map(|s| s.expose().to_string()).unwrap_or_default();
        let mut headers = self.cci_headers("");
        let cci_token = get(&self.token.cci_access_token);
        let set = |h: &mut reqwest::header::HeaderMap, k: &'static str, v: String| {
            h.insert(k, v.parse().map_err(|_| CloudError::Malformed("token is not a header value".into()))?);
            Ok::<_, CloudError>(())
        };
        set(&mut headers, "authorization", format!("Bearer {}", cci_token.trim_start_matches("Bearer ").trim()))?;
        set(&mut headers, "authentication", get(&self.token.non_ccs_token))?;
        if self.token.exchangeable_token.is_some() {
            set(&mut headers, "exchangeable-token", get(&self.token.exchangeable_token))?;
            set(&mut headers, "non-ccs-token", get(&self.token.non_ccs_token))?;
        }
        let url = format!("{}/auth/v1/token-exchange", self.endpoints.cci_api.trim_end_matches('/'));
        let resp = self.client.post(url).query(&[("serviceType", "CCS")]).headers(headers).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        let v: Value = serde_json::from_str(&text).unwrap_or_default();
        let ccs = v["accessToken"].as_str().or(v["ccsAccessToken"].as_str()).filter(|s| !s.is_empty());
        let (200, Some(ccs)) = (status, ccs) else {
            return Err(CloudError::Unauthorized(format!(
                "CCS token exchange: HTTP {status} {}",
                http::snippet(&text, 200)
            )));
        };
        self.token.access_token = Some(Secret::new(format!("Bearer {ccs}")));
        self.token.set_valid_for(v["expiresTime"].as_i64().unwrap_or(3600));
        Ok(())
    }

    /// Register a push device for this client (the server ties tokens to a device id).
    async fn register_device(&mut self) -> Result<()> {
        let body = json!({"pushRegId": http::random_hex(32), "pushType": "APNS", "uuid": http::uuid4()});
        let resp = self
            .client
            .post(self.spa("notifications/register"))
            .header("ccsp-service-id", CCSP_SERVICE_ID)
            .header("ccsp-application-id", APP_ID)
            .header("Stamp", stamp_at(Utc::now().timestamp()))
            .header("User-Agent", USER_AGENT)
            .json(&body)
            .send()
            .await?;
        let status = resp.status().as_u16();
        let v: Value = serde_json::from_str(&resp.text().await?)?;
        check(status, &v).map_err(|f| match f {
            Fail::Other(e) => e,
            other => CloudError::Api { code: "register".into(), message: format!("{other:?}") },
        })?;
        let id = v["resMsg"]["deviceId"].as_str().ok_or_else(|| CloudError::Malformed("no resMsg.deviceId".into()))?;
        self.token.device_id = Some(id.to_string());
        self.persist()
    }

    /// One authenticated ccapi request; refreshes the token or re-registers the device once if
    /// the server asks for it.
    async fn request(&mut self, method: reqwest::Method, path: &str, body: Option<&Value>, ccs2: i64) -> Result<Value> {
        self.ensure_token().await?;
        if self.token.device_id.as_deref().is_none_or(str::is_empty) {
            self.register_device().await?;
        }
        let mut retried = false;
        loop {
            let mut req = self
                .client
                .request(method.clone(), self.spa(path))
                .header("Authorization", self.token.access_token.as_ref().map(Secret::expose).unwrap_or_default())
                .header("ccsp-service-id", CCSP_SERVICE_ID)
                .header("ccsp-application-id", APP_ID)
                .header("Stamp", stamp_at(Utc::now().timestamp()))
                .header("ccsp-device-id", self.token.device_id.clone().unwrap_or_default())
                .header("Ccuccs2protocolsupport", ccs2.to_string())
                .header("User-Agent", USER_AGENT);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            let text = resp.text().await?;
            let v: Value = serde_json::from_str(&text).unwrap_or_default();
            match check(status, &v) {
                Ok(()) => return Ok(v),
                Err(Fail::Token) if !retried => self.renew().await?,
                Err(Fail::DeviceId) if !retried => self.register_device().await?,
                Err(Fail::Token) => return Err(CloudError::Unauthorized("access token rejected after refresh".into())),
                Err(Fail::DeviceId) => {
                    return Err(CloudError::Api { code: "4002".into(), message: "invalid device id".into() });
                }
                Err(Fail::Other(e)) => return Err(e),
            }
            retried = true;
        }
    }

    /// The configured vehicle (looked up once).
    pub async fn vehicle(&mut self) -> Result<Vehicle> {
        if let Some(v) = &self.vehicle {
            return Ok(v.clone());
        }
        let v = self.request(reqwest::Method::GET, "vehicles", None, 0).await?;
        let list = v["resMsg"]["vehicles"].as_array().cloned().unwrap_or_default();
        let want = self.want_vehicle.clone();
        let found = list.iter().find(|e| {
            want.as_deref().is_none_or(|w| e["nickname"].as_str() == Some(w) || e["vin"].as_str() == Some(w))
        });
        let e = found.ok_or_else(|| CloudError::NotReady(format!("vehicle {want:?} not on the account")))?;
        let vehicle = Vehicle {
            id: e["vehicleId"]
                .as_str()
                .ok_or_else(|| CloudError::Malformed("vehicle without vehicleId".into()))?
                .into(),
            name: e["nickname"].as_str().unwrap_or_default().into(),
            ccs2: e["ccuCCS2ProtocolSupport"].as_i64().unwrap_or(0),
        };
        self.vehicle = Some(vehicle.clone());
        Ok(vehicle)
    }

    /// One read of the cached vehicle state (no rate limit; [`CloudSource::read`] has one).
    pub async fn status(&mut self) -> Result<Ev3Reading> {
        let veh = self.vehicle().await?;
        if veh.ccs2 == 0 {
            return Err(CloudError::Malformed("vehicle does not report the CCS2 state format".into()));
        }
        let path = format!("vehicles/{}/ccs2/carstatus/latest", veh.id);
        let v = self.request(reqwest::Method::GET, &path, None, veh.ccs2).await?;
        parse_ccs2_status(&v)
    }

    /// `number.ev3_ac_charging_limit`: set the AC target SoC, keeping the DC one (HA's
    /// `async_set_native_value`). 50..=100 in steps of 10.
    pub async fn set_ac_limit(&mut self, pct: u8) -> Result<Applied> {
        if !(50..=100).contains(&pct) || !pct.is_multiple_of(10) {
            return Err(CloudError::OutOfRange(format!("AC limit {pct} % (50..100 in steps of 10)")));
        }
        let current = match self.poller.last() {
            Some(r) => r.clone(),
            None => self.status().await?,
        };
        if current.ac_limit == Some(f64::from(pct)) {
            return Ok(Applied::Unchanged);
        }
        let dc = current.dc_limit.filter(|d| (50.0..=100.0).contains(d)).ok_or_else(|| {
            CloudError::NotReady("the DC charging limit is not known, so the AC one cannot be set".into())
        })?;
        let veh = self.vehicle().await?;
        let body = json!({"targetSOClist": [
            {"plugType": 0, "targetSOClevel": dc as i64},
            {"plugType": 1, "targetSOClevel": pct},
        ]});
        self.request(reqwest::Method::POST, &format!("vehicles/{}/charge/target", veh.id), Some(&body), veh.ccs2)
            .await?;
        self.poller.update(|r| r.ac_limit = Some(f64::from(pct)));
        Ok(Applied::Done)
    }
}

const ENTITIES: [(&str, EntityKind, Option<&str>, Option<&str>); 7] = [
    (PLUG, EntityKind::BinarySensor, None, Some("ev3.plugged")),
    (CHARGE, EntityKind::BinarySensor, None, None),
    (SOC, EntityKind::Sensor, Some("%"), Some("ev3.soc")),
    (AC_LIMIT, EntityKind::Number, Some("%"), Some("ev3.ac_limit")),
    (DC_LIMIT, EntityKind::Number, Some("%"), None),
    (ODOMETER, EntityKind::Sensor, Some("km"), Some("ev3.odometer_km")),
    (UPDATED, EntityKind::Sensor, None, None),
];

#[async_trait]
impl CloudSource for Kia {
    fn name(&self) -> &'static str {
        "kia"
    }

    fn entities(&self) -> Vec<EntityDescriptor> {
        ENTITIES
            .iter()
            .map(|(id, kind, unit, field)| {
                let d = EntityDescriptor::new(*id, *kind, *unit);
                let d = if *id == AC_LIMIT { d.writable() } else { d };
                match field {
                    Some(f) => d.feeds(f),
                    None => d,
                }
            })
            .collect()
    }

    async fn read(&mut self) -> Result<CloudReading> {
        match self.poller.gate(Instant::now()) {
            Gate::Cached(r) => return Ok(CloudReading::Ev3(r.clone())),
            Gate::Wait(d) => return Err(CloudError::Backoff(d)),
            Gate::Fetch => {}
        }
        let outcome = self.status().await;
        match &outcome {
            Ok(_) => self.gate.session(true),
            Err(e) if login::is_rejection(e) => self.gate.session(false),
            Err(_) => {}
        }
        self.poller.record(Instant::now(), outcome).map(CloudReading::Ev3)
    }

    fn unavailable(&self) -> CloudReading {
        CloudReading::Ev3(Ev3Reading::default())
    }

    fn auth(&self) -> Option<AuthStatus> {
        Some(self.auth_status())
    }

    async fn set_credentials(&mut self, fields: &Map<String, Value>) -> Result<AuthStatus> {
        Kia::set_credentials(self, fields).await
    }
}

#[async_trait]
impl CloudActuator for Kia {
    async fn apply(&mut self, cmd: &CloudCommand) -> Result<Applied> {
        match cmd {
            CloudCommand::Ev3AcLimit(pct) => self.set_ac_limit(*pct).await,
            _ => Ok(Applied::NotMine),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::{Log, Mock, Reply, Script};
    use std::collections::HashMap;

    /// The EV3's `ccs2/carstatus/latest` state as HA held it on 2026-11-07 (`sensor.ev3_data`,
    /// attribute `vehicle_data`), in the API's response envelope.
    const STATUS: &str = include_str!("../fixtures/kia_ccs2_status.json");
    const VEHICLES: &str = r#"{"retCode":"S","resCode":"0000","resMsg":{"vehicles":[
        {"vehicleId":"veh-other","nickname":"Other","vehicleName":"EV6","regDate":"2024-01-01","vin":"VINOTHER000000000","type":"EV","ccuCCS2ProtocolSupport":1},
        {"vehicleId":"veh-ev3","nickname":"EV3","vehicleName":"EV3","regDate":"2025-05-01","vin":"VINPLACEHOLDER001","type":"EV","ccuCCS2ProtocolSupport":1}]}}"#;
    const OK: &str = r#"{"retCode":"S","resCode":"0000","resMsg":null,"msgId":"m1"}"#;
    const LATEST: &str = "GET /api/v1/spa/vehicles/veh-ev3/ccs2/carstatus/latest";
    const TOKEN: &str = "POST /auth/api/v2/user/oauth2/token";
    const REFRESH_RT: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    fn creds(token: KiaToken) -> KiaCredentials {
        KiaCredentials {
            username: "user@example.invalid".into(),
            password: "not-a-token".into(),
            pin: "0000".into(),
            region: 1,
            brand: 1,
            token: Some(token),
        }
    }

    fn fresh_token() -> KiaToken {
        let mut t = KiaToken {
            access_token: Some("Bearer old-access".into()),
            refresh_token: Some(REFRESH_RT.into()),
            device_id: Some("dev-1".into()),
            ..KiaToken::default()
        };
        t.set_valid_for(3600);
        t
    }

    fn expired(mut t: KiaToken) -> KiaToken {
        t.valid_until = Some("2026-11-07T13:23:20.491816+00:00".into());
        t
    }

    struct Rig {
        mock: Mock,
        script: Script,
        log: Log,
        dir: tempfile::TempDir,
    }

    async fn rig() -> Rig {
        let (mock, script, log) = Mock::scripted().await;
        script.set("GET /api/v1/spa/vehicles", Reply::json(200, VEHICLES));
        script.set(LATEST, Reply::json(200, STATUS));
        script.set("POST /api/v1/spa/vehicles/veh-ev3/charge/target", Reply::json(200, OK));
        script.set(
            TOKEN,
            Reply::json(
                200,
                r#"{"token_type":"Bearer","access_token":"new-access","refresh_token":"ROTATED","expires_in":86400}"#,
            ),
        );
        script.set(
            "POST /api/v1/spa/notifications/register",
            Reply::json(200, r#"{"retCode":"S","resCode":"0000","resMsg":{"deviceId":"dev-2"}}"#),
        );
        Rig { mock, script, log, dir: tempfile::tempdir().unwrap() }
    }

    impl Rig {
        fn config(&self) -> KiaConfig {
            KiaConfig {
                credentials: self.dir.path().join("cloud-kia.json"),
                credentials_store: Some(self.dir.path().join("credentials/ev3.json")),
                state_file: self.dir.path().join("state/kia-token.json"),
                vehicle: Some("EV3".into()),
                poll_interval_s: 0,
                endpoints: KiaEndpoints {
                    api_base: self.mock.base.clone(),
                    login_host: self.mock.base.clone(),
                    cci_api: self.mock.base.clone(),
                },
            }
        }

        fn kia(&self, token: KiaToken) -> Kia {
            Kia::with_credentials(self.config(), creds(token)).unwrap()
        }

        /// A client with its own (empty) state file.
        fn isolated(&self, token: KiaToken) -> Kia {
            let state_file = self.dir.path().join(format!("{}.json", http::random_hex(8)));
            Kia::with_credentials(KiaConfig { state_file, ..self.config() }, creds(token)).unwrap()
        }

        fn stored(&self) -> Option<KiaToken> {
            StateFile::new(self.config().state_file).unwrap().load().unwrap()
        }
    }

    /// The captured state maps onto the HA entities (values as HA showed them on 2026-11-07).
    #[test]
    fn fixture_maps_to_ev3_entities() {
        let r = parse_ccs2_status(&serde_json::from_str(STATUS).unwrap()).unwrap();
        assert_eq!(r.soc, Some(54.5));
        assert_eq!(r.plugged, Some(false));
        assert_eq!(r.charging, Some(false));
        assert_eq!((r.ac_limit, r.dc_limit), (Some(100.0), Some(100.0)));
        assert_eq!(r.odometer_km, Some(8634.1));
        assert_eq!(r.last_updated.unwrap().to_rfc3339(), "2026-11-07T09:31:38+00:00");
        assert_eq!(
            r.world(),
            energie_core::world::Ev3 {
                plugged: Some(false),
                soc: Some(54.5),
                ac_limit: Some(100.0),
                odometer_km: Some(8634.1)
            }
        );
        let states = r.entity_states();
        assert_eq!(states[0], (PLUG.into(), Some(crate::EntityValue::Bool(false))));
        assert_eq!(states[6].1, Some(crate::EntityValue::Text("2026-11-07T09:31:38+00:00".into())));
    }

    /// Plugged in and charging; a body without the state tree is a parse error.
    #[test]
    fn charging_and_malformed_states() {
        let mut v: Value = serde_json::from_str(STATUS).unwrap();
        let ci = &mut v["resMsg"]["state"]["Vehicle"]["Green"]["ChargingInformation"];
        ci["ConnectorFastening"]["State"] = 1.into();
        ci["Charging"]["RemainTime"] = 95.into();
        let r = parse_ccs2_status(&v).unwrap();
        assert_eq!((r.plugged, r.charging), (Some(true), Some(true)));
        assert!(matches!(parse_ccs2_status(&json!({"resMsg": {}})), Err(CloudError::Malformed(_))));
    }

    /// The error mapping of `_check_response_for_errors`.
    #[test]
    fn response_checks() {
        let f = |code: &str| json!({"retCode": "F", "resCode": code, "resMsg": "msg"});
        assert!(matches!(check(200, &f("4002")), Err(Fail::DeviceId)));
        assert!(matches!(check(200, &f("7501")), Err(Fail::Token)));
        assert!(matches!(check(200, &f("5091")), Err(Fail::Other(CloudError::RateLimited(_)))));
        assert!(matches!(check(200, &f("9999")), Err(Fail::Other(CloudError::Api { .. }))));
        let obj = json!({"retCode": "F", "resCode": "4004", "resMsg": {"x": 1}});
        assert!(matches!(check(200, &obj), Err(Fail::Other(CloudError::Api { message, .. })) if message.contains('x')));
        assert!(matches!(check(200, &json!({"error": "Key not authorized: Token is expired"})), Err(Fail::Token)));
        assert!(matches!(check(200, &json!({"error": "nope"})), Err(Fail::Other(CloudError::Api { .. }))));
        assert!(matches!(check(401, &json!({})), Err(Fail::Token)));
        assert!(matches!(check(200, &json!({"x": 1})), Err(Fail::Other(CloudError::Malformed(_)))));
        assert!(matches!(check(503, &json!({})), Err(Fail::Other(CloudError::Status(503)))));
        assert!(check(200, &json!({"retCode": "S"})).is_ok());
    }

    /// `Stamp` and `X-Fingerprint` match the reference implementation (vectors computed with
    /// the library's formulas).
    #[test]
    fn stamp_and_fingerprint_vectors() {
        assert_eq!(stamp_at(1_762_500_000), "oYa3/zyroR0vT6ZgagTzPJcFq9FRkgRPjU5ih3eFC/Og5gc/7ggOhn/PR8bB1ME=");
        assert_eq!(
            fingerprint("1762500000000", "{\"a\": 1,\n \"b\":\t\"x y\"}"),
            "85278e1e560baddd931f304e9b530471749d73967593eaef00de6c2061f61fd1"
        );
        assert_eq!(
            fingerprint("1762500000000", ""),
            "a696ac4f807072ea115df734ba7a07b0ef793fbf36833a086878ecaa669f1ac4"
        );
        assert!(berlin_offset() == "+01:00" || berlin_offset() == "+02:00");
    }

    /// Token freshness: aware and naive `valid_until`, the library's `datetime.min`, no token.
    #[test]
    fn token_validity() {
        let now = Utc::now();
        assert!(fresh_token().is_fresh(now));
        assert!(!expired(fresh_token()).is_fresh(now));
        let naive = KiaToken {
            valid_until: Some("2999-01-01T00:00:00".into()),
            access_token: Some("Bearer x".into()),
            ..KiaToken::default()
        };
        assert!(naive.is_fresh(now));
        let min = KiaToken { valid_until: Some("0001-01-01T00:00:00".into()), ..naive.clone() };
        assert!(!min.is_fresh(now));
        let garbage = KiaToken { valid_until: Some("soon".into()), ..naive.clone() };
        assert_eq!(garbage.valid_until(), None);
        assert!(!KiaToken { access_token: None, ..naive }.is_fresh(now));
        // The HA entry's token deserializes (shape of the live config entry, placeholder values).
        let ha: KiaToken = serde_json::from_str(r#"{"access_token":"Bearer a","cci_access_token":null,"control_token":null,
            "control_token_expiry":0,"device_id":"d","exchangeable_refresh_token":null,"exchangeable_token":null,"id_token":null,
            "non_ccs_refresh_token":null,"non_ccs_token":null,"password":"p","pin":"0000","refresh_token":"r","stamp":null,
            "user_id":null,"username":"u","valid_until":"2026-11-07T13:23:20.491816+00:00"}"#).unwrap();
        assert!(!ha.is_cci());
        assert!(!format!("{ha:?}").contains("Bearer a"));
    }

    /// Only Kia Europe is accepted; the configuration parses with defaults.
    #[test]
    fn config_and_region() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: KiaConfig = serde_json::from_value(
            json!({"credentials": "/etc/energie/cloud-kia.json", "state_file": dir.path().join("k.json")}),
        )
        .unwrap();
        assert_eq!(cfg.endpoints, KiaEndpoints::default());
        assert_eq!(cfg.poll_interval_s, 1800);
        let mut c = creds(fresh_token());
        c.region = 9;
        assert!(matches!(Kia::with_credentials(cfg.clone(), c), Err(CloudError::Config(_))));
        let k = Kia::with_credentials(KiaConfig { poll_interval_s: 60, ..cfg.clone() }, creds(fresh_token())).unwrap();
        assert_eq!(k.poll_interval(), MIN_POLL);
        assert!(matches!(Kia::new(cfg), Err(CloudError::Config(_))));
        let path = dir.path().join("cloud-kia.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({"username":"u","password":"p","pin":"1","region":1,"brand":1})).unwrap(),
        )
        .unwrap();
        let cfg = KiaConfig {
            credentials: path,
            credentials_store: None,
            state_file: dir.path().join("k.json"),
            vehicle: None,
            poll_interval_s: 1800,
            endpoints: KiaEndpoints::default(),
        };
        assert_eq!(Kia::new(cfg).unwrap().token(), &KiaToken::default());
    }

    /// A fresh token is used as is: vehicles once, then the cached state; headers as the app
    /// sends them; one read per poll interval.
    #[tokio::test]
    async fn reads_cached_state_with_fresh_token() {
        let r = rig().await;
        let mut k = r.kia(fresh_token());
        assert_eq!(k.name(), "kia");
        let CloudReading::Ev3(ev3) = k.read().await.unwrap() else { panic!() };
        assert_eq!(ev3.soc, Some(54.5));
        let CloudReading::Ev3(again) = k.read().await.unwrap() else { panic!() };
        assert_eq!(again, ev3);
        assert_eq!(r.log.count("GET /api/v1/spa/vehicles"), 2);
        assert_eq!(r.log.count(LATEST), 1);
        assert_eq!(r.log.count(TOKEN), 0);
        let (h, _) = r.log.last(LATEST);
        assert_eq!(h["authorization"], "Bearer old-access");
        assert_eq!(h["ccsp-service-id"], CCSP_SERVICE_ID);
        assert_eq!(h["ccsp-application-id"], APP_ID);
        assert_eq!(h["ccsp-device-id"], "dev-1");
        assert_eq!(h["ccuccs2protocolsupport"], "1");
        assert_eq!(h["user-agent"], USER_AGENT);
        assert!(h.contains_key("stamp"));
        assert_eq!(k.vehicle().await.unwrap(), Vehicle { id: "veh-ev3".into(), name: "EV3".into(), ccs2: 1 });
        assert_eq!(k.unavailable(), CloudReading::Ev3(Ev3Reading::default()));
        let ents = k.entities();
        assert_eq!(ents.len(), 7);
        assert!(ents.iter().any(|e| e.entity_id == AC_LIMIT && e.writable && e.world_field == Some("ev3.ac_limit")));
        assert!(ents.iter().filter(|e| e.writable).count() == 1);
        assert!(r.stored().is_none(), "a fresh token is not rewritten");
    }

    /// An expired legacy token is refreshed with the refresh-token grant; the rotated token
    /// lands in the state file and wins over the credentials' on the next start.
    #[tokio::test]
    async fn refreshes_legacy_token_and_persists() {
        let r = rig().await;
        let mut k = r.kia(expired(fresh_token()));
        k.status().await.unwrap();
        let (_, form) = r.log.last(TOKEN);
        assert!(form.contains("grant_type=refresh_token"), "{form}");
        assert!(form.contains(&format!("refresh_token={REFRESH_RT}")));
        assert!(form.contains(&format!("client_id={CCSP_SERVICE_ID}")) && form.contains("client_secret=secret"));
        assert_eq!(r.log.last(LATEST).0["authorization"], "Bearer new-access");
        let stored = r.stored().unwrap();
        assert_eq!(stored.refresh_token.as_ref().unwrap().expose(), "ROTATED");
        assert!(stored.is_fresh(Utc::now()));
        let k2 = r.kia(expired(fresh_token()));
        assert_eq!(k2.token().refresh_token.as_ref().unwrap().expose(), "ROTATED");
        // A refresh answer without a new refresh token keeps the old one.
        r.script.once(TOKEN, Reply::json(200, r#"{"token_type":"Bearer","access_token":"a2"}"#));
        let mut k3 = Kia::with_credentials(
            KiaConfig { state_file: r.dir.path().join("other.json"), ..r.config() },
            creds(expired(fresh_token())),
        )
        .unwrap();
        k3.refresh().await.unwrap();
        assert_eq!(k3.token().refresh_token.as_ref().unwrap().expose(), REFRESH_RT);
    }

    /// A rejected refresh token is `Unauthorized`; a 48-character password (the legacy way of
    /// entering the refresh token) is tried next.
    #[tokio::test]
    async fn rejected_refresh_tokens() {
        let r = rig().await;
        r.script.set(TOKEN, Reply::json(400, r#"{"error":"invalid_grant","error_description":"expired"}"#));
        let mut c = creds(expired(fresh_token()));
        c.password = Secret::default();
        let mut k = Kia::with_credentials(r.config(), c).unwrap();
        assert!(matches!(k.status().await, Err(CloudError::Unauthorized(m)) if m.contains("expired")));
        assert_eq!(k.auth_status().state, "aanmelden nodig", "no password to log in with");
        let mut none = r.kia(KiaToken::default());
        assert!(matches!(none.refresh().await, Err(CloudError::Unauthorized(m)) if m.contains("no refresh token")));
        r.script.once(TOKEN, Reply::json(400, r#"{"error":"invalid_grant"}"#));
        r.script
            .once(TOKEN, Reply::json(200, r#"{"token_type":"Bearer","access_token":"via-password","expires_in":60}"#));
        let mut c = creds(expired(fresh_token()));
        c.password = Secret::new("B".repeat(48));
        let mut k = Kia::with_credentials(r.config(), c).unwrap();
        k.refresh().await.unwrap();
        assert_eq!(k.token().access_token.as_ref().unwrap().expose(), "Bearer via-password");
        assert_eq!(
            r.log.last(TOKEN).1,
            format!(
                "grant_type=refresh_token&refresh_token={}&client_id={CCSP_SERVICE_ID}&client_secret=secret",
                "B".repeat(48)
            )
        );
        r.script.once(TOKEN, Reply::json(500, "<html>"));
        assert!(
            matches!(r.kia(expired(fresh_token())).refresh().await, Err(CloudError::Unauthorized(m)) if m.contains("no access_token"))
        );
    }

    /// A token the server rejects mid-session is refreshed once; a second rejection is final.
    #[tokio::test]
    async fn retries_once_on_expired_access_token() {
        let r = rig().await;
        r.script.once(LATEST, Reply::json(401, r#"{"errId":"x"}"#));
        let mut k = r.kia(fresh_token());
        assert_eq!(k.status().await.unwrap().soc, Some(54.5));
        assert_eq!(r.log.count(TOKEN), 1);
        r.script.once(LATEST, Reply::json(200, r#"{"retCode":"F","resCode":"7501","resMsg":"expired"}"#));
        r.script.once(LATEST, Reply::json(200, r#"{"retCode":"F","resCode":"7501","resMsg":"expired"}"#));
        assert!(matches!(k.status().await, Err(CloudError::Unauthorized(_))));
    }

    /// A stale device id (`4002`) is re-registered once and persisted; a missing one is
    /// registered before the first request.
    #[tokio::test]
    async fn re_registers_the_device() {
        let r = rig().await;
        let invalid = r#"{"retCode":"F","resCode":"4002","resMsg":"Invalid request body - invalid deviceId"}"#;
        r.script.once(LATEST, Reply::json(200, invalid));
        let mut k = r.kia(fresh_token());
        k.status().await.unwrap();
        assert_eq!(k.token().device_id.as_deref(), Some("dev-2"));
        assert_eq!(r.stored().unwrap().device_id.as_deref(), Some("dev-2"));
        let (h, body) = r.log.last("POST /api/v1/spa/notifications/register");
        assert_eq!(h["ccsp-service-id"], CCSP_SERVICE_ID);
        let b: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((b["pushType"].as_str(), b["pushRegId"].as_str().unwrap().len()), (Some("APNS"), 64));
        r.script.once(LATEST, Reply::json(200, invalid));
        r.script.once(LATEST, Reply::json(200, invalid));
        assert!(matches!(k.status().await, Err(CloudError::Api { code, .. }) if code == "4002"));
        let mut t = fresh_token();
        t.device_id = None;
        let mut k = r.kia(t);
        k.status().await.unwrap();
        assert_eq!(r.log.count("POST /api/v1/spa/notifications/register"), 3);
        r.script.set("POST /api/v1/spa/notifications/register", Reply::json(200, r#"{"retCode":"S","resMsg":{}}"#));
        assert!(matches!(
            r.kia(KiaToken { device_id: None, ..fresh_token() }).status().await,
            Err(CloudError::Malformed(_))
        ));
        r.script.set("POST /api/v1/spa/notifications/register", Reply::json(200, invalid));
        assert!(
            matches!(r.kia(KiaToken { device_id: None, ..fresh_token() }).status().await, Err(CloudError::Api { code, .. }) if code == "register")
        );
        r.script.set(
            "POST /api/v1/spa/notifications/register",
            Reply::json(200, r#"{"retCode":"F","resCode":"5091","resMsg":"x"}"#),
        );
        assert!(matches!(
            r.kia(KiaToken { device_id: None, ..fresh_token() }).status().await,
            Err(CloudError::RateLimited(_))
        ));
    }

    /// Vehicle selection by nickname or VIN, the first one by default; unknown or broken
    /// listings and non-CCS2 cars are errors.
    #[tokio::test]
    async fn vehicle_selection() {
        let r = rig().await;
        let mut k = Kia::with_credentials(KiaConfig { vehicle: None, ..r.config() }, creds(fresh_token())).unwrap();
        assert_eq!(k.vehicle().await.unwrap().id, "veh-other");
        let mut k = Kia::with_credentials(
            KiaConfig { vehicle: Some("VINPLACEHOLDER001".into()), ..r.config() },
            creds(fresh_token()),
        )
        .unwrap();
        assert_eq!(k.vehicle().await.unwrap().id, "veh-ev3");
        let mut k =
            Kia::with_credentials(KiaConfig { vehicle: Some("Niro".into()), ..r.config() }, creds(fresh_token()))
                .unwrap();
        assert!(matches!(k.vehicle().await, Err(CloudError::NotReady(_))));
        r.script.set(
            "GET /api/v1/spa/vehicles",
            Reply::json(
                200,
                r#"{"retCode":"S","resMsg":{"vehicles":[{"nickname":"EV3","ccuCCS2ProtocolSupport":0}]}}"#,
            ),
        );
        assert!(matches!(r.kia(fresh_token()).vehicle().await, Err(CloudError::Malformed(_))));
        r.script.set(
            "GET /api/v1/spa/vehicles",
            Reply::json(200, r#"{"retCode":"S","resMsg":{"vehicles":[{"vehicleId":"v","nickname":"EV3"}]}}"#),
        );
        assert!(matches!(r.kia(fresh_token()).status().await, Err(CloudError::Malformed(m)) if m.contains("CCS2")));
    }

    /// Failures put `read` in backoff; the rate limit code is its own error.
    #[tokio::test]
    async fn read_backs_off_after_errors() {
        let r = rig().await;
        r.script
            .set(LATEST, Reply::json(200, r#"{"retCode":"F","resCode":"5091","resMsg":"Exceeds number of requests"}"#));
        let mut k = r.kia(fresh_token());
        assert!(matches!(k.read().await, Err(CloudError::RateLimited(_))));
        assert!(matches!(k.read().await, Err(CloudError::Backoff(_))));
        assert_eq!(r.log.count(LATEST), 1);
    }

    /// The AC limit setter: validates, skips an unchanged value, keeps the DC limit, and
    /// updates the cached reading. Only ever sent to the mock.
    #[tokio::test]
    async fn sets_the_ac_charge_limit() {
        let r = rig().await;
        let mut k = r.kia(fresh_token());
        assert!(matches!(k.set_ac_limit(45).await, Err(CloudError::OutOfRange(_))));
        assert!(matches!(k.set_ac_limit(85).await, Err(CloudError::OutOfRange(_))));
        assert_eq!(k.apply(&CloudCommand::Ev3AcLimit(100)).await.unwrap(), Applied::Unchanged);
        k.read().await.unwrap();
        assert_eq!(k.apply(&CloudCommand::Ev3AcLimit(80)).await.unwrap(), Applied::Done);
        let (h, body) = r.log.last("POST /api/v1/spa/vehicles/veh-ev3/charge/target");
        let b: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            b,
            json!({"targetSOClist": [{"plugType": 0, "targetSOClevel": 100}, {"plugType": 1, "targetSOClevel": 80}]})
        );
        assert_eq!(h["authorization"], "Bearer old-access");
        let CloudReading::Ev3(ev3) = k.read().await.unwrap() else { panic!() };
        assert_eq!(ev3.ac_limit, Some(80.0));
        let off = CloudCommand::Airco { unit: energie_core::command::AircoUnit::Beneden, on: false };
        assert_eq!(k.apply(&off).await.unwrap(), Applied::NotMine);
        let mut v: Value = serde_json::from_str(STATUS).unwrap();
        v["resMsg"]["state"]["Vehicle"]["Green"]["ChargingInformation"]["TargetSoC"]["Quick"] = Value::Null;
        r.script.set(LATEST, Reply::json(200, v.to_string()));
        assert!(matches!(r.kia(fresh_token()).set_ac_limit(80).await, Err(CloudError::NotReady(_))));
        r.script.set(
            "POST /api/v1/spa/vehicles/veh-ev3/charge/target",
            Reply::json(200, r#"{"retCode":"F","resCode":"4004","resMsg":"Duplicate request"}"#),
        );
        r.script.set(LATEST, Reply::json(200, STATUS));
        assert!(matches!(r.kia(fresh_token()).set_ac_limit(90).await, Err(CloudError::Api { .. })));
    }

    fn cci_token() -> KiaToken {
        KiaToken {
            cci_access_token: Some("Bearer cci-a".into()),
            refresh_token: Some("cci-r".into()),
            non_ccs_token: Some("non-ccs".into()),
            exchangeable_token: Some("exch".into()),
            id_token: Some("idt".into()),
            ..expired(fresh_token())
        }
    }

    /// The OneApp token set: signed refresh, `t=` cookie, CCS exchange, persisted.
    #[tokio::test]
    async fn refreshes_cci_token_set() {
        let r = rig().await;
        r.script.set(
            "POST /auth/v2/token-refresh",
            Reply::json(200, r#"{"accessToken":"cci-a2","refreshToken":"cci-r2","nonCcsToken":"non-ccs2"}"#)
                .header("set-cookie", "t=exch2; Path=/; HttpOnly"),
        );
        r.script
            .set("POST /auth/v1/token-exchange", Reply::json(200, r#"{"accessToken":"ccs-1","expiresTime":86400}"#));
        let mut k = r.kia(cci_token());
        k.status().await.unwrap();
        let (h, body) = r.log.last("POST /auth/v2/token-refresh");
        let b: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (b["accessToken"].as_str(), b["refreshToken"].as_str(), b["idToken"].as_str()),
            (Some("cci-a"), Some("cci-r"), Some("idt"))
        );
        assert_eq!(h["client-id"], CCI_CLIENT_ID);
        assert_eq!(h["x-fingerprint"], fingerprint(h["x-timestamp"].to_str().unwrap(), &body).as_str());
        assert!(h["app-request-id"].to_str().unwrap().ends_with("_dev1_AOS"));
        let (h, _) = r.log.last("POST /auth/v1/token-exchange?serviceType=CCS");
        assert_eq!(h["authorization"], "Bearer cci-a2");
        assert_eq!(h["authentication"], "non-ccs2");
        assert_eq!(h["exchangeable-token"], "exch2");
        assert_eq!(r.log.last(LATEST).0["authorization"], "Bearer ccs-1");
        let t = r.stored().unwrap();
        assert_eq!(t.refresh_token.unwrap().expose(), "cci-r2");
        assert_eq!(t.id_token.unwrap().expose(), "idt");
    }

    /// CCI failures: a rejected refresh, a failed exchange, a cookie without a value, no
    /// exchangeable token, a legacy `ccsAccessToken`.
    #[tokio::test]
    async fn cci_failures_and_variants() {
        let r = rig().await;
        r.script.once("POST /auth/v2/token-refresh", Reply::json(401, r#"{"code":9036}"#));
        assert!(
            matches!(r.isolated(cci_token()).refresh().await, Err(CloudError::Unauthorized(m)) if m.contains("401"))
        );
        r.script.set("POST /auth/v2/token-refresh", Reply::json(200, "{}").header("set-cookie", "t=; Path=/"));
        r.script.once("POST /auth/v1/token-exchange", Reply::json(200, "{}"));
        assert!(
            matches!(r.isolated(cci_token()).refresh().await, Err(CloudError::Unauthorized(m)) if m.contains("exchange"))
        );
        r.script.once("POST /auth/v1/token-exchange", Reply::json(200, r#"{"ccsAccessToken":"ccs-2"}"#));
        let mut k = r.isolated(KiaToken { exchangeable_token: None, ..cci_token() });
        k.refresh().await.unwrap();
        assert_eq!(k.token().access_token.as_ref().unwrap().expose(), "Bearer ccs-2");
        assert!(!r.log.last("POST /auth/v1/token-exchange").0.contains_key("exchangeable-token"));
        r.script.set("POST /auth/v2/token-refresh", Reply::json(200, "not json"));
        assert!(matches!(r.isolated(cci_token()).refresh().await, Err(CloudError::Malformed(_))));
        r.script.set("POST /auth/v2/token-refresh", Reply::json(200, "{}"));
        let mut k = r.isolated(KiaToken { non_ccs_token: Some("bad\nvalue".into()), ..cci_token() });
        assert!(matches!(k.refresh().await, Err(CloudError::Malformed(_))));
    }

    /// A test RSA key (small, for speed) as the certs endpoint's JWK.
    fn rsa_key() -> (rsa::RsaPrivateKey, String) {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use rsa::traits::PublicKeyParts;
        let key = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 512).unwrap();
        let jwk = json!({"retCode": "S", "retValue": {
            "kid": "kid-1",
            "n": URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),
            "e": URL_SAFE_NO_PAD.encode(key.e().to_bytes_be()),
        }});
        (key, jwk.to_string())
    }

    const AUTHORIZE: &str = "GET /auth/api/v2/user/oauth2/authorize";
    const CERTS: &str = "GET /auth/api/v1/accounts/certs";
    const SIGNIN: &str = "POST /auth/account/signin";
    const CCI_TOKEN: &str = "POST /auth/v1/token";
    const EXCHANGE: &str = "POST /auth/v1/token-exchange";

    /// The mock answers the whole OneApp login.
    fn script_login(r: &Rig, jwk: &str) {
        r.script.set(AUTHORIZE, Reply::html("<html>sign in</html>"));
        r.script.set(CERTS, Reply::json(200, jwk));
        r.script.set(SIGNIN, Reply::redirect("https://oneapp.kia.com/redirect?code=code-1&state=ccsp"));
        r.script.set(
            CCI_TOKEN,
            Reply::json(
                200,
                r#"{"accessToken":"cci-a","refreshToken":"cci-r","nonCcsToken":"non-ccs","exchangeableAccessToken":"exch","idToken":"idt","expiresIn":3599}"#,
            ),
        );
        r.script.set(EXCHANGE, Reply::json(200, r#"{"accessToken":"ccs-1","expiresTime":86400}"#));
    }

    /// A rejected refresh token with a known password: the OneApp login (authorize with the
    /// OneApp client, RSA-encrypted password, code → CCI token set → CCS token), persisted; the
    /// vehicle read then works.
    #[tokio::test]
    async fn password_login_after_a_rejected_refresh() {
        let r = rig().await;
        let (key, jwk) = rsa_key();
        script_login(&r, &jwk);
        r.script.set(TOKEN, Reply::json(400, r#"{"error":"invalid_grant"}"#));
        let mut k = r.kia(expired(fresh_token())).with_gate(LoginGate::new(Duration::ZERO));
        let CloudReading::Ev3(ev3) = k.read().await.unwrap() else { panic!() };
        assert_eq!(ev3.soc, Some(54.5));
        let auth = k.auth().unwrap();
        assert_eq!((auth.state.as_str(), auth.credentials.as_str(), auth.can_login), ("ok", "etc", true));
        assert_eq!(auth.last_result.as_deref(), Some("ok"));

        let paths = r.log.paths();
        let auth_req = paths.iter().find(|p| p.starts_with(AUTHORIZE)).unwrap();
        assert!(auth_req.contains(&format!("client_id={ONEAPP_CLIENT_ID}")) && auth_req.contains("state=ccsp"));
        assert!(r.log.last(AUTHORIZE).0["user-agent"].to_str().unwrap().ends_with("_CCS_APP_AOS"));
        let (_, form) = r.log.last(SIGNIN);
        let form: HashMap<String, String> = url_form(&form);
        assert_eq!(form["username"], "user@example.invalid");
        assert_eq!(
            (form["encryptedPassword"].as_str(), form["kid"].as_str(), form["state"].as_str()),
            ("true", "kid-1", "ccsp")
        );
        let cipher: Vec<u8> = (0..form["password"].len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&form["password"][i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(key.decrypt(rsa::Pkcs1v15Encrypt, &cipher).unwrap(), b"not-a-token");
        assert!(!form.values().any(|v| v == "not-a-token"), "never sent in the clear");
        assert!(paths.iter().any(|p| p == "POST /auth/v1/token?code=code-1"), "{paths:?}");
        let (h, _) = r.log.last(CCI_TOKEN);
        assert_eq!(h["client-device-id"], "dev-2", "the freshly registered device");
        assert_eq!(h["x-fingerprint"], fingerprint(h["x-timestamp"].to_str().unwrap(), "").as_str());
        assert_eq!(r.log.last(LATEST).0["authorization"], "Bearer ccs-1");
        let t = r.stored().unwrap();
        assert_eq!(
            (t.refresh_token.unwrap().expose(), t.cci_access_token.unwrap().expose(), t.device_id.as_deref()),
            ("cci-r", "cci-a", Some("dev-2"))
        );
        assert!(t.password.is_none() && t.username.is_none(), "no credentials in the token file");
    }

    fn url_form(body: &str) -> HashMap<String, String> {
        reqwest::Url::parse(&format!("http://x/?{body}"))
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into(), v.into()))
            .collect()
    }

    /// Each way the sign-in ends without a code has its own reason; only the credential ones
    /// count as rejections.
    #[tokio::test]
    async fn password_login_failures() {
        let r = rig().await;
        let (_, jwk) = rsa_key();
        script_login(&r, &jwk);
        let login = async |r: &Rig| {
            let e = r.isolated(KiaToken::default()).login().await.unwrap_err();
            (login::is_rejection(&e), e.to_string())
        };
        r.script.once(
            SIGNIN,
            Reply::redirect("https://oneapp.kia.com/redirect?error=access_denied&error_description=invalid%20password"),
        );
        let (rej, m) = login(&r).await;
        assert!(rej && m.contains("invalid password"), "{m}");
        r.script.once(SIGNIN, Reply::redirect("/web/v1/user/authorization?x=1"));
        let (rej, m) = login(&r).await;
        assert!(rej && m.contains("accept new terms"), "{m}");
        r.script.once(SIGNIN, Reply::redirect("/auth/api/v2/user/oauth2/authorize?again"));
        let (rej, m) = login(&r).await;
        assert!(rej && m.contains("login page"), "{m}");
        r.script.once(SIGNIN, Reply::redirect("/somewhere/else?x=secret"));
        let (rej, m) = login(&r).await;
        assert!(rej && m.contains("/somewhere/else") && !m.contains("secret"), "{m}");
        r.script.once(SIGNIN, Reply::html("form again"));
        let (rej, m) = login(&r).await;
        assert!(rej && m.contains("HTTP 200"), "{m}");
        r.script.once(SIGNIN, Reply::redirect("http://[bad"));
        let (rej, m) = login(&r).await;
        assert!(!rej && m.contains("not a URL"), "{m}");
        r.script.once(AUTHORIZE, Reply::html("You are abusing our service"));
        let (rej, m) = login(&r).await;
        assert!(!rej && m.contains("WAF"), "{m}");
        r.script.once(CERTS, Reply::json(500, "{}"));
        let (rej, m) = login(&r).await;
        assert!(!rej && m.contains("no RSA key"), "{m}");
        r.script.once(CERTS, Reply::json(200, r#"{"retValue":{"n":"!!","e":"AQAB"}}"#));
        let (rej, m) = login(&r).await;
        assert!(!rej && m.contains("base64url"), "{m}");
        r.script.once(CCI_TOKEN, Reply::json(401, "nope"));
        let (rej, m) = login(&r).await;
        assert!(!rej && m.contains("CCI token exchange: HTTP 401"), "{m}");
        r.script.once(EXCHANGE, Reply::json(500, "{}"));
        let (rej, m) = login(&r).await;
        assert!(!rej && m.contains("CCS token exchange"), "{m}");
        r.script.once(CCI_TOKEN, Reply::json(200, r#"{"accessToken":"a","nonCcsToken":"bad\nvalue"}"#));
        assert!(matches!(r.isolated(KiaToken::default()).login().await, Err(CloudError::Malformed(_))));
        r.script.once(CCI_TOKEN, Reply::json(200, "not json"));
        assert!(matches!(r.isolated(KiaToken::default()).login().await, Err(CloudError::Malformed(_))));
        // A failed device register falls back to a random identity.
        r.script.once("POST /api/v1/spa/notifications/register", Reply::json(500, "{}"));
        let mut k = r.isolated(KiaToken::default());
        k.login().await.unwrap();
        assert_eq!(k.token().device_id.as_ref().unwrap().len(), 36);
        // Without a plaintext password there is no login.
        let mut c = creds(KiaToken::default());
        c.password = Secret::new("C".repeat(48));
        let mut k = Kia::with_credentials(r.config(), c).unwrap();
        assert!(matches!(k.login().await, Err(CloudError::Unauthorized(m)) if m.contains("no username")));
        assert!(matches!(encrypt_password("AQAB", "AQ", "x"), Err(CloudError::Malformed(m)) if m.contains("RSA key")));
    }

    /// Three rejected logins block the gate (one per gap, none after that) until new
    /// credentials arrive; those are stored 0600, win over the file, and log in at once.
    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_logins_block_until_new_credentials() {
        use std::os::unix::fs::PermissionsExt;
        let r = rig().await;
        let (_, jwk) = rsa_key();
        script_login(&r, &jwk);
        r.script.set(TOKEN, Reply::json(400, r#"{"error":"invalid_grant"}"#));
        r.script.set(SIGNIN, Reply::redirect("https://oneapp.kia.com/redirect?error=x&error_description=bad"));
        std::fs::write(r.config().credentials, serde_json::to_string(&creds(expired(fresh_token()))).unwrap()).unwrap();
        let mut k = Kia::new(r.config()).unwrap().with_gate(LoginGate::new(Duration::ZERO));
        assert_eq!(k.auth_status().credentials, "etc");
        for _ in 0..3 {
            k.renew().await.unwrap_err();
        }
        assert_eq!(r.log.count(SIGNIN), 3);
        let e = k.renew().await.unwrap_err();
        assert!(matches!(&e, CloudError::Unauthorized(m) if m.contains("3 rejected")), "{e}");
        assert_eq!(r.log.count(SIGNIN), 3, "blocked: no fourth attempt");
        let st = k.auth_status();
        assert_eq!((st.state.as_str(), st.blocked, st.rejected_logins), ("geblokkeerd", true, 3));
        let mut slow = r.isolated(expired(fresh_token()));
        slow.renew().await.unwrap_err();
        assert!(matches!(slow.renew().await, Err(CloudError::Backoff(_))), "one login per 30 min");

        r.script.set(SIGNIN, Reply::redirect("https://oneapp.kia.com/redirect?code=code-2"));
        let m = |v: Value| v.as_object().unwrap().clone();
        let st = k.set_credentials(&m(json!({"username": "new@example.invalid", "password": "pw-2"}))).await.unwrap();
        assert_eq!((st.state.as_str(), st.credentials.as_str(), st.rejected_logins), ("ok", "state", 0));
        let path = r.config().credentials_store.unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((stored["username"].as_str(), stored["region"].as_i64()), (Some("new@example.invalid"), Some(1)));
        assert!(stored.get("token").is_none());
        let again = Kia::new(r.config()).unwrap();
        assert_eq!(
            (again.auth_status().credentials.as_str(), again.creds.username.expose()),
            ("state", "new@example.invalid")
        );

        // A refresh token as the password is refreshed with, not logged in with.
        r.script.set(TOKEN, Reply::json(200, r#"{"token_type":"Bearer","access_token":"rt","expires_in":60}"#));
        let st =
            k.set_credentials(&m(json!({"username": "u", "password": "D".repeat(48), "pin": "1234"}))).await.unwrap();
        assert_eq!((st.state.as_str(), st.can_login), ("ok", false));
        assert!(r.log.last(TOKEN).1.contains(&"D".repeat(48)));
        for bad in [json!({"username": "u"}), json!({"username": "u", "password": "p", "vin": "x"})] {
            assert!(matches!(k.set_credentials(&m(bad)).await, Err(CloudError::Config(_))));
        }
        let mut no_store =
            Kia::with_credentials(KiaConfig { credentials_store: None, ..r.config() }, creds(KiaToken::default()))
                .unwrap();
        let e = no_store.set_credentials(&m(json!({"username": "u", "password": "p"}))).await.unwrap_err();
        assert!(matches!(e, CloudError::Config(m) if m.contains("state dir")));
    }
}
