//! Fluvius (Mijn Fluvius portal, `fluvius` custom integration): the monthly capacity peak,
//! `sensor.fluvius_meter_<serial>_peak_power`. Read-only, display only.
//!
//! `GET https://mijn.fluvius.be/verbruik/api/meter-measurement-spikes/{ean}` with
//! `historyFrom` (1 January, local midnight) and `historyUntil` (today 23:59:59.999), ISO 8601
//! with milliseconds and offset, plus `asServiceProvider=false` and `meterSerialNumber`. The
//! answer is a list of months `{"d", "de", "v": [{"v": kW, "sst", "set"}]}`; the sensor is the
//! latest month's peak.
//!
//! Auth is Azure AD B2C with PKCE, done the way the browser does it: the MSAL config
//! (`/api/global/msal/config`) names the client id, authority and scopes; the authorize page
//! carries `var SETTINGS = {csrf, transId, hosts}` and `var SA_FIELDS`; the credentials go to
//! `{tenant}/SelfAsserted`, `{tenant}/api/CombinedSigninAndSignup/confirmed` redirects to the
//! portal with the code, and `{authority}/oauth2/v2.0/token` exchanges it. Later the refresh
//! token is used (`grant_type=refresh_token`); B2C rotates it, so the token set is kept in a state
//! file (`{refresh_token, access_token, access_expires_at}`, the integration's token store).
//! `invalid_grant` falls back to a full login; other token-endpoint failures are transient.
//!
//! The credentials file is the integration's two config entries as a JSON array (electricity
//! and gas); the peak exists for the electricity meter only. Polling: every 6 hours by default
//! (HA: hourly), at least hourly, with 15 minute → 6 hour backoff.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use reqwest::Url;
use reqwest::cookie::Jar;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{CloudError, Result};
use crate::poll::{Gate, Poller};
use crate::source::{CloudReading, CloudSource, EntityDescriptor, EntityKind, EntityState, num};
use crate::store::{Secret, StateFile, load_json};
use crate::{html, http};

pub const MSAL_CONFIG_URL: &str = "https://mijn.fluvius.be/api/global/msal/config";
pub const API_BASE: &str = "https://mijn.fluvius.be";
const DEFAULT_AUTHORITY: &str = "https://login.fluvius.be/klanten.onmicrosoft.com/B2C_1A_customer_signup_signin";
const DEFAULT_REDIRECT_URI: &str = "https://mijn.fluvius.be/";
const DEFAULT_SCOPE: &str = "https://klanten.onmicrosoft.com/MijnFluvius/user_impersonation";
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Floor of the poll interval.
pub const MIN_POLL: Duration = Duration::from_secs(3600);

/// One config entry's `data`.
#[derive(Debug, Clone, Deserialize)]
pub struct FluviusEntry {
    pub email: Secret,
    pub password: Secret,
    pub ean: String,
    pub meter_serial: String,
    #[serde(default = "default_meter_type")]
    pub meter_type: String,
}

fn default_meter_type() -> String {
    "electricity".into()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct FluviusEndpoints {
    pub msal_config: String,
    pub api_base: String,
}

impl Default for FluviusEndpoints {
    fn default() -> Self {
        FluviusEndpoints { msal_config: MSAL_CONFIG_URL.into(), api_base: API_BASE.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct FluviusConfig {
    /// The config entries as a JSON array (`/etc/energie/cloud-fluvius.json`).
    pub credentials: PathBuf,
    /// Where the rotating token set is kept (`/var/lib/energie/fluvius-token.json`).
    pub state_file: PathBuf,
    /// The EAN to read; the first electricity entry when absent.
    #[serde(default)]
    pub ean: Option<String>,
    /// Seconds between reads; raised to [`MIN_POLL`] when lower.
    #[serde(default = "default_poll")]
    pub poll_interval_s: u64,
    #[serde(default)]
    pub endpoints: FluviusEndpoints,
}

fn default_poll() -> u64 {
    6 * 3600
}

/// The token store, in the integration's shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FluviusTokens {
    pub refresh_token: Option<Secret>,
    pub access_token: Option<Secret>,
    /// Unix seconds.
    pub access_expires_at: f64,
}

/// One month's peak.
#[derive(Debug, Clone, PartialEq)]
pub struct Peak {
    pub period_start: DateTime<FixedOffset>,
    pub period_end: DateTime<FixedOffset>,
    pub spike_start: DateTime<FixedOffset>,
    pub spike_end: DateTime<FixedOffset>,
    pub kw: f64,
}

/// The capacity peak as the dashboard sees it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FluviusReading {
    /// `sensor.fluvius_meter_<serial>_peak_power`, kW: the latest month's peak.
    pub peak_kw: Option<f64>,
    /// The latest month and its peak quarter hour.
    pub latest: Option<Peak>,
    /// `monthly_peaks_kw` attribute: `("2026-11", 3.892)`, oldest first, at most 12.
    pub monthly_kw: Vec<(String, f64)>,
    /// Lowercase meter serial, for the entity id.
    pub meter: String,
}

impl FluviusReading {
    pub fn world(&self) -> energie_core::world::Fluvius {
        energie_core::world::Fluvius { peak_kw: self.peak_kw }
    }

    pub fn entity_id(meter: &str) -> String {
        format!("sensor.fluvius_meter_{}_peak_power", meter.to_lowercase())
    }

    pub fn entity_states(&self) -> Vec<EntityState> {
        vec![num(&Self::entity_id(&self.meter), self.peak_kw)]
    }
}

fn parse_time(v: &Value) -> Option<DateTime<FixedOffset>> {
    let s = v.as_str()?.replace('Z', "+00:00");
    DateTime::parse_from_rfc3339(&s).ok().or_else(|| {
        chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|t| t.and_utc().fixed_offset())
    })
}

/// Parse a `meter-measurement-spikes` answer into peaks, oldest first.
pub fn parse_spikes(v: &Value) -> Result<Vec<Peak>> {
    let chunks = v.as_array().ok_or_else(|| CloudError::Malformed("spikes: expected a list".into()))?;
    let mut peaks = Vec::new();
    for chunk in chunks {
        let Some(start) = parse_time(&chunk["d"]) else { continue };
        let end = parse_time(&chunk["de"]).unwrap_or(start);
        for r in chunk["v"].as_array().into_iter().flatten() {
            let (Some(ss), Some(se)) = (parse_time(&r["sst"]), parse_time(&r["set"])) else { continue };
            let kw = r["v"].as_f64().or_else(|| r["v"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0.0);
            peaks.push(Peak { period_start: start, period_end: end, spike_start: ss, spike_end: se, kw });
        }
    }
    peaks.sort_by_key(|p| p.period_start);
    Ok(peaks)
}

/// The reading for `peaks` of meter `meter`.
pub fn reading(meter: &str, peaks: &[Peak]) -> FluviusReading {
    let latest = peaks.last().cloned();
    let monthly_kw = peaks
        .iter()
        .rev()
        .take(12)
        .rev()
        .map(|p| (p.spike_start.format("%Y-%m").to_string(), (p.kw * 1000.0).round() / 1000.0))
        .collect();
    FluviusReading { peak_kw: latest.as_ref().map(|p| p.kw), latest, monthly_kw, meter: meter.to_lowercase() }
}

/// `historyFrom`/`historyUntil` for `now`: 1 January local midnight to today 23:59:59.999.
pub fn spike_range(now: DateTime<Utc>, tz: Tz) -> (String, String) {
    let local = now.with_timezone(&tz);
    let at = |d: NaiveDate, h, m, s, ms| {
        let naive = d.and_hms_milli_opt(h, m, s, ms).expect("valid time");
        tz.from_local_datetime(&naive)
            .earliest()
            .expect("no gap at these hours")
            .format("%Y-%m-%dT%H:%M:%S%.3f%:z")
            .to_string()
    };
    let jan1 = NaiveDate::from_ymd_opt(local.year(), 1, 1).expect("valid date");
    (at(jan1, 0, 0, 0, 0), at(local.date_naive(), 23, 59, 59, 999))
}

/// MSAL settings from the portal: authority, client id, redirect URI, scopes.
#[derive(Debug, Clone, PartialEq)]
pub struct Msal {
    pub authority: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: String,
}

/// `_resolve_msal_settings` and `_normalise_scopes`.
pub fn msal_settings(m: &Value) -> Result<Msal> {
    let pick = |k: &str| m[k].as_str().or(m["auth"][k].as_str()).map(str::to_string);
    let authority = pick("authority").unwrap_or_else(|| DEFAULT_AUTHORITY.into()).trim_end_matches('/').to_string();
    let client_id = pick("clientId").ok_or_else(|| CloudError::Malformed("MSAL config has no clientId".into()))?;
    let redirect_uri = pick("redirectUri").unwrap_or_else(|| DEFAULT_REDIRECT_URI.into());
    let mut scopes: Vec<String> = Vec::new();
    let mut add = |v: &Value| match v {
        Value::String(s) => scopes.extend(s.split_whitespace().map(str::to_string)),
        Value::Array(a) => scopes.extend(a.iter().filter_map(Value::as_str).map(str::to_string)),
        _ => {}
    };
    for key in ["scopes", "defaultScopes", "apiScopes"] {
        add(&m[key]);
    }
    add(&m["authRequest"]["scopes"]);
    if let Some(map) = m["protectedResourceMap"].as_object() {
        map.values().for_each(&mut add);
    }
    scopes.extend(["openid", "offline_access", DEFAULT_SCOPE].map(str::to_string));
    let mut unique: Vec<String> = Vec::new();
    for s in scopes {
        if !unique.contains(&s) {
            unique.push(s);
        }
    }
    Ok(Msal { authority, client_id, redirect_uri, scopes: unique.join(" ") })
}

/// A PKCE verifier and its S256 challenge.
pub fn pkce() -> (String, String) {
    let verifier = http::random_urlsafe(64);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Cloud client for the capacity peak of one electricity meter.
pub struct Fluvius {
    endpoints: FluviusEndpoints,
    entry: FluviusEntry,
    state: StateFile,
    tokens: FluviusTokens,
    tz: Tz,
    client: reqwest::Client,
    no_redirect: reqwest::Client,
    poller: Poller<FluviusReading>,
}

impl Fluvius {
    pub fn new(cfg: FluviusConfig) -> Result<Self> {
        let entries: Vec<FluviusEntry> = load_json(&cfg.credentials)?;
        Fluvius::with_entries(cfg, entries)
    }

    /// Pick the configured EAN, else the first electricity entry.
    pub fn with_entries(cfg: FluviusConfig, entries: Vec<FluviusEntry>) -> Result<Self> {
        let entry = entries
            .into_iter()
            .find(|e| match &cfg.ean {
                Some(ean) => &e.ean == ean,
                None => e.meter_type != "gas",
            })
            .ok_or_else(|| {
                CloudError::Config(format!("no electricity entry (ean {:?}) in the credentials", cfg.ean))
            })?;
        if entry.meter_type == "gas" {
            return Err(CloudError::Config("a gas meter has no capacity peak".into()));
        }
        let state = StateFile::new(&cfg.state_file)?;
        let tokens = state.load()?.unwrap_or_default();
        let jar = Arc::new(Jar::default());
        let interval = Duration::from_secs(cfg.poll_interval_s).max(MIN_POLL);
        Ok(Fluvius {
            endpoints: cfg.endpoints,
            entry,
            state,
            tokens,
            tz: chrono_tz::Europe::Brussels,
            client: http::client(http::TIMEOUT, Some(jar.clone()), true)?,
            no_redirect: http::client(http::TIMEOUT, Some(jar), false)?,
            poller: Poller::new(interval, Duration::from_secs(900), Duration::from_secs(6 * 3600)),
        })
    }

    pub fn poll_interval(&self) -> Duration {
        self.poller.interval()
    }

    pub fn tokens(&self) -> &FluviusTokens {
        &self.tokens
    }

    async fn msal(&self) -> Result<Msal> {
        let resp = self.client.get(&self.endpoints.msal_config).header("User-Agent", USER_AGENT).send().await?;
        if !resp.status().is_success() {
            return Err(CloudError::Status(resp.status().as_u16()));
        }
        msal_settings(&serde_json::from_str(&resp.text().await?)?)
    }

    fn store(&mut self, v: &Value) -> Result<String> {
        let access = v["access_token"].as_str().filter(|s| !s.is_empty());
        let access = access.ok_or_else(|| CloudError::Malformed("token response without access_token".into()))?;
        let expires_in = v["expires_in"].as_f64().or_else(|| v["expires_in"].as_str().and_then(|s| s.parse().ok()));
        self.tokens.access_token = Some(Secret::new(access));
        self.tokens.access_expires_at = Utc::now().timestamp() as f64 + (expires_in.unwrap_or(3600.0) - 60.0).max(0.0);
        if let Some(rt) = v["refresh_token"].as_str().filter(|s| !s.is_empty()) {
            self.tokens.refresh_token = Some(Secret::new(rt));
        }
        self.state.save(&self.tokens)?;
        Ok(access.to_string())
    }

    /// A usable access token: the stored one, a refreshed one, or a full login.
    pub async fn access_token(&mut self) -> Result<String> {
        let now = Utc::now().timestamp() as f64;
        if let Some(t) = self.tokens.access_token.as_ref().filter(|_| self.tokens.access_expires_at > now) {
            return Ok(t.expose().to_string());
        }
        let msal = self.msal().await?;
        if let Some(rt) = self.tokens.refresh_token.clone() {
            let form = [
                ("client_id", msal.client_id.as_str()),
                ("scope", msal.scopes.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", rt.expose()),
            ];
            let resp = self.client.post(format!("{}/oauth2/v2.0/token", msal.authority)).form(&form).send().await?;
            let status = resp.status().as_u16();
            let text = resp.text().await?;
            match status {
                200 => return self.store(&serde_json::from_str(&text)?),
                400 if text.contains("invalid_grant") => {
                    self.tokens.refresh_token = None;
                    self.state.save(&self.tokens)?;
                }
                s => return Err(CloudError::Api { code: format!("token {s}"), message: http::snippet(&text, 200) }),
            }
        }
        let tokens = self.login(&msal).await?;
        self.store(&tokens)
    }

    /// The full B2C login; returns the token response.
    pub async fn login(&self, msal: &Msal) -> Result<Value> {
        let (verifier, challenge) = pkce();
        let state = http::random_urlsafe(24);
        let mut authorize = http::parse_url(&format!("{}/oauth2/v2.0/authorize", msal.authority))?;
        authorize
            .query_pairs_mut()
            .append_pair("client_id", &msal.client_id)
            .append_pair("redirect_uri", &msal.redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("response_mode", "query")
            .append_pair("scope", &msal.scopes)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("nonce", &http::random_urlsafe(24))
            .append_pair("prompt", "login")
            .append_pair("client_info", "1")
            .append_pair("login_hint", self.entry.email.expose());
        let resp = self.client.get(authorize).header("User-Agent", USER_AGENT).send().await?;
        if !resp.status().is_success() {
            return Err(CloudError::Status(resp.status().as_u16()));
        }
        let page_url = resp.url().clone();
        let page = resp.text().await?;
        let missing = |what: &str| CloudError::Malformed(format!("B2C page without {what}"));
        let settings = html::js_var(&page, "SETTINGS").ok_or_else(|| missing("SETTINGS"))?;
        let fields = html::js_var(&page, "SA_FIELDS").ok_or_else(|| missing("SA_FIELDS"))?;
        let attrs = fields["AttributeFields"].as_array().cloned().unwrap_or_default();
        let login_field = attrs.first().and_then(|a| a["ID"].as_str()).ok_or_else(|| missing("a login field"))?;
        let password_field = attrs
            .iter()
            .find(|a| a["IS_PASSWORD"].as_bool() == Some(true))
            .and_then(|a| a["ID"].as_str())
            .ok_or_else(|| missing("a password field"))?;
        let get = |k: &str| settings[k].as_str().map(str::to_string);
        let (Some(csrf), Some(tx), Some(policy), Some(tenant)) =
            (get("csrf"), get("transId"), settings["hosts"]["policy"].as_str(), settings["hosts"]["tenant"].as_str())
        else {
            return Err(missing("csrf/transId/hosts"));
        };
        let origin = http::origin(&page_url);
        let tenant_base = if tenant.starts_with("http") {
            tenant.trim_end_matches('/').to_string()
        } else {
            format!("{origin}/{}", tenant.trim_matches('/'))
        };
        let tenant_url = http::parse_url(&tenant_base)?;
        let tenant_origin = http::origin(&tenant_url);
        let form = [
            ("request_type", "RESPONSE"),
            (login_field, self.entry.email.expose()),
            (password_field, self.entry.password.expose()),
        ];
        let resp = self
            .client
            .post(format!("{tenant_base}/SelfAsserted"))
            .query(&[("tx", tx.as_str()), ("p", policy)])
            .header("X-CSRF-TOKEN", &csrf)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Accept", "application/json, text/javascript, */*; q=0.01")
            .header("Origin", tenant_origin)
            .header("Referer", &tenant_base)
            .form(&form)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(CloudError::Status(resp.status().as_u16()));
        }
        let v: Value = serde_json::from_str(&resp.text().await?).unwrap_or_default();
        let status = v["status"].as_str().map(str::to_string).unwrap_or_else(|| v["status"].to_string());
        if status != "200" && status != "success" {
            let why = v["message"].as_str().unwrap_or("credentials rejected").to_string();
            return Err(CloudError::Unauthorized(why));
        }
        let api = get("api").unwrap_or_else(|| "CombinedSigninAndSignup".into());
        let mut next = http::parse_url(&format!("{tenant_base}/api/{}/confirmed", api.trim_matches('/')))?;
        next.query_pairs_mut()
            .append_pair("rememberMe", "false")
            .append_pair("csrf_token", &csrf)
            .append_pair("tx", &tx)
            .append_pair("p", policy);
        let base = http::parse_url(&format!("{origin}/"))?;
        let (code, redirect_seen) = self.follow_for_code(next, &base, &state).await?;
        let redirect = if msal.redirect_uri.is_empty() { redirect_seen } else { msal.redirect_uri.clone() };
        let form = [
            ("client_id", msal.client_id.as_str()),
            ("scope", msal.scopes.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier.as_str()),
        ];
        let resp = self.client.post(format!("{}/oauth2/v2.0/token", msal.authority)).form(&form).send().await?;
        let status = resp.status().as_u16();
        let text = resp.text().await?;
        if status != 200 {
            return Err(CloudError::Api { code: format!("token {status}"), message: http::snippet(&text, 200) });
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// Follow at most six redirects by hand until one carries `code`.
    async fn follow_for_code(&self, mut next: Url, base: &Url, state: &str) -> Result<(String, String)> {
        for _ in 0..6 {
            let resp = self.no_redirect.get(next.clone()).header("User-Agent", USER_AGENT).send().await?;
            if !resp.status().is_redirection() {
                return Err(CloudError::Unauthorized(format!(
                    "sign-in did not redirect (HTTP {})",
                    resp.status().as_u16()
                )));
            }
            let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            let url = base.join(&location).map_err(|e| CloudError::Malformed(format!("redirect: {e}")))?;
            if let Some(code) = http::query_param(&url, "code") {
                if http::query_param(&url, "state").is_some_and(|s| s != state) {
                    return Err(CloudError::Unauthorized("B2C returned another state".into()));
                }
                return Ok((code, format!("{}{}", http::origin(&url), url.path())));
            }
            next = url;
        }
        Err(CloudError::Unauthorized("no authorization code after six redirects".into()))
    }

    /// One `meter-measurement-spikes` request at `now` (no rate limit; [`CloudSource::read`]
    /// has one). A 401 gets a new token and one retry.
    pub async fn peaks_at(&mut self, now: DateTime<Utc>) -> Result<Vec<Peak>> {
        let (from, until) = spike_range(now, self.tz);
        let url = format!(
            "{}/verbruik/api/meter-measurement-spikes/{}",
            self.endpoints.api_base.trim_end_matches('/'),
            self.entry.ean
        );
        let mut retried = false;
        loop {
            let token = self.access_token().await?;
            let resp = self
                .client
                .get(&url)
                .query(&[
                    ("historyFrom", from.as_str()),
                    ("historyUntil", until.as_str()),
                    ("asServiceProvider", "false"),
                    ("meterSerialNumber", self.entry.meter_serial.as_str()),
                ])
                .bearer_auth(token)
                .header("Accept", "application/json")
                .send()
                .await?;
            let status = resp.status().as_u16();
            match status {
                200 => return parse_spikes(&serde_json::from_str(&resp.text().await?)?),
                401 if !retried => {
                    self.tokens.access_token = None;
                    retried = true;
                }
                401 => return Err(CloudError::Unauthorized("Fluvius rejected a fresh access token".into())),
                s => return Err(CloudError::Status(s)),
            }
        }
    }
}

#[async_trait]
impl CloudSource for Fluvius {
    fn name(&self) -> &'static str {
        "fluvius"
    }

    fn entities(&self) -> Vec<EntityDescriptor> {
        let id = FluviusReading::entity_id(&self.entry.meter_serial);
        vec![EntityDescriptor::new(id, EntityKind::Sensor, Some("kW")).feeds("fluvius.peak_kw")]
    }

    async fn read(&mut self) -> Result<CloudReading> {
        match self.poller.gate(Instant::now()) {
            Gate::Cached(r) => return Ok(CloudReading::Fluvius(r.clone())),
            Gate::Wait(d) => return Err(CloudError::Backoff(d)),
            Gate::Fetch => {}
        }
        let meter = self.entry.meter_serial.clone();
        let outcome = self.peaks_at(Utc::now()).await.map(|p| reading(&meter, &p));
        self.poller.record(Instant::now(), outcome).map(CloudReading::Fluvius)
    }

    fn unavailable(&self) -> CloudReading {
        CloudReading::Fluvius(FluviusReading {
            meter: self.entry.meter_serial.to_lowercase(),
            ..FluviusReading::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::{Log, Mock, Reply, Script};
    use serde_json::json;

    /// Month peaks as HA's `monthly_peaks_kw` attribute held them on 2026-11-07 (November's
    /// quarter hour as HA showed it, the other quarter hours placeholders), newest first.
    const SPIKES: &str = include_str!("../fixtures/fluvius_spikes.json");
    const TENANT: &str = "/klanten.onmicrosoft.com/B2C_1A_customer_signup_signin";
    const SPIKES_PATH: &str = "GET /verbruik/api/meter-measurement-spikes/541400000000000000";

    fn entries() -> Vec<FluviusEntry> {
        let e = |ean: &str, serial: &str, kind: &str| FluviusEntry {
            email: "user@example.invalid".into(),
            password: "hunter2".into(),
            ean: ean.into(),
            meter_serial: serial.into(),
            meter_type: kind.into(),
        };
        vec![e("541400000000000001", "7AMX0000000000", "gas"), e("541400000000000000", "1SAG0000000000", "electricity")]
    }

    fn t(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn parses_spikes_like_the_sensor() {
        let peaks = parse_spikes(&serde_json::from_str(SPIKES).unwrap()).unwrap();
        assert_eq!(peaks.len(), 11);
        let r = reading("1SAG0000000000", &peaks);
        assert_eq!(r.peak_kw, Some(3.892));
        let latest = r.latest.clone().unwrap();
        assert_eq!(latest.period_start.to_rfc3339(), "2026-11-01T00:00:00+01:00");
        assert_eq!(latest.spike_start.with_timezone(&Utc).to_rfc3339(), "2026-11-01T13:45:00+00:00");
        assert_eq!(r.monthly_kw.first().unwrap(), &("2026-01".to_string(), 6.748));
        assert_eq!(r.monthly_kw.len(), 11);
        assert_eq!(r.world(), energie_core::world::Fluvius { peak_kw: Some(3.892) });
        assert_eq!(r.entity_states()[0].0, "sensor.fluvius_meter_1sag0000000000_peak_power");
        let odd = json!([
            {"d": "2026-01-01T00:00:00Z", "v": [{"v": "1.5", "sst": "2026-01-02T10:00:00", "set": "2026-01-02T10:15:00Z"}, {"v": 2, "sst": null}]},
            {"d": null, "v": [{"v": 9}]},
            {"d": "2026-02-01T00:00:00Z"}
        ]);
        let p = parse_spikes(&odd).unwrap();
        assert_eq!((p.len(), p[0].kw, p[0].period_end), (1, 1.5, p[0].period_start));
        assert!(matches!(parse_spikes(&json!({})), Err(CloudError::Malformed(_))));
        assert_eq!(reading("X", &[]).peak_kw, None);
    }

    #[test]
    fn history_range_is_local_time() {
        let (from, until) = spike_range(t("2026-11-07T23:30:00Z"), chrono_tz::Europe::Brussels);
        assert_eq!(from, "2026-01-01T00:00:00.000+01:00");
        assert_eq!(until, "2026-11-08T23:59:59.999+01:00");
        let (_, summer) = spike_range(t("2026-07-01T12:00:00Z"), chrono_tz::Europe::Brussels);
        assert_eq!(summer, "2026-07-01T23:59:59.999+02:00");
    }

    #[test]
    fn msal_and_pkce() {
        let m = msal_settings(&json!({"auth": {"clientId": "cid", "authority": "https://login.example/t/p/"},
            "scopes": "openid profile", "authRequest": {"scopes": ["profile", "x"]},
            "protectedResourceMap": {"api": ["y"], "other": "z w", "n": 5}}))
        .unwrap();
        assert_eq!(
            (m.authority.as_str(), m.client_id.as_str(), m.redirect_uri.as_str()),
            ("https://login.example/t/p", "cid", DEFAULT_REDIRECT_URI)
        );
        assert_eq!(m.scopes, format!("openid profile x y z w offline_access {DEFAULT_SCOPE}"));
        let d = msal_settings(&json!({"clientId": "c", "redirectUri": "https://r/"})).unwrap();
        assert_eq!((d.authority.as_str(), d.redirect_uri.as_str()), (DEFAULT_AUTHORITY, "https://r/"));
        assert!(matches!(msal_settings(&json!({})), Err(CloudError::Malformed(_))));
        let (v, c) = pkce();
        assert_eq!(v.len(), 86);
        assert_eq!(c, base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(v.as_bytes())));
    }

    #[test]
    fn entry_selection() {
        let dir = tempfile::tempdir().unwrap();
        let cfg: FluviusConfig = serde_json::from_value(
            json!({"credentials": "/etc/energie/cloud-fluvius.json", "state_file": dir.path().join("f.json")}),
        )
        .unwrap();
        assert_eq!((cfg.poll_interval_s, &cfg.endpoints), (21600, &FluviusEndpoints::default()));
        assert!(matches!(Fluvius::new(cfg.clone()), Err(CloudError::Config(_))));
        let f = Fluvius::with_entries(FluviusConfig { poll_interval_s: 1, ..cfg.clone() }, entries()).unwrap();
        assert_eq!((f.entry.ean.as_str(), f.poll_interval()), ("541400000000000000", MIN_POLL));
        let gas = FluviusConfig { ean: Some("541400000000000001".into()), ..cfg.clone() };
        assert!(matches!(Fluvius::with_entries(gas, entries()), Err(CloudError::Config(m)) if m.contains("gas")));
        let none = FluviusConfig { ean: Some("nope".into()), ..cfg.clone() };
        assert!(matches!(Fluvius::with_entries(none, entries()), Err(CloudError::Config(_))));
        let e: FluviusEntry =
            serde_json::from_value(json!({"email": "e", "password": "p", "ean": "1", "meter_serial": "s"})).unwrap();
        assert_eq!(e.meter_type, "electricity");
        assert!(!format!("{:?}", entries()).contains("hunter2"));
        let path = dir.path().join("cloud-fluvius.json");
        std::fs::write(&path, r#"[{"email":"e","password":"p","ean":"1","meter_serial":"s"}]"#).unwrap();
        assert!(Fluvius::new(FluviusConfig { credentials: path, ..cfg }).is_ok());
    }

    struct Rig {
        mock: Mock,
        script: Script,
        log: Log,
        dir: tempfile::TempDir,
    }

    const PAGE: &str = r#"<html><script>var SETTINGS = {"csrf":"csrf-1","transId":"StateProperties=tx1","hosts":{"tenant":"/klanten.onmicrosoft.com/B2C_1A_customer_signup_signin","policy":"B2C_1A_customer_signup_signin"},"api":"CombinedSigninAndSignup"};
        var SA_FIELDS = {"AttributeFields":[{"ID":"signInName"},{"ID":"password","IS_PASSWORD":true}]};</script></html>"#;

    async fn rig() -> Rig {
        let (mock, script, log) = Mock::scripted().await;
        let msal = json!({"auth": {"clientId": "cid-1", "authority": mock.url(TENANT), "redirectUri": "https://mijn.fluvius.be/"},
            "scopes": [DEFAULT_SCOPE]});
        script.set("GET /api/global/msal/config", Reply::json(200, msal.to_string()));
        script.set(&format!("GET {TENANT}/oauth2/v2.0/authorize"), Reply::html(PAGE));
        script.set(&format!("POST {TENANT}/SelfAsserted"), Reply::json(200, r#"{"status":"200"}"#));
        script.set(&format!("GET {TENANT}/api/CombinedSigninAndSignup/confirmed"), Reply::redirect("/hop"));
        script.set("GET /hop", Reply::redirect("https://mijn.fluvius.be/?code=code-1"));
        script.set(
            &format!("POST {TENANT}/oauth2/v2.0/token"),
            Reply::json(200, r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600}"#),
        );
        script.set(SPIKES_PATH, Reply::json(200, SPIKES));
        Rig { mock, script, log, dir: tempfile::tempdir().unwrap() }
    }

    impl Rig {
        fn config(&self) -> FluviusConfig {
            FluviusConfig {
                credentials: "/nonexistent".into(),
                state_file: self.dir.path().join("fluvius-token.json"),
                ean: None,
                poll_interval_s: 3600,
                endpoints: FluviusEndpoints {
                    msal_config: self.mock.url("/api/global/msal/config"),
                    api_base: self.mock.base.clone(),
                },
            }
        }

        fn fluvius(&self) -> Fluvius {
            Fluvius::with_entries(self.config(), entries()).unwrap()
        }

        fn stored(&self) -> FluviusTokens {
            StateFile::new(self.config().state_file).unwrap().load().unwrap().unwrap()
        }
    }

    /// End to end: B2C login with PKCE, the spikes request with its parameters, the token set
    /// persisted, one read per poll interval.
    #[tokio::test]
    async fn logs_in_and_reads_the_peak() {
        let r = rig().await;
        let mut f = r.fluvius();
        assert_eq!(f.name(), "fluvius");
        let CloudReading::Fluvius(p) = f.read().await.unwrap() else { panic!() };
        assert_eq!(p.peak_kw, Some(3.892));
        f.read().await.unwrap();
        assert_eq!(r.log.count(SPIKES_PATH), 1);
        let (h, _) = r.log.last(SPIKES_PATH);
        assert_eq!(h["authorization"], "Bearer at-1");
        let line = r.log.paths().into_iter().find(|p| p.starts_with(SPIKES_PATH)).unwrap();
        assert!(
            line.contains("asServiceProvider=false") && line.contains("meterSerialNumber=1SAG0000000000"),
            "{line}"
        );
        assert!(line.contains("historyFrom=") && line.contains("T00%3A00%3A00.000%2B01%3A00"), "{line}");
        let (h, form) =
            r.log.last(&format!("POST {TENANT}/SelfAsserted?tx=StateProperties%3Dtx1&p=B2C_1A_customer_signup_signin"));
        assert_eq!(h["x-csrf-token"], "csrf-1");
        assert!(
            form.contains("signInName=user%40example.invalid")
                && form.contains("password=hunter2")
                && form.contains("request_type=RESPONSE")
        );
        let (_, token_form) = r.log.last(&format!("POST {TENANT}/oauth2/v2.0/token"));
        assert!(
            token_form.contains("grant_type=authorization_code")
                && token_form.contains("code=code-1")
                && token_form.contains("code_verifier=")
        );
        let authorize = r.log.paths().into_iter().find(|p| p.contains("/authorize?")).unwrap();
        assert!(
            authorize.contains("code_challenge_method=S256") && authorize.contains("login_hint=user%40example.invalid")
        );
        let stored = r.stored();
        assert_eq!(stored.refresh_token.unwrap().expose(), "rt-1");
        assert!(stored.access_expires_at > Utc::now().timestamp() as f64);
        assert_eq!(f.entities()[0].entity_id, "sensor.fluvius_meter_1sag0000000000_peak_power");
        let CloudReading::Fluvius(u) = f.unavailable() else { panic!() };
        assert_eq!((u.peak_kw, u.meter.as_str()), (None, "1sag0000000000"));
    }

    /// A stored access token is reused; an expired one is refreshed with the (rotating) refresh
    /// token; `invalid_grant` falls back to a full login; other token errors are transient.
    #[tokio::test]
    async fn token_lifecycle() {
        let r = rig().await;
        let st = StateFile::new(r.config().state_file).unwrap();
        st.save(&FluviusTokens {
            refresh_token: Some("rt-0".into()),
            access_token: Some("at-0".into()),
            access_expires_at: 4e9,
        })
        .unwrap();
        let mut f = r.fluvius();
        f.peaks_at(Utc::now()).await.unwrap();
        assert_eq!(r.log.last(SPIKES_PATH).0["authorization"], "Bearer at-0");
        assert_eq!(r.log.count("GET /api/global/msal/config"), 0);

        st.save(&FluviusTokens {
            refresh_token: Some("rt-0".into()),
            access_token: Some("at-0".into()),
            access_expires_at: 0.0,
        })
        .unwrap();
        r.script.once(
            &format!("POST {TENANT}/oauth2/v2.0/token"),
            Reply::json(200, r#"{"access_token":"at-2","refresh_token":"rt-2","expires_in":"3600"}"#),
        );
        let mut f = r.fluvius();
        f.peaks_at(Utc::now()).await.unwrap();
        let (_, form) = r.log.last(&format!("POST {TENANT}/oauth2/v2.0/token"));
        assert!(form.contains("grant_type=refresh_token") && form.contains("refresh_token=rt-0"), "{form}");
        assert_eq!(r.stored().refresh_token.unwrap().expose(), "rt-2");
        assert_eq!(r.log.count(&format!("GET {TENANT}/oauth2/v2.0/authorize")), 0);

        st.save(&FluviusTokens { refresh_token: Some("rt-0".into()), access_token: None, access_expires_at: 0.0 })
            .unwrap();
        r.script.once(&format!("POST {TENANT}/oauth2/v2.0/token"), Reply::json(400, r#"{"error":"invalid_grant"}"#));
        let mut f = r.fluvius();
        assert_eq!(f.access_token().await.unwrap(), "at-1");
        assert_eq!(r.log.count(&format!("GET {TENANT}/oauth2/v2.0/authorize")), 1);
        assert_eq!(f.tokens().refresh_token.as_ref().unwrap().expose(), "rt-1");

        st.save(&FluviusTokens { refresh_token: Some("rt-0".into()), access_token: None, access_expires_at: 0.0 })
            .unwrap();
        r.script.once(&format!("POST {TENANT}/oauth2/v2.0/token"), Reply::json(503, "busy"));
        assert!(matches!(r.fluvius().access_token().await, Err(CloudError::Api { .. })));
        r.script.once(&format!("POST {TENANT}/oauth2/v2.0/token"), Reply::json(200, r#"{"token_type":"Bearer"}"#));
        assert!(matches!(r.fluvius().access_token().await, Err(CloudError::Malformed(_))));
        r.script.once("GET /api/global/msal/config", Reply::json(500, ""));
        assert!(matches!(r.fluvius().access_token().await, Err(CloudError::Status(500))));
    }

    /// A 401 on the data request gets one new token; a second 401 or another status fails.
    #[tokio::test]
    async fn data_request_errors() {
        let r = rig().await;
        let mut f = r.fluvius();
        r.script.once(SPIKES_PATH, Reply::json(401, ""));
        f.peaks_at(Utc::now()).await.unwrap();
        r.script.once(SPIKES_PATH, Reply::json(401, ""));
        r.script.once(SPIKES_PATH, Reply::json(401, ""));
        assert!(matches!(f.peaks_at(Utc::now()).await, Err(CloudError::Unauthorized(_))));
        r.script.set(SPIKES_PATH, Reply::json(500, ""));
        assert!(matches!(f.read().await, Err(CloudError::Status(500))));
        assert!(matches!(f.read().await, Err(CloudError::Backoff(_))));
    }

    /// The ways a B2C login breaks.
    #[tokio::test]
    async fn login_failures() {
        let r = rig().await;
        let msal = Msal {
            authority: r.mock.url(TENANT),
            client_id: "cid-1".into(),
            redirect_uri: String::new(),
            scopes: "openid".into(),
        };
        let f = r.fluvius();
        let v = f.login(&msal).await.unwrap();
        assert_eq!(v["access_token"], "at-1");
        let (_, form) = r.log.last(&format!("POST {TENANT}/oauth2/v2.0/token"));
        assert!(form.contains("redirect_uri=https%3A%2F%2Fmijn.fluvius.be%2F"), "{form}");

        let authorize = format!("GET {TENANT}/oauth2/v2.0/authorize");
        async fn fail_with(r: &Rig, f: &Fluvius, msal: &Msal, reply: Reply, key: &str) -> CloudError {
            r.script.once(key, reply);
            f.login(msal).await.unwrap_err()
        }
        macro_rules! fail {
            ($reply:expr, $key:expr) => {
                fail_with(&r, &f, &msal, $reply, $key)
            };
        }
        assert!(matches!(fail!(Reply::json(503, ""), &authorize).await, CloudError::Status(503)));
        assert!(
            matches!(fail!(Reply::html("<html>"), &authorize).await, CloudError::Malformed(m) if m.contains("SETTINGS"))
        );
        assert!(
            matches!(fail!(Reply::html("var SETTINGS = {};"), &authorize).await, CloudError::Malformed(m) if m.contains("SA_FIELDS"))
        );
        let no_pw = r#"var SETTINGS = {}; var SA_FIELDS = {"AttributeFields":[{"ID":"signInName"}]};"#;
        assert!(
            matches!(fail!(Reply::html(no_pw), &authorize).await, CloudError::Malformed(m) if m.contains("password"))
        );
        let empty = r#"var SETTINGS = {}; var SA_FIELDS = {"AttributeFields":[]};"#;
        assert!(
            matches!(fail!(Reply::html(empty), &authorize).await, CloudError::Malformed(m) if m.contains("login field"))
        );
        let no_csrf =
            r#"var SETTINGS = {}; var SA_FIELDS = {"AttributeFields":[{"ID":"a"},{"ID":"b","IS_PASSWORD":true}]};"#;
        assert!(
            matches!(fail!(Reply::html(no_csrf), &authorize).await, CloudError::Malformed(m) if m.contains("csrf"))
        );
        let self_asserted = format!("POST {TENANT}/SelfAsserted");
        assert!(
            matches!(fail!(Reply::json(200, r#"{"status":"400","message":"Wrong password"}"#), &self_asserted).await, CloudError::Unauthorized(m) if m == "Wrong password")
        );
        assert!(matches!(
            fail!(Reply::json(200, r#"{"status":400}"#), &self_asserted).await,
            CloudError::Unauthorized(_)
        ));
        assert!(matches!(fail!(Reply::json(500, ""), &self_asserted).await, CloudError::Status(500)));
        let confirmed = format!("GET {TENANT}/api/CombinedSigninAndSignup/confirmed");
        assert!(
            matches!(fail!(Reply::html("no redirect"), &confirmed).await, CloudError::Unauthorized(m) if m.contains("HTTP 200"))
        );
        assert!(
            matches!(fail!(Reply::redirect("https://mijn.fluvius.be/?code=c&state=other"), &confirmed).await, CloudError::Unauthorized(m) if m.contains("state"))
        );
        assert!(matches!(fail!(Reply::redirect("http://[::1"), &confirmed).await, CloudError::Malformed(_)));
        assert!(matches!(
            fail!(Reply::redirect("/api/CombinedSigninAndSignup/confirmed-loop"), &confirmed).await,
            CloudError::Unauthorized(_)
        ));
        r.script.set(
            "GET /api/CombinedSigninAndSignup/confirmed-loop",
            Reply::redirect("/api/CombinedSigninAndSignup/confirmed-loop"),
        );
        assert!(
            matches!(fail!(Reply::redirect("/api/CombinedSigninAndSignup/confirmed-loop"), &confirmed).await, CloudError::Unauthorized(m) if m.contains("six"))
        );
        let token = format!("POST {TENANT}/oauth2/v2.0/token");
        assert!(matches!(fail!(Reply::json(400, "nope"), &token).await, CloudError::Api { .. }));

        let abs = PAGE.replace("\"tenant\":\"/klanten", &format!("\"tenant\":\"{}/klanten", r.mock.base));
        r.script.once(&authorize, Reply::html(abs));
        f.login(&msal).await.unwrap();
    }
}
