//! VW Group EU Data Act portal (`cupra_eu_data_act` integration), the ID.Buzz:
//! `sensor.de_buzz_accu`, `sensor.de_buzz_laaddoel`, `sensor.de_buzz_kilometerstand`. Read-only.
//!
//! The portal (`eu-data-act.drivesomethinggreater.com`) delivers the car's data as ZIP
//! snapshots, one about every 15 minutes while the car reports, each holding one JSON file
//! `{"vin", "Data": [{"key", "dataFieldName"?, "value", "timestampUtc"?}]}`. The client lists
//! them (`GET /proxy_api/euda-apim/datadelivery/vehicles/{vin}/{identifier}/list`, header
//! `type: partial`), downloads the newest with content (`…/download`, headers `filename`,
//! `type`), and picks the fields the way the integration does: by field name (from
//! `dataFieldName` or the portal's data dictionary, keyed by UUID), preferring usable values,
//! then the freshest `timestampUtc`, then the last occurrence. Values missing from a new
//! snapshot keep the previous one (the integration's sticky merge).
//!
//! Auth is a browser-style OIDC login at `identity.vwgroup.io` with the account's email and
//! password, as the integration does it since v0.6.45: authorize → either the classic
//! identifier form (email) and authenticate form (password), or the Auth0 Universal Login
//! (`/u/login`, one form, CUPRA/SEAT since 2026-10-26) → a redirect chain followed by hand that
//! stops at the portal's `/services/callbacklogin`, which exchanges the code and sets the
//! `access_token` cookie. That cookie is the login; the callback's own redirect to a CMS page
//! (which answers 406 since 2026-10-08) is never followed. An optional marketing-consent page
//! (classic `/consent/marketing/…?callback=`, or v2 `…/marketing-consent` skipped with a POST)
//! is skipped, and the IdP terms interstitial is accepted once; anything else that needs a
//! browser is a clear [`CloudError::Unauthorized`]. The user agent is `energie/<version>`, and
//! the portal is not primed first (VW's request). A 401/403 (or a redirect away from the
//! portal) logs in again once, through a [`LoginGate`] (one login per 30 min, none after three
//! rejections until the credentials change). The session lives in memory; credentials set with
//! `energie credentials set` are kept in the state dir and win over the file.
//!
//! Polling: every 15 minutes (the snapshot cadence; HA retries every minute when a snapshot is
//! overdue, this client does not), at least 5 minutes, with 5 → 60 minute backoff.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use energie_drivers::AuthStatus;
use reqwest::Url;
use reqwest::cookie::Jar;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{CloudError, Result};
use crate::login::{self, CredentialStore, Fields, LoginGate};
use crate::poll::{Gate, Poller};
use crate::source::{CloudReading, CloudSource, EntityDescriptor, EntityKind, EntityState, num, text};
use crate::store::Secret;
use crate::{html, http, zip};

pub const PORTAL: &str = "https://eu-data-act.drivesomethinggreater.com";
pub const IDENTITY: &str = "https://identity.vwgroup.io";
/// Who we are, as VW asked integrations to say (no browser disguise).
const USER_AGENT: &str = concat!("energie/", env!("CARGO_PKG_VERSION"));
const NO_CONTENT_SUFFIX: &str = "_no_content_found.zip";

/// Floor of the poll interval.
pub const MIN_POLL: Duration = Duration::from_secs(5 * 60);

/// OIDC `state` suffix and client id per brand slug (the integration's `brands.py`).
pub fn brand(slug: &str) -> Option<(&'static str, &'static str)> {
    const VW: &str = "9b58543e-1c15-4193-91d5-8a14145bebb0@apps_vw-dilab_com";
    const SEAT: &str = "f85e5b69-e3b2-43aa-9c0d-1b7d0e0b576f@apps_vw-dilab_com";
    Some(match slug {
        "volkswagen" => ("VOLKSWAGEN_PASSENGER_CARS", VW),
        "volkswagen_commercial" => ("VOLKSWAGEN_COMMERCIAL_VEHICLES", VW),
        "audi" => ("AUDI", "cc29b87a-5e9a-4362-aecf-5adea6b01bbb@apps_vw-dilab_com"),
        "skoda" => ("SKODA", "3ea88bf9-1d4e-4a68-b3ad-4098c1f1d246@apps_vw-dilab_com"),
        "seat" => ("SEAT", SEAT),
        "cupra" => ("CUPRA", SEAT),
        "bentley" => ("BENTLEY", "d38aac0f-3d89-4a63-8538-b75b31322c7b@apps_vw-dilab_com"),
        _ => return None,
    })
}

/// The HA config entry's `data` (and the shape of a captured credentials file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CupraCredentials {
    #[serde(default = "default_brand")]
    pub brand: String,
    pub email: Secret,
    pub password: Secret,
    pub vin: Secret,
    pub identifier: String,
    #[serde(default)]
    pub nickname: Option<String>,
}

fn default_brand() -> String {
    "cupra".into()
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct CupraEndpoints {
    pub portal: String,
    pub identity: String,
}

impl Default for CupraEndpoints {
    fn default() -> Self {
        CupraEndpoints { portal: PORTAL.into(), identity: IDENTITY.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CupraConfig {
    /// HA config-entry `data` as JSON (`/etc/energie/cloud-cupra.json`).
    pub credentials: PathBuf,
    /// Credentials captured at run time (`<state_dir>/credentials/<id>.json`); they win over
    /// `credentials` when present.
    #[serde(default)]
    pub credentials_store: Option<PathBuf>,
    /// Seconds between listings; raised to [`MIN_POLL`] when lower.
    #[serde(default = "default_poll")]
    pub poll_interval_s: u64,
    #[serde(default)]
    pub endpoints: CupraEndpoints,
}

fn default_poll() -> u64 {
    15 * 60
}

/// The Buzz as the logic and the dashboard see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BuzzReading {
    /// `sensor.de_buzz_accu`, % (`battery_state_report.soc`; flat datasets: `state_of_charge`).
    pub soc: Option<f64>,
    /// `sensor.de_buzz_laaddoel`, % (`settings.target_soc`).
    pub target_soc: Option<f64>,
    /// `sensor.de_buzz_kilometerstand`, km (`mileage.value`; flat datasets: `mileage`).
    pub odometer_km: Option<f64>,
    /// `sensor.de_buzz_laatste_voertuigupdate`: the newest `car_captured_time` in the snapshot.
    pub captured_at: Option<DateTime<Utc>>,
}

impl BuzzReading {
    pub fn world(&self) -> energie_core::world::Buzz {
        energie_core::world::Buzz { soc: self.soc, target_soc: self.target_soc, odometer_km: self.odometer_km }
    }

    pub fn entity_states(&self) -> Vec<EntityState> {
        self.entity_states_for("de_buzz")
    }

    fn entity_states_for(&self, slug: &str) -> Vec<EntityState> {
        vec![
            num(&format!("sensor.{slug}_accu"), self.soc),
            num(&format!("sensor.{slug}_laaddoel"), self.target_soc),
            num(&format!("sensor.{slug}_kilometerstand"), self.odometer_km),
            text(&format!("sensor.{slug}_laatste_voertuigupdate"), self.captured_at.map(|t| t.to_rfc3339())),
        ]
    }

    /// Fields this snapshot lacks keep the previous value; the capture time never goes back.
    fn sticky(self, prev: &BuzzReading) -> BuzzReading {
        BuzzReading {
            soc: self.soc.or(prev.soc),
            target_soc: self.target_soc.or(prev.target_soc),
            odometer_km: self.odometer_km.or(prev.odometer_km),
            captured_at: self.captured_at.max(prev.captured_at),
        }
    }
}

/// HA's entity-id slug of a device name (`De Buzz` → `de_buzz`).
pub fn slug(name: &str) -> String {
    let s: String = name.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
    s.split('_').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("_")
}

/// The portal's data dictionary, reduced to the fields this client reads (UUID → field name).
static FIELDS: LazyLock<HashMap<String, String>> =
    LazyLock::new(|| serde_json::from_str(include_str!("../data/euda_fields.json")).expect("bundled dictionary"));

/// Portal "no reading" sentinels (uint16/int32/uint32 max).
fn usable(v: f64) -> bool {
    ![65_535.0, 2_147_483_647.0, 4_294_967_295.0].contains(&v)
}

/// A value field's timestamp: ISO 8601, or epoch milliseconds (12+ digits).
pub fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.len() >= 12 && s.bytes().all(|b| b.is_ascii_digit()) {
        return s.parse::<i64>().ok().and_then(DateTime::from_timestamp_millis);
    }
    DateTime::parse_from_rfc3339(&s.replace('Z', "+00:00"))
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok().map(|t| t.and_utc()))
}

struct Point<'a> {
    field: String,
    value: &'a str,
    at: Option<DateTime<Utc>>,
}

/// Parse one dataset JSON into a reading.
pub fn parse_dataset(payload: &Value) -> Result<BuzzReading> {
    let data = payload["Data"].as_array().ok_or_else(|| CloudError::Malformed("dataset without Data".into()))?;
    let points: Vec<Point> = data
        .iter()
        .filter_map(|item| {
            let key = item["key"].as_str()?;
            let field = item["dataFieldName"].as_str().map(str::to_string).or_else(|| FIELDS.get(key).cloned())?;
            let value = item["value"].as_str().unwrap_or_default();
            Some(Point { field, value, at: item["timestampUtc"].as_str().and_then(parse_timestamp) })
        })
        .collect();
    // `find_by_field`: usable first, then freshest, then last in the file.
    let pick = |name: &str| -> Option<f64> {
        let matches: Vec<(usize, &Point, Option<f64>)> = points
            .iter()
            .enumerate()
            .filter(|(_, p)| p.field == name)
            .map(|(i, p)| (i, p, p.value.trim().parse::<f64>().ok().filter(|v| usable(*v))))
            .collect();
        let good: Vec<_> = matches.iter().filter(|(_, _, v)| v.is_some()).collect();
        good.into_iter().max_by_key(|(i, p, _)| (p.at.is_some(), p.at, *i)).and_then(|(_, _, v)| *v)
    };
    let unit = points.iter().rev().find(|p| p.field == "mileage.unit").map(|p| p.value.trim().to_ascii_uppercase());
    let miles = unit.is_some_and(|u| u == "MILES" || u == "MILE");
    let mileage = pick("mileage.value").or_else(|| pick("mileage"));
    let captured_at = points
        .iter()
        .filter(|p| {
            let leaf = p.field.rsplit('.').next().unwrap_or_default();
            leaf == "car_captured_time" || leaf == "car_captured_utc_timestamp"
        })
        .filter_map(|p| parse_timestamp(p.value))
        .max();
    Ok(BuzzReading {
        soc: pick("battery_state_report.soc").or_else(|| pick("state_of_charge")),
        target_soc: pick("settings.target_soc"),
        odometer_km: mileage.map(|m| if miles { m * 1.609_344 } else { m }),
        captured_at,
    })
}

/// When a listing entry was made: `createdOn`, else the `YYYYMMDDhhmmss` part of its name.
fn created_on(entry: &Value) -> Option<DateTime<Utc>> {
    if let Some(t) = entry["createdOn"].as_str().and_then(parse_timestamp) {
        return Some(t);
    }
    let name = entry["name"].as_str()?;
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.rsplit('_').find_map(|p| NaiveDateTime::parse_from_str(p, "%Y%m%d%H%M%S").ok()).map(|t| t.and_utc())
}

fn same_host(a: &Url, b: &Url) -> bool {
    a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

/// Redirect statuses the login follows by hand, and how many hops at most.
const LOGIN_REDIRECTS: [u16; 3] = [301, 302, 303];
const MAX_LOGIN_HOPS: usize = 10;
/// The portal's OIDC callback: it exchanges the code and sets the `access_token` cookie.
const CALLBACK_PATH: &str = "/services/callbacklogin";

/// A form's fields, in order.
type Form = Vec<(String, String)>;

/// Where a login hop ended: the callback (with whether it set `access_token`), or a page.
struct Landing {
    url: Url,
    status: u16,
    token: bool,
    page: String,
}

/// A `Set-Cookie` of `name` with a value.
fn sets_cookie(headers: &reqwest::header::HeaderMap, name: &str) -> bool {
    headers.get_all(reqwest::header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok()).any(|c| {
        c.split(';')
            .next()
            .and_then(|kv| kv.trim().strip_prefix(name))
            .and_then(|r| r.strip_prefix('='))
            .is_some_and(|v| !v.is_empty())
    })
}

/// Browser-style headers of the login hops (`_login_headers`).
fn login_headers(referer: Option<&Url>) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("User-Agent", USER_AGENT.to_string()),
        (
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8".into(),
        ),
        ("Accept-Language", "de-DE,de;q=0.9,en;q=0.8".into()),
        ("Cache-Control", "max-age=0".into()),
    ];
    if let Some(r) = referer {
        h.push(("Referer", r.to_string()));
        h.push(("Origin", http::origin(r)));
    }
    h
}

fn with_headers(mut req: reqwest::RequestBuilder, headers: Vec<(&'static str, String)>) -> reqwest::RequestBuilder {
    for (k, v) in headers {
        req = req.header(k, v);
    }
    req
}

/// What a person has to do when the login stops at a page only a browser gets past.
const IN_A_BROWSER: &str =
    "log in once at the portal in a browser and accept the terms, then set the credentials again";

/// Cloud client for one vehicle on the EU Data Act portal.
pub struct Cupra {
    endpoints: CupraEndpoints,
    creds: CupraCredentials,
    creds_source: login::Source,
    store: Option<CredentialStore>,
    identifier: String,
    /// Follows redirects (the API calls, the first login hops); shares its jar with `manual`.
    client: reqwest::Client,
    /// Does not follow: the credential hops, which must stop at the portal callback.
    manual: reqwest::Client,
    logged_in: bool,
    sticky: BuzzReading,
    poller: Poller<BuzzReading>,
    gate: LoginGate,
}

impl Cupra {
    /// Load the credentials (the captured ones when present, else the file).
    pub fn new(cfg: CupraConfig) -> Result<Self> {
        let store = cfg.credentials_store.as_ref().map(CredentialStore::new).transpose()?;
        let (creds, source) = login::load_credentials::<CupraCredentials>(store.as_ref(), &cfg.credentials)?;
        let mut c = Cupra::with_credentials(cfg, creds)?;
        c.creds_source = source;
        Ok(c)
    }

    pub fn with_credentials(cfg: CupraConfig, creds: CupraCredentials) -> Result<Self> {
        if brand(&creds.brand).is_none() {
            return Err(CloudError::Config(format!("unknown brand {:?}", creds.brand)));
        }
        http::parse_url(&cfg.endpoints.portal)?;
        http::parse_url(&cfg.endpoints.identity)?;
        let interval = Duration::from_secs(cfg.poll_interval_s).max(MIN_POLL);
        let jar = Arc::new(Jar::default());
        Ok(Cupra {
            identifier: creds.identifier.clone(),
            endpoints: cfg.endpoints,
            creds,
            creds_source: login::Source::Etc,
            store: cfg.credentials_store.map(CredentialStore::new).transpose()?,
            client: http::client(http::TIMEOUT, Some(jar.clone()), true)?,
            manual: http::client(http::TIMEOUT, Some(jar), false)?,
            logged_in: false,
            sticky: BuzzReading::default(),
            poller: Poller::new(interval, Duration::from_secs(300), Duration::from_secs(3600)),
            gate: LoginGate::default(),
        })
    }

    /// Replace the login gate (tests use a shorter gap).
    pub fn with_gate(mut self, gate: LoginGate) -> Self {
        self.gate = gate;
        self
    }

    pub fn poll_interval(&self) -> Duration {
        self.poller.interval()
    }

    /// The sign-in state.
    pub fn auth_status(&self) -> AuthStatus {
        let can = !self.creds.email.is_empty() && !self.creds.password.is_empty();
        self.gate.status(self.creds_source.label(), can, Instant::now())
    }

    fn portal(&self) -> Url {
        http::parse_url(&self.endpoints.portal).expect("checked in the constructor")
    }

    fn identity(&self) -> Url {
        http::parse_url(&self.endpoints.identity).expect("checked in the constructor")
    }

    fn slug(&self) -> String {
        slug(self.creds.nickname.as_deref().unwrap_or("De Buzz"))
    }

    /// The OIDC authorize URL (built directly: the portal's own redirect servlet fails for
    /// non-browser clients).
    fn authorize_url(&self) -> Url {
        let (state_key, client_id) = brand(&self.creds.brand).expect("checked in the constructor");
        let base = self.endpoints.identity.trim_end_matches('/');
        let mut url = http::parse_url(&format!("{base}/oidc/v1/authorize")).expect("checked in the constructor");
        url.query_pairs_mut()
            .append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("scope", "openid cars profile")
            .append_pair("state", &format!("de__de__{state_key}"))
            .append_pair("redirect_uri", &format!("{}/login", self.endpoints.portal.trim_end_matches('/')))
            .append_pair("prompt", "login");
        url
    }

    /// Fields to POST for one login step: the form's inputs plus `hmac`/`relayState` from the
    /// page's `templateModel` and the script's `csrf_token`.
    fn login_fields(page: &str) -> (Vec<(String, String)>, Option<String>) {
        let (action, mut fields) = html::first_form(page);
        let mut set = |k: &str, v: &str, overwrite: bool| match fields.iter_mut().find(|(n, _)| n == k) {
            Some(f) if overwrite => f.1 = v.to_string(),
            Some(_) => {}
            None => fields.push((k.to_string(), v.to_string())),
        };
        if let Some(model) = html::object_after(page, "templateModel") {
            for key in ["hmac", "relayState"] {
                if let Some(v) = model[key].as_str().filter(|v| !v.is_empty()) {
                    set(key, v, true);
                }
            }
        }
        if let Some(csrf) = html::csrf_token(page) {
            set("_csrf", &csrf, false);
        }
        (fields, action)
    }

    /// The page's login error: the classic `templateModel`, or Auth0's `data-error-code`.
    fn login_error(page: &str) -> Option<String> {
        if let Some(model) = html::object_after(page, "templateModel") {
            let e = if model["error"].is_null() { &model["errorCode"] } else { &model["error"] };
            match e {
                Value::Null => {}
                Value::String(s) => return Some(s.clone()),
                Value::Object(o) => {
                    return o.get("text").or(o.get("errorCode")).and_then(Value::as_str).map(str::to_string);
                }
                other => return Some(other.to_string()),
            }
        }
        let code = page.split("data-error-code=").nth(1)?;
        let code: String =
            code.trim_start_matches(['"', '\'']).chars().take_while(|c| !matches!(c, '"' | '\'')).collect();
        match code.trim() {
            "" => None,
            "wrong-email-or-password" | "wrong-email-credentials" => {
                Some("login failed, check email and password".into())
            }
            c => Some(c.replace('-', " ")),
        }
    }

    fn has(fields: &[(String, String)], k: &str) -> bool {
        fields.iter().any(|(n, _)| n == k)
    }

    /// Auth0 Universal Login (`/u/login`, CUPRA/SEAT since 2026-10-26) instead of the classic
    /// signin-service pages.
    fn is_auth0(url: &Url, fields: &[(String, String)]) -> bool {
        url.path().contains("/u/login")
            || (["username", "password", "state"].iter().all(|k| Self::has(fields, k)) && !Self::has(fields, "hmac"))
    }

    fn is_callback(&self, url: &Url) -> bool {
        same_host(url, &self.portal()) && url.path().starts_with(CALLBACK_PATH)
    }

    /// Follow a login response's redirects by hand, stopping at the portal callback (whose
    /// own redirect to a CMS page, which answers 406 to clients like this one, is not
    /// followed).
    async fn follow(&self, mut resp: reqwest::Response) -> Result<Landing> {
        for _ in 0..MAX_LOGIN_HOPS {
            let (url, status) = (resp.url().clone(), resp.status().as_u16());
            if self.is_callback(&url) {
                let token = sets_cookie(resp.headers(), "access_token");
                return Ok(Landing { url, status, token, page: String::new() });
            }
            let next = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string);
            match next.filter(|_| LOGIN_REDIRECTS.contains(&status)) {
                Some(loc) => {
                    let to = url.join(&loc).map_err(|e| CloudError::Malformed(e.to_string()))?;
                    resp = with_headers(self.manual.get(to), login_headers(Some(&url))).send().await?;
                }
                None => return Ok(Landing { url, status, token: false, page: resp.text().await? }),
            }
        }
        Err(CloudError::Api {
            code: "redirects".into(),
            message: format!("login took over {MAX_LOGIN_HOPS} redirects"),
        })
    }

    /// POST a login form without following, then [`Cupra::follow`].
    async fn post_login(&self, url: Url, fields: &[(String, String)], referer: &Url) -> Result<Landing> {
        let resp = with_headers(self.manual.post(url), login_headers(Some(referer))).form(fields).send().await?;
        self.follow(resp).await
    }

    /// The full OIDC login; leaves the portal session cookies in the jar. Success is the
    /// callback setting `access_token`.
    pub async fn login(&mut self) -> Result<()> {
        self.logged_in = false;
        let portal_root = self.portal().join("/").expect("a base url");
        let resp =
            with_headers(self.client.get(self.authorize_url()), login_headers(Some(&portal_root))).send().await?;
        let signin_url = resp.url().clone();
        let page = resp.text().await?;
        let (mut fields, action) = Self::login_fields(&page);
        let landing = if Self::is_auth0(&signin_url, &fields) {
            if !Self::has(&fields, "state") {
                return Err(CloudError::Unauthorized("could not parse the Auth0 sign-in form".into()));
            }
            Self::put(&mut fields, "username", self.creds.email.expose());
            Self::put(&mut fields, "password", self.creds.password.expose());
            if !Self::has(&fields, "action") {
                fields.push(("action".into(), "default".into()));
            }
            // Auth0 wants `?state=` on the URL as well as in the body.
            self.post_login(signin_url.clone(), &fields, &signin_url).await?
        } else {
            self.classic_login(signin_url, fields, action).await?
        };
        self.finish(landing).await?;
        self.logged_in = true;
        Ok(())
    }

    /// The classic two-step VW identity login: the email, then the password.
    async fn classic_login(
        &self,
        signin_url: Url,
        mut fields: Vec<(String, String)>,
        action: Option<String>,
    ) -> Result<Landing> {
        if !Self::has(&fields, "hmac") || !Self::has(&fields, "_csrf") {
            return Err(CloudError::Unauthorized("could not parse the sign-in form".into()));
        }
        Self::put(&mut fields, "email", self.creds.email.expose());
        let target =
            signin_url.join(action.as_deref().unwrap_or("")).map_err(|e| CloudError::Malformed(e.to_string()))?;
        let resp =
            with_headers(self.client.post(target), login_headers(Some(&signin_url))).form(&fields).send().await?;
        let auth_url = resp.url().clone();
        let page = resp.text().await?;
        let (mut fields, action) = Self::login_fields(&page);
        if !Self::has(&fields, "hmac") || !Self::has(&fields, "_csrf") {
            let why = Self::login_error(&page).unwrap_or_else(|| "no password form (check the email address)".into());
            return Err(CloudError::Unauthorized(why));
        }
        Self::put(&mut fields, "email", self.creds.email.expose());
        Self::put(&mut fields, "password", self.creds.password.expose());
        // The browser posts to the clean URL with relayState in the body.
        let target = match action {
            Some(a) => auth_url.join(&a).map_err(|e| CloudError::Malformed(e.to_string()))?,
            None => {
                let mut u = auth_url.clone();
                u.set_query(None);
                u
            }
        };
        self.post_login(target, &fields, &auth_url).await
    }

    /// The classic marketing-consent page's continuation (`callback`), checked to stay on the
    /// portal or the identity provider.
    fn consent_callback(&self, url: &Url) -> Result<Option<Url>> {
        if !url.path().contains("/consent/marketing/") {
            return Ok(None);
        }
        let raw = http::query_param(url, "callback").filter(|c| !c.is_empty()).ok_or_else(|| {
            CloudError::Unauthorized("login interrupted: a marketing consent page without a continuation".into())
        })?;
        let to = url.join(&raw).map_err(|e| CloudError::Malformed(e.to_string()))?;
        let ok = [self.portal(), self.identity()].iter().any(|b| b.scheme() == to.scheme() && same_host(b, &to));
        if !ok {
            return Err(CloudError::Unauthorized(
                "login interrupted: the marketing consent page leads elsewhere".into(),
            ));
        }
        Ok(Some(to))
    }

    /// The v2 marketing-consent page: where to POST `relayState`/`state` to skip it.
    fn consent_v2_skip(&self, url: &Url) -> Result<Option<(Url, Form)>> {
        let path = url.path().trim_end_matches('/');
        if !path.contains("/login/ui/marketing-consent") || path.ends_with("/skip") || path.ends_with("/accept") {
            return Ok(None);
        }
        let id = self.identity();
        if url.scheme() != id.scheme() || !same_host(url, &id) {
            return Err(CloudError::Unauthorized("login interrupted: a marketing consent page on another host".into()));
        }
        let q = |k| http::query_param(url, k).filter(|v| !v.is_empty());
        let (Some(relay), Some(state)) = (q("relayState"), q("state")) else {
            return Err(CloudError::Unauthorized(
                "login interrupted: a marketing consent page without relayState/state".into(),
            ));
        };
        let mut skip = url.clone();
        skip.set_path(&format!("{path}/skip"));
        skip.set_query(None);
        skip.set_fragment(None);
        Ok(Some((skip, vec![("relayState".into(), relay), ("state".into(), state)])))
    }

    /// Judge where the credentials led: the callback with its cookie is a login; the optional
    /// marketing consent is skipped and the terms page accepted once; anything else fails.
    async fn finish(&self, mut l: Landing) -> Result<()> {
        let (mut after_terms, mut after_marketing) = (false, false);
        loop {
            if self.is_callback(&l.url) {
                return if l.token {
                    Ok(())
                } else {
                    Err(CloudError::Unauthorized("the login callback set no session (no access_token cookie)".into()))
                };
            }
            let classic = self.consent_callback(&l.url)?;
            let v2 = if classic.is_none() { self.consent_v2_skip(&l.url)? } else { None };
            if classic.is_some() || v2.is_some() {
                if after_marketing {
                    return Err(CloudError::Unauthorized(
                        "login interrupted: the marketing consent page came back after it was skipped".into(),
                    ));
                }
                after_marketing = true;
                l = match classic {
                    Some(to) => {
                        self.follow(with_headers(self.manual.get(to), login_headers(Some(&l.url))).send().await?)
                            .await?
                    }
                    None => {
                        let (skip, fields) = v2.expect("one of them is set");
                        self.post_login(skip, &fields, &l.url).await?
                    }
                };
                continue;
            }
            if l.status >= 400 {
                let why = Self::login_error(&l.page).unwrap_or_else(|| format!("login rejected (HTTP {})", l.status));
                return Err(CloudError::Unauthorized(why));
            }
            if l.url.as_str().contains("terms-and-conditions") {
                let (fields, action) = Self::login_fields(&l.page);
                if after_terms || ["_csrf", "relayState", "hmac"].iter().any(|k| !Self::has(&fields, k)) {
                    return Err(CloudError::Unauthorized(format!(
                        "the identity provider wants new terms accepted: {IN_A_BROWSER}"
                    )));
                }
                after_terms = true;
                let target = match action {
                    Some(a) => l.url.join(&a).map_err(|e| CloudError::Malformed(e.to_string()))?,
                    None => {
                        let mut u = l.url.clone();
                        u.set_query(None);
                        u
                    }
                };
                let referer = l.url.clone();
                l = self.post_login(target, &fields, &referer).await?;
                continue;
            }
            let s = l.url.as_str();
            if s.contains("signin-service") || s.contains("/u/login") || l.url.path().contains("/error") {
                let why = Self::login_error(&l.page)
                    .unwrap_or_else(|| format!("login failed: check email and password, or {IN_A_BROWSER}"));
                return Err(CloudError::Unauthorized(why));
            }
            return Err(CloudError::Unauthorized(format!(
                "login did not return to the portal (ended at {}{})",
                http::origin(&l.url),
                l.url.path()
            )));
        }
    }

    /// [`Cupra::login`] through the gate.
    async fn login_gated(&mut self) -> Result<()> {
        self.gate.check(Instant::now())?;
        let outcome = self.login().await;
        self.gate.record(Instant::now(), Utc::now(), &outcome);
        outcome
    }

    /// Keep new credentials in the store and log in with them now. `vin`, `identifier`,
    /// `brand` and `nickname` keep their current values unless given.
    pub async fn set_credentials(&mut self, fields: &Map<String, Value>) -> Result<AuthStatus> {
        let f = Fields::new(fields, &["email", "password"], &["vin", "identifier", "brand", "nickname"])?;
        let store =
            self.store.clone().ok_or_else(|| CloudError::Config("no state dir for captured credentials".into()))?;
        let mut creds = self.creds.clone();
        creds.email = f.secret("email").unwrap_or_default();
        creds.password = f.secret("password").unwrap_or_default();
        if let Some(v) = f.secret("vin") {
            creds.vin = v;
        }
        if let Some(i) = f.get("identifier") {
            creds.identifier = i.to_string();
        }
        if let Some(b) = f.get("brand") {
            if brand(b).is_none() {
                return Err(CloudError::Config(format!("unknown brand {b:?}")));
            }
            creds.brand = b.to_string();
        }
        if let Some(n) = f.get("nickname") {
            creds.nickname = Some(n.to_string());
        }
        store.save(&creds)?;
        self.identifier = creds.identifier.clone();
        self.creds = creds;
        self.creds_source = login::Source::State;
        self.gate.reset();
        self.poller.reset();
        // The status reports the outcome.
        let _ = self.login_gated().await;
        Ok(self.auth_status())
    }

    fn put(fields: &mut Vec<(String, String)>, k: &str, v: &str) {
        fields.retain(|(n, _)| n != k);
        fields.push((k.to_string(), v.to_string()));
    }

    /// GET a portal path with extra headers, logging in first and once more if the session
    /// has expired. Returns status and body.
    async fn get(&mut self, path: &str, headers: &[(&str, &str)]) -> Result<(u16, Vec<u8>)> {
        if !self.logged_in {
            self.login_gated().await?;
        }
        let url = format!("{}{path}", self.endpoints.portal.trim_end_matches('/'));
        let mut retried = false;
        loop {
            let mut req = self.client.get(&url).header("User-Agent", USER_AGENT);
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            let resp = req.send().await?;
            let status = resp.status().as_u16();
            let expired = matches!(status, 401 | 403) || !same_host(resp.url(), &self.portal());
            let body = resp.bytes().await?.to_vec();
            if !expired {
                return Ok((status, body));
            }
            if retried {
                return Err(CloudError::Unauthorized(format!("portal session rejected (HTTP {status})")));
            }
            self.login_gated().await?;
            retried = true;
        }
    }

    fn vehicle_path(&self, what: &str) -> String {
        format!("/proxy_api/euda-apim/datadelivery/vehicles/{}/{}/{what}", self.creds.vin.expose(), self.identifier)
    }

    /// The dataset listing: `[{name, createdOn, size}]`.
    pub async fn list(&mut self) -> Result<Vec<Value>> {
        let (status, body) = self.get(&self.vehicle_path("list"), &[("type", "partial")]).await?;
        match status {
            404 => Ok(vec![]),
            200..=299 => {
                let v: Value = serde_json::from_slice(&body)?;
                Ok(match v {
                    Value::Array(a) => a,
                    other => other["files"].as_array().cloned().unwrap_or_default(),
                })
            }
            s => Err(CloudError::Status(s)),
        }
    }

    /// Re-read the data-request identifier (it changes when the portal subscription is
    /// recreated). True when it changed.
    async fn refresh_identifier(&mut self) -> Result<bool> {
        let path = format!("/proxy_api/euda-apim/datarequest/vehicles/{}/metadata/partial", self.creds.vin.expose());
        let (status, body) = self.get(&path, &[]).await?;
        let v: Value = serde_json::from_slice(&body).unwrap_or_default();
        let new = v["Identifier"].as_str().or(v["identifier"].as_str()).filter(|_| status == 200);
        match new {
            Some(id) if id != self.identifier => {
                self.identifier = id.to_string();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Download one dataset and parse it.
    pub async fn download(&mut self, name: &str) -> Result<BuzzReading> {
        let path = self.vehicle_path("download");
        let (status, body) = self.get(&path, &[("filename", name), ("type", "partial")]).await?;
        if status != 200 {
            return Err(CloudError::Status(status));
        }
        parse_dataset(&serde_json::from_slice(&zip::first_json(&body)?)?)
    }

    /// One fetch: list, then the newest dataset with content (falling back to two older ones
    /// on errors), merged into the previous reading.
    pub async fn fetch(&mut self) -> Result<BuzzReading> {
        let mut listing = self.list().await?;
        if listing.is_empty() && self.refresh_identifier().await? {
            listing = self.list().await?;
        }
        let mut content: Vec<(Option<DateTime<Utc>>, String)> = listing
            .iter()
            .filter_map(|e| {
                e["name"].as_str().filter(|n| !n.ends_with(NO_CONTENT_SUFFIX)).map(|n| (created_on(e), n.to_string()))
            })
            .collect();
        content.sort();
        let mut last = CloudError::NotReady(format!("no datasets with content yet ({} listed)", listing.len()));
        for (_, name) in content.iter().rev().take(3) {
            match self.download(name).await {
                Ok(r) => {
                    self.sticky = r.sticky(&self.sticky);
                    return Ok(self.sticky.clone());
                }
                Err(e @ CloudError::Unauthorized(_)) => return Err(e),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
}

#[async_trait]
impl CloudSource for Cupra {
    fn name(&self) -> &'static str {
        "cupra"
    }

    fn entities(&self) -> Vec<EntityDescriptor> {
        let s = self.slug();
        vec![
            EntityDescriptor::new(format!("sensor.{s}_accu"), EntityKind::Sensor, Some("%")).feeds("buzz.soc"),
            EntityDescriptor::new(format!("sensor.{s}_laaddoel"), EntityKind::Sensor, Some("%"))
                .feeds("buzz.target_soc"),
            EntityDescriptor::new(format!("sensor.{s}_kilometerstand"), EntityKind::Sensor, Some("km"))
                .feeds("buzz.odometer_km"),
            EntityDescriptor::new(format!("sensor.{s}_laatste_voertuigupdate"), EntityKind::Sensor, None),
        ]
    }

    async fn read(&mut self) -> Result<CloudReading> {
        match self.poller.gate(Instant::now()) {
            Gate::Cached(r) => return Ok(CloudReading::Buzz(r.clone())),
            Gate::Wait(d) => return Err(CloudError::Backoff(d)),
            Gate::Fetch => {}
        }
        let outcome = self.fetch().await;
        match &outcome {
            Ok(_) => self.gate.session(true),
            Err(e) if login::is_rejection(e) => self.gate.session(false),
            Err(_) => {}
        }
        self.poller.record(Instant::now(), outcome).map(CloudReading::Buzz)
    }

    fn unavailable(&self) -> CloudReading {
        CloudReading::Buzz(BuzzReading::default())
    }

    fn auth(&self) -> Option<AuthStatus> {
        Some(self.auth_status())
    }

    async fn set_credentials(&mut self, fields: &Map<String, Value>) -> Result<AuthStatus> {
        Cupra::set_credentials(self, fields).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::mock::{Log, Mock, Reply, Script};
    use serde_json::json;

    const VIN: &str = "WVPLACEHOLDER0001";
    const LIST: &str = "GET /proxy_api/euda-apim/datadelivery/vehicles/WVPLACEHOLDER0001/ident-1/list";
    const DOWNLOAD: &str = "GET /proxy_api/euda-apim/datadelivery/vehicles/WVPLACEHOLDER0001/ident-1/download";

    /// A snapshot shaped like the portal's (values as HA showed on 2026-11-07): duplicated
    /// fields with conflicting values, sentinels, a dictionary-only key.
    fn dataset() -> Value {
        json!({"vin": VIN, "user_id": "u", "Data": [
            {"key": "506cb83e-f99f-3af3-bbeb-0429b69a78d9", "dataFieldName": "battery_state_report.soc", "value": "41", "timestampUtc": "2026-10-08T08:40:00Z"},
            {"key": "93b55324-6628-36df-8f76-8eba797fc59c", "dataFieldName": "battery_state_report.soc", "value": "43", "timestampUtc": "2026-10-08T08:52:25Z"},
            {"key": "7bddd5e7-43a4-3878-bd63-9502782f77a5", "dataFieldName": "battery_state_report.soc", "value": "65535", "timestampUtc": "2026-10-08T09:00:00Z"},
            {"key": "76acaa98-37ef-3466-b013-21c77ed343ae", "value": "100"},
            {"key": "30cc36fd-71ca-3c09-9296-e94ebd47bd2b", "dataFieldName": "mileage.value", "value": "10019"},
            {"key": "37fcab93-1d16-329f-b13c-844d244faa04", "dataFieldName": "mileage.unit", "value": "KILOMETRES"},
            {"key": "1acad4b1-8026-357f-acf7-a5c18fe23f3f", "dataFieldName": "car_captured_time", "value": "2026-10-08T08:52:25Z"},
            {"key": "2e6ddd5d-0475-384a-a4b4-8fdf7e96d7f0", "dataFieldName": "profile_state_report.car_captured_time", "value": "1791449000000"},
            {"key": "unknown-key", "value": "1"},
            {"value": "no key"}
        ]})
    }

    fn zipped(v: &Value) -> Vec<u8> {
        zip::build(&[("data.json", v.to_string().as_bytes(), true)])
    }

    #[test]
    fn parses_a_dataset_like_the_integration() {
        let r = parse_dataset(&dataset()).unwrap();
        assert_eq!(r.soc, Some(43.0), "freshest usable reading wins, sentinel skipped");
        assert_eq!(r.target_soc, Some(100.0), "field name from the data dictionary");
        assert_eq!(r.odometer_km, Some(10019.0));
        assert_eq!(r.captured_at.unwrap().to_rfc3339(), "2026-10-08T08:52:25+00:00");
        assert_eq!(
            r.world(),
            energie_core::world::Buzz { soc: Some(43.0), target_soc: Some(100.0), odometer_km: Some(10019.0) }
        );
        assert_eq!(r.entity_states()[0], ("sensor.de_buzz_accu".into(), Some(crate::EntityValue::Number(43.0))));
        assert!(matches!(parse_dataset(&json!({})), Err(CloudError::Malformed(_))));
    }

    /// Flat (pre-ID) field names, miles, ties broken by file order.
    #[test]
    fn flat_datasets_and_miles() {
        let v = json!({"Data": [
            {"key": "a", "dataFieldName": "state_of_charge", "value": "60"},
            {"key": "b", "dataFieldName": "state_of_charge", "value": "61"},
            {"key": "c", "dataFieldName": "mileage", "value": "100"},
            {"key": "d", "dataFieldName": "mileage.unit", "value": "miles"},
            {"key": "e", "dataFieldName": "settings.target_soc", "value": "n/a"}
        ]});
        let r = parse_dataset(&v).unwrap();
        assert_eq!(r.soc, Some(61.0));
        assert!((r.odometer_km.unwrap() - 160.9344).abs() < 1e-9);
        assert_eq!((r.target_soc, r.captured_at), (None, None));
    }

    #[test]
    fn timestamps_names_and_sticky_values() {
        assert_eq!(parse_timestamp("1791449000000").unwrap().to_rfc3339(), "2026-10-08T08:43:20+00:00");
        assert_eq!(parse_timestamp("2026-10-08T08:52:25.5").unwrap().timestamp(), 1_791_449_545);
        assert_eq!(parse_timestamp("1791449000"), None);
        assert_eq!(slug("De Buzz"), "de_buzz");
        assert_eq!(slug(" ID. Buzz!"), "id_buzz");
        let a = BuzzReading {
            soc: Some(1.0),
            target_soc: Some(80.0),
            odometer_km: None,
            captured_at: parse_timestamp("2026-10-08T10:00:00Z"),
        };
        let b = BuzzReading {
            soc: None,
            target_soc: Some(90.0),
            odometer_km: Some(5.0),
            captured_at: parse_timestamp("2026-10-08T09:00:00Z"),
        };
        let m = b.sticky(&a);
        assert_eq!((m.soc, m.target_soc, m.odometer_km), (Some(1.0), Some(90.0), Some(5.0)));
        assert_eq!(m.captured_at, a.captured_at);
        assert_eq!(created_on(&json!({"createdOn": "2026-10-08T10:03:57Z"})).unwrap().timestamp(), 1_791_453_837);
        assert_eq!(
            created_on(&json!({"name": "20261008100357_WVPLACEHOLDER0001.zip"})).unwrap().timestamp(),
            1_791_453_837
        );
        assert_eq!(
            created_on(&json!({"name": "WVPLACEHOLDER0001_20261008100357.zip"})).unwrap().timestamp(),
            1_791_453_837
        );
        assert_eq!(created_on(&json!({"name": "x"})), None);
        assert_eq!(created_on(&json!({})), None);
    }

    #[test]
    fn brands_and_config() {
        assert_eq!(brand("volkswagen_commercial").unwrap().0, "VOLKSWAGEN_COMMERCIAL_VEHICLES");
        for b in ["volkswagen", "audi", "skoda", "seat", "cupra", "bentley"] {
            assert!(brand(b).unwrap().1.ends_with("@apps_vw-dilab_com"));
        }
        assert_eq!(brand("trabant"), None);
        let cfg: CupraConfig = serde_json::from_value(json!({"credentials": "/etc/energie/cloud-cupra.json"})).unwrap();
        assert_eq!((cfg.poll_interval_s, &cfg.endpoints), (900, &CupraEndpoints::default()));
        assert!(matches!(Cupra::new(cfg.clone()), Err(CloudError::Config(_))));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cloud-cupra.json");
        let entry = json!({"brand": "volkswagen_commercial", "email": "e", "password": "p", "vin": VIN, "identifier": "i", "nickname": "De Buzz"});
        std::fs::write(&path, entry.to_string()).unwrap();
        assert!(Cupra::new(CupraConfig { credentials: path, ..cfg.clone() }).is_ok());
        let mut c = creds();
        c.brand = "trabant".into();
        assert!(matches!(Cupra::with_credentials(cfg.clone(), c), Err(CloudError::Config(_))));
        let bad = CupraConfig {
            endpoints: CupraEndpoints { portal: "nope".into(), ..CupraEndpoints::default() },
            ..cfg.clone()
        };
        assert!(matches!(Cupra::with_credentials(bad, creds()), Err(CloudError::Config(_))));
        let c: CupraCredentials =
            serde_json::from_value(json!({"email": "e", "password": "p", "vin": VIN, "identifier": "i"})).unwrap();
        assert_eq!(c.brand, "cupra");
        let k = Cupra::with_credentials(CupraConfig { poll_interval_s: 1, ..cfg }, c).unwrap();
        assert_eq!(k.poll_interval(), MIN_POLL);
        let url = k.authorize_url();
        assert_eq!(url.path(), "/oidc/v1/authorize");
        assert_eq!(http::query_param(&url, "state").as_deref(), Some("de__de__CUPRA"));
        assert_eq!(http::query_param(&url, "scope").as_deref(), Some("openid cars profile"));
        assert_eq!(
            http::query_param(&url, "redirect_uri").as_deref(),
            Some("https://eu-data-act.drivesomethinggreater.com/login")
        );
        assert!(url.as_str().contains("scope=openid+cars+profile"));
        assert!(!format!("{:?}", creds()).contains("hunter2"));
    }

    #[test]
    fn login_page_scraping() {
        let page = r#"<form action="/a"><input name="_csrf" value="form-csrf"><input name="hmac" value="old"></form>
            <script>window._IDK = { templateModel: {"hmac":"new","relayState":"rs","error":null,"errorCode":"login.error.x"}, csrf_token: 'js-csrf' };</script>"#;
        let (fields, action) = Cupra::login_fields(page);
        assert_eq!(action.as_deref(), Some("/a"));
        assert!(
            fields.contains(&("hmac".into(), "new".into())) && fields.contains(&("relayState".into(), "rs".into()))
        );
        assert!(fields.contains(&("_csrf".into(), "form-csrf".into())));
        assert_eq!(Cupra::login_error(page).as_deref(), Some("login.error.x"));
        assert_eq!(
            Cupra::login_error(r#"templateModel: {"error": {"text": "Wrong password"}}"#).as_deref(),
            Some("Wrong password")
        );
        assert_eq!(Cupra::login_error(r#"templateModel: {"error": 5}"#).as_deref(), Some("5"));
        assert_eq!(Cupra::login_error(r#"templateModel: {}"#), None);
        assert_eq!(Cupra::login_error("none"), None);
    }

    fn creds() -> CupraCredentials {
        CupraCredentials {
            brand: "volkswagen_commercial".into(),
            email: "user@example.invalid".into(),
            password: "hunter2".into(),
            vin: VIN.into(),
            identifier: "ident-1".into(),
            nickname: Some("De Buzz".into()),
        }
    }

    struct Rig {
        portal: Mock,
        pscript: Script,
        plog: Log,
        identity: Mock,
        iscript: Script,
        ilog: Log,
        dir: tempfile::TempDir,
    }

    const SIGNIN: &str = r#"<html><form id="emailPasswordForm" action="/signin-service/v1/cid/login/identifier" method="post">
        <input type="hidden" name="_csrf" value="csrf-1"><input type="hidden" name="relayState" value="rs-1">
        <input type="hidden" name="hmac" value="hmac-1"><input type="email" name="email"></form></html>"#;
    const PASSWORD: &str = r#"<html><script>window._IDK = { templateModel: {"hmac":"hmac-2","relayState":"rs-2","emailPasswordForm":{"email":"x"},"postAction":"login/authenticate"}, csrf_token: 'csrf-2' };</script></html>"#;

    async fn rig() -> Rig {
        let (portal, pscript, plog) = Mock::scripted().await;
        let (identity, iscript, ilog) = Mock::scripted().await;
        pscript.set("GET /de/en/user.html", Reply { status: 406, ..Reply::html("not acceptable") });
        pscript.set(
            "GET /services/callbacklogin",
            Reply::redirect(portal.url("/de/en/user.html"))
                .header("set-cookie", "session=s1; Path=/")
                .header("set-cookie", "access_token=tok-1; Path=/; HttpOnly"),
        );
        iscript
            .set("GET /oidc/v1/authorize", Reply::redirect("/signin-service/v1/cid/login/identifier?relayState=rs-1"));
        iscript.set("GET /signin-service/v1/cid/login/identifier", Reply::html(SIGNIN));
        iscript.set(
            "POST /signin-service/v1/cid/login/identifier",
            Reply::redirect("/signin-service/v1/cid/login/authenticate?relayState=rs-2"),
        );
        iscript.set("GET /signin-service/v1/cid/login/authenticate", Reply::html(PASSWORD));
        iscript.set(
            "POST /signin-service/v1/cid/login/authenticate",
            Reply::redirect(portal.url("/services/callbacklogin?code=c1")),
        );
        pscript.set(LIST, Reply::json(200, json!([
            {"name": "20261008100357_WVPLACEHOLDER0001.zip", "createdOn": "2026-10-08T10:03:57Z", "size": 5736},
            {"name": "20261008094342_WVPLACEHOLDER0001.zip", "size": 5731},
            {"name": "20261008110000_WVPLACEHOLDER0001_no_content_found.zip", "createdOn": "2026-10-08T11:00:00Z"}
        ]).to_string()));
        pscript.set(DOWNLOAD, Reply::bytes(zipped(&dataset())));
        Rig { portal, pscript, plog, identity, iscript, ilog, dir: tempfile::tempdir().unwrap() }
    }

    impl Rig {
        fn cupra(&self) -> Cupra {
            let cfg = CupraConfig {
                credentials: "/nonexistent".into(),
                credentials_store: Some(self.dir.path().join("credentials/buzz.json")),
                poll_interval_s: 900,
                endpoints: CupraEndpoints { portal: self.portal.base.clone(), identity: self.identity.base.clone() },
            };
            Cupra::with_credentials(cfg, creds()).unwrap().with_gate(LoginGate::new(Duration::ZERO))
        }
    }

    /// End to end: OIDC login (email step, password step, redirect chain), listing, download of
    /// the newest dataset with content, cookies sent, one fetch per poll interval.
    #[tokio::test]
    async fn logs_in_lists_and_downloads() {
        let r = rig().await;
        let mut c = r.cupra();
        assert_eq!(c.name(), "cupra");
        let CloudReading::Buzz(b) = c.read().await.unwrap() else { panic!() };
        assert_eq!((b.soc, b.target_soc, b.odometer_km), (Some(43.0), Some(100.0), Some(10019.0)));
        let CloudReading::Buzz(again) = c.read().await.unwrap() else { panic!() };
        assert_eq!(again, b);
        assert_eq!(r.plog.count(LIST), 1);
        let (h, _) = r.plog.last(DOWNLOAD);
        assert_eq!(h["filename"], "20261008100357_WVPLACEHOLDER0001.zip");
        assert_eq!(h["type"], "partial");
        let cookie = h["cookie"].to_str().unwrap();
        assert!(cookie.contains("session=s1") && cookie.contains("access_token=tok-1"), "{cookie}");
        assert_eq!(r.plog.count("GET /de/en/user.html"), 0, "the CMS page after the callback is never fetched");
        assert_eq!(r.plog.count("GET / "), 0, "no priming request");
        let (h, _) = r.plog.last("GET /services/callbacklogin");
        assert_eq!(h["user-agent"], concat!("energie/", env!("CARGO_PKG_VERSION")));
        assert!(h["accept"].to_str().unwrap().starts_with("text/html"));
        assert_eq!(h["accept-language"], "de-DE,de;q=0.9,en;q=0.8");
        let (_, email_form) = r.ilog.last("POST /signin-service/v1/cid/login/identifier");
        assert!(email_form.contains("hmac=hmac-1") && email_form.contains("_csrf=csrf-1"), "{email_form}");
        assert!(email_form.contains("email=user%40example.invalid"));
        let (h, pw_form) = r.ilog.last("POST /signin-service/v1/cid/login/authenticate");
        assert!(
            pw_form.contains("hmac=hmac-2") && pw_form.contains("_csrf=csrf-2") && pw_form.contains("password=hunter2")
        );
        assert!(h["referer"].to_str().unwrap().contains("authenticate?relayState=rs-2"));
        let (_, _) = r.ilog.last("GET /oidc/v1/authorize?client_id=9b58543e");
        let ents = c.entities();
        assert_eq!(ents[0].entity_id, "sensor.de_buzz_accu");
        assert_eq!(ents.iter().filter(|e| e.world_field.is_some()).count(), 3);
        assert_eq!(c.unavailable(), CloudReading::Buzz(BuzzReading::default()));
    }

    /// An expired session (401, or a redirect to the identity provider) logs in again once.
    #[tokio::test]
    async fn re_logs_in_on_expired_session() {
        let r = rig().await;
        let mut c = r.cupra();
        c.fetch().await.unwrap();
        r.pscript.once(LIST, Reply::json(401, "{}"));
        c.fetch().await.unwrap();
        assert_eq!(r.ilog.count("GET /oidc/v1/authorize"), 2);
        r.pscript.once(LIST, Reply::redirect(r.identity.url("/signin-service/v1/cid/login/identifier")));
        c.fetch().await.unwrap();
        assert_eq!(r.ilog.count("GET /oidc/v1/authorize"), 3);
        r.pscript.set(LIST, Reply::json(403, "{}"));
        assert!(matches!(c.fetch().await, Err(CloudError::Unauthorized(m)) if m.contains("403")));
    }

    /// Wrong passwords, unparseable forms and logins that end elsewhere are `Unauthorized`.
    #[tokio::test]
    async fn login_failures() {
        let r = rig().await;
        r.iscript.once(
            "POST /signin-service/v1/cid/login/authenticate",
            Reply::redirect("/signin-service/v1/cid/login/authenticate?error=login.errors.password_invalid"),
        );
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m.contains("check email")));
        r.iscript.once(
            "POST /signin-service/v1/cid/login/authenticate",
            Reply { status: 400, ..Reply::html(r#"templateModel: {"error": {"text": "Bad"}}"#) },
        );
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m == "Bad"));
        r.iscript.once("POST /signin-service/v1/cid/login/authenticate", Reply { status: 500, ..Reply::html("") });
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m.contains("HTTP 500")));
        r.iscript.once("POST /signin-service/v1/cid/login/authenticate", Reply::redirect("/elsewhere"));
        r.iscript.set("GET /elsewhere", Reply::html("somewhere else"));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m.contains("did not return")));
        r.iscript.once(
            "GET /signin-service/v1/cid/login/authenticate",
            Reply::html(r#"templateModel: {"error": "unknown email"}"#),
        );
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m == "unknown email"));
        r.iscript.once("GET /signin-service/v1/cid/login/authenticate", Reply::html("nothing"));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m.contains("password form")));
        r.iscript.once("GET /signin-service/v1/cid/login/identifier", Reply::html("<html>maintenance</html>"));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m.contains("sign-in form")));
        r.iscript.once("GET /signin-service/v1/cid/login/authenticate", Reply::html(r#"<form action="login/authenticate"></form><script>templateModel: {"hmac":"h"}, csrf_token: "c"</script>"#));
        r.iscript.once(
            "POST /signin-service/v1/cid/login/login/authenticate",
            Reply::redirect(r.portal.url("/services/callbacklogin?code=c2")),
        );
        r.cupra().login().await.unwrap();
        assert_eq!(r.ilog.count("POST /signin-service/v1/cid/login/login/authenticate"), 1);
    }

    /// Empty listings re-read the identifier; no-content snapshots, download failures and
    /// broken archives fall back to older datasets, then fail.
    #[tokio::test]
    async fn listings_identifiers_and_fallbacks() {
        let r = rig().await;
        let meta = "GET /proxy_api/euda-apim/datarequest/vehicles/WVPLACEHOLDER0001/metadata/partial";
        r.pscript.once(LIST, Reply::json(404, ""));
        r.pscript.set(meta, Reply::json(200, r#"{"Identifier":"ident-2"}"#));
        let list2 = "GET /proxy_api/euda-apim/datadelivery/vehicles/WVPLACEHOLDER0001/ident-2/list";
        r.pscript.set(list2, Reply::json(200, r#"{"files":[{"name":"20261008100357_X.zip"}]}"#));
        r.pscript.set(
            "GET /proxy_api/euda-apim/datadelivery/vehicles/WVPLACEHOLDER0001/ident-2/download",
            Reply::bytes(zipped(&dataset())),
        );
        let mut c = r.cupra();
        assert_eq!(c.fetch().await.unwrap().soc, Some(43.0));
        assert_eq!(c.identifier, "ident-2");
        r.pscript.set(list2, Reply::json(200, "[]"));
        assert!(matches!(c.fetch().await, Err(CloudError::NotReady(_))));
        r.pscript.set(meta, Reply::json(500, "oops"));
        assert!(matches!(c.fetch().await, Err(CloudError::NotReady(_))));
        r.pscript.set(list2, Reply::json(502, ""));
        assert!(matches!(c.fetch().await, Err(CloudError::Status(502))));
        r.pscript.set(list2, Reply::json(200, "not json"));
        assert!(matches!(c.fetch().await, Err(CloudError::Malformed(_))));

        let mut c = r.cupra();
        r.pscript.once(DOWNLOAD, Reply::json(500, ""));
        assert_eq!(c.fetch().await.unwrap().soc, Some(43.0), "falls back to the older dataset");
        assert_eq!(r.plog.last(DOWNLOAD).0["filename"], "20261008094342_WVPLACEHOLDER0001.zip");
        r.pscript.set(DOWNLOAD, Reply::bytes(b"garbage".to_vec()));
        assert!(matches!(c.fetch().await, Err(CloudError::Malformed(_))));
        r.pscript.set(DOWNLOAD, Reply::json(403, ""));
        assert!(matches!(c.fetch().await, Err(CloudError::Unauthorized(_))));
        r.pscript.set(
            DOWNLOAD,
            Reply::bytes(zipped(
                &json!({"Data": [{"key": "k", "dataFieldName": "settings.target_soc", "value": "80"}]}),
            )),
        );
        let b = c.fetch().await.unwrap();
        assert_eq!((b.soc, b.target_soc), (Some(43.0), Some(80.0)), "sticky: soc kept from the previous snapshot");
    }

    /// Failures put `read` in backoff.
    #[tokio::test]
    async fn read_backs_off() {
        let r = rig().await;
        r.pscript.set(LIST, Reply::json(503, ""));
        let mut c = r.cupra();
        assert!(matches!(c.read().await, Err(CloudError::Status(503))));
        assert!(matches!(c.read().await, Err(CloudError::Backoff(_))));
    }

    const AUTHENTICATE: &str = "POST /signin-service/v1/cid/login/authenticate";

    /// Auth0 Universal Login: one form at `/u/login?state=…`, POSTed back to the same URL with
    /// username, password, state and `action=default`, then the callback.
    #[tokio::test]
    async fn auth0_universal_login() {
        let r = rig().await;
        r.iscript.set("GET /oidc/v1/authorize", Reply::redirect("/u/login?state=st-1"));
        let form = r#"<form method="post"><input type="hidden" name="state" value="st-1"><input name="username"><input name="password" type="password"></form>"#;
        r.iscript.set("GET /u/login", Reply::html(form));
        r.iscript.set("POST /u/login", Reply::redirect(r.portal.url("/services/callbacklogin?code=c9")));
        r.cupra().login().await.unwrap();
        let paths = r.ilog.paths();
        assert!(paths.contains(&"POST /u/login?state=st-1".to_string()), "{paths:?}");
        let (h, posted) = r.ilog.last("POST /u/login");
        for f in ["state=st-1", "username=user%40example.invalid", "password=hunter2", "action=default"] {
            assert!(posted.contains(f), "{posted}");
        }
        assert!(h["referer"].to_str().unwrap().contains("/u/login?state=st-1"));
        assert_eq!(h["origin"], r.identity.base.as_str());
        // A rejected Auth0 login names Auth0's error code.
        // The sign-in page first, then the page the rejected POST leads back to.
        r.iscript.once("POST /u/login", Reply::redirect("/u/login?state=st-1&error=1"));
        r.iscript.once("GET /u/login", Reply::html(form));
        r.iscript.once("GET /u/login", Reply::html(r#"<div data-error-code="wrong-email-or-password"></div>"#));
        assert!(
            matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m == "login failed, check email and password")
        );
        r.iscript.once("POST /u/login", Reply::redirect("/u/login?state=st-1&error=2"));
        r.iscript.once("GET /u/login", Reply::html(form));
        r.iscript.once("GET /u/login", Reply::html(r#"<div data-error-code='too-many-attempts'></div>"#));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m == "too many attempts"));
        // A login form without its state cannot be posted.
        r.iscript.once("GET /u/login", Reply::html(r#"<form><input name="username"><input name="password"></form>"#));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m.contains("Auth0")));
        assert!(Cupra::is_auth0(
            &Url::parse("https://x/elsewhere").unwrap(),
            &[("username".into(), String::new()), ("password".into(), String::new()), ("state".into(), "s".into())]
        ));
        assert_eq!(Cupra::login_error(r#"data-error-code="""#), None);
    }

    /// The optional marketing consent is skipped (classic: GET its callback; v2: POST
    /// relayState/state to `…/skip`); coming back to it fails, as do continuations elsewhere.
    #[tokio::test]
    async fn marketing_consent_is_skipped() {
        let r = rig().await;
        let callback = r.portal.url("/services/callbacklogin?code=c3");
        let consent = format!("/consent/marketing/v1?callback={}", url_encode(&callback));
        r.iscript.once(AUTHENTICATE, Reply::redirect(consent.clone()));
        r.cupra().login().await.unwrap();
        assert_eq!(r.plog.count("GET /services/callbacklogin?code=c3"), 1);

        r.iscript.once(AUTHENTICATE, Reply::redirect("/v2/login/ui/marketing-consent?relayState=rs-9&state=st-9"));
        r.iscript.set(
            "POST /v2/login/ui/marketing-consent/skip",
            Reply::redirect(r.portal.url("/services/callbacklogin?code=c4")),
        );
        r.cupra().login().await.unwrap();
        let (_, form) = r.ilog.last("POST /v2/login/ui/marketing-consent/skip");
        assert!(form.contains("relayState=rs-9") && form.contains("state=st-9"), "{form}");

        let fail = async |loc: String| {
            r.iscript.once(AUTHENTICATE, Reply::redirect(loc));
            let e = r.cupra().login().await.unwrap_err();
            assert!(login::is_rejection(&e), "{e}");
            e.to_string()
        };
        r.iscript.once(
            "POST /v2/login/ui/marketing-consent/skip",
            Reply::redirect("/v2/login/ui/marketing-consent?relayState=a&state=b"),
        );
        assert!(fail("/v2/login/ui/marketing-consent?relayState=a&state=b".into()).await.contains("came back"));
        assert!(fail("/v2/login/ui/marketing-consent?relayState=a".into()).await.contains("relayState/state"));
        assert!(fail("/consent/marketing/v1".into()).await.contains("without a continuation"));
        assert!(
            fail("/consent/marketing/v1?callback=https%3A%2F%2Fevil.invalid%2Fx".into()).await.contains("elsewhere")
        );
        r.pscript.set("GET /v2/login/ui/marketing-consent", Reply::html("x"));
        let elsewhere = r.portal.url("/v2/login/ui/marketing-consent?relayState=a&state=b");
        assert!(fail(elsewhere).await.contains("another host"));
        // `…/skip` and `…/accept` are not consent pages themselves.
        r.iscript.set("GET /v2/login/ui/marketing-consent/accept", Reply::html("x"));
        assert!(fail("/v2/login/ui/marketing-consent/accept".into()).await.contains("did not return"));
    }

    fn url_encode(s: &str) -> String {
        let mut u = Url::parse("http://x/").unwrap();
        u.query_pairs_mut().append_pair("v", s);
        u.query().unwrap()[2..].to_string()
    }

    /// The IdP terms page is accepted once (its form: _csrf, relayState, hmac); a second one,
    /// or one without those fields, needs a browser. A callback without the session cookie and
    /// an endless redirect chain fail.
    #[tokio::test]
    async fn terms_callback_cookie_and_redirect_limit() {
        let r = rig().await;
        let terms = r#"<form action="/signin-service/v1/terms-and-conditions/accept"><input name="_csrf" value="c"><input name="relayState" value="r"><input name="hmac" value="h"></form>"#;
        r.iscript.once(AUTHENTICATE, Reply::redirect("/signin-service/v1/terms-and-conditions?x=1"));
        r.iscript.set("GET /signin-service/v1/terms-and-conditions", Reply::html(terms));
        r.iscript.set(
            "POST /signin-service/v1/terms-and-conditions/accept",
            Reply::redirect(r.portal.url("/services/callbacklogin?code=c5")),
        );
        r.cupra().login().await.unwrap();
        let (_, form) = r.ilog.last("POST /signin-service/v1/terms-and-conditions/accept");
        assert!(form.contains("_csrf=c") && form.contains("hmac=h"), "{form}");

        r.iscript.once(AUTHENTICATE, Reply::redirect("/signin-service/v1/terms-and-conditions?x=2"));
        r.iscript.once(
            "POST /signin-service/v1/terms-and-conditions/accept",
            Reply::redirect("/signin-service/v1/terms-and-conditions?x=3"),
        );
        let e = r.cupra().login().await.unwrap_err();
        assert!(matches!(&e, CloudError::Unauthorized(m) if m.contains("in a browser")), "{e}");
        r.iscript.once(AUTHENTICATE, Reply::redirect("/signin-service/v1/terms-and-conditions?x=4"));
        r.iscript.once("GET /signin-service/v1/terms-and-conditions", Reply::html("<form></form>"));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m.contains("accept the terms")));
        // A terms form without an action posts to the page itself.
        r.iscript.once(AUTHENTICATE, Reply::redirect("/signin-service/v1/terms-and-conditions?x=5"));
        r.iscript.once(
            "GET /signin-service/v1/terms-and-conditions",
            Reply::html(terms.replace(r#" action="/signin-service/v1/terms-and-conditions/accept""#, "")),
        );
        r.iscript.once(
            "POST /signin-service/v1/terms-and-conditions",
            Reply::redirect(r.portal.url("/services/callbacklogin?code=c6")),
        );
        r.cupra().login().await.unwrap();

        r.pscript.once("GET /services/callbacklogin", Reply::redirect(r.portal.url("/de/en/user.html")));
        let e = r.cupra().login().await.unwrap_err();
        assert!(matches!(&e, CloudError::Unauthorized(m) if m.contains("no access_token")), "{e}");
        r.iscript.once(AUTHENTICATE, Reply::redirect("/loop"));
        r.iscript.set("GET /loop", Reply::redirect("/loop"));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Api { code, .. }) if code == "redirects"));
        r.iscript.once(AUTHENTICATE, Reply::redirect("http://[bad"));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Malformed(_))));
        r.iscript.once(AUTHENTICATE, Reply::redirect("/signin-service/v1/error"));
        r.iscript.set("GET /signin-service/v1/error", Reply::html("oops"));
        assert!(matches!(r.cupra().login().await, Err(CloudError::Unauthorized(m)) if m.contains("in a browser")));
    }

    /// Captured credentials: stored 0600 (vin and identifier kept unless given), preferred over
    /// the file, the gate reset and a login at once; three rejected logins block the gate.
    #[cfg(unix)]
    #[tokio::test]
    async fn credentials_and_the_login_gate() {
        use std::os::unix::fs::PermissionsExt;
        let r = rig().await;
        let file = r.dir.path().join("cloud-cupra.json");
        std::fs::write(&file, json!({"brand": "volkswagen_commercial", "email": "e", "password": "p", "vin": VIN, "identifier": "ident-1"}).to_string()).unwrap();
        let store = r.dir.path().join("credentials/buzz.json");
        let cfg = CupraConfig {
            credentials: file,
            credentials_store: Some(store.clone()),
            poll_interval_s: 900,
            endpoints: CupraEndpoints { portal: r.portal.base.clone(), identity: r.identity.base.clone() },
        };
        r.iscript.set(AUTHENTICATE, Reply::redirect("/signin-service/v1/cid/login/authenticate?error=x"));
        let mut c = Cupra::new(cfg.clone()).unwrap();
        assert_eq!(c.auth().unwrap().credentials, "etc");
        assert!(matches!(c.read().await, Err(CloudError::Unauthorized(_))));
        let st = c.auth().unwrap();
        assert_eq!((st.state.as_str(), st.rejected_logins), ("aanmelden nodig", 1));
        c.poller.reset();
        assert!(matches!(c.read().await, Err(CloudError::Backoff(_))), "one login per 30 min");
        let mut c = c.with_gate(LoginGate::new(Duration::ZERO));
        for _ in 0..3 {
            c.poller.reset();
            let _ = c.read().await;
        }
        assert!(c.auth().unwrap().blocked);
        let logins = r.ilog.count(AUTHENTICATE);

        r.iscript.set(AUTHENTICATE, Reply::redirect(r.portal.url("/services/callbacklogin?code=c7")));
        let m = |v: Value| v.as_object().unwrap().clone();
        let st = c
            .set_credentials(&m(json!({"email": "new@example.invalid", "password": "pw-2", "nickname": "De Buzz"})))
            .await
            .unwrap();
        assert_eq!((st.state.as_str(), st.credentials.as_str(), st.blocked), ("ok", "state", false));
        assert_eq!(r.ilog.count(AUTHENTICATE), logins + 1);
        assert_eq!(std::fs::metadata(&store).unwrap().permissions().mode() & 0o777, 0o600);
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(
            (saved["email"].as_str(), saved["vin"].as_str(), saved["identifier"].as_str()),
            (Some("new@example.invalid"), Some(VIN), Some("ident-1"))
        );
        assert_eq!(Cupra::new(cfg).unwrap().auth().unwrap().credentials, "state");
        let fields =
            m(json!({"email": "e", "password": "p", "vin": "VIN2", "identifier": "ident-9", "brand": "cupra"}));
        let st = CloudSource::set_credentials(&mut c, &fields).await.unwrap();
        assert_eq!(st.state, "ok");
        assert_eq!((c.identifier.as_str(), c.creds.brand.as_str(), c.creds.vin.expose()), ("ident-9", "cupra", "VIN2"));
        for bad in [
            json!({"email": "e"}),
            json!({"email": "e", "password": "p", "brand": "trabant"}),
            json!({"email": "e", "password": "p", "pin": "1"}),
        ] {
            assert!(matches!(c.set_credentials(&m(bad)).await, Err(CloudError::Config(_))));
        }
        let mut no_store = r.cupra();
        no_store.store = None;
        assert!(
            matches!(no_store.set_credentials(&m(json!({"email": "e", "password": "p"}))).await, Err(CloudError::Config(m)) if m.contains("state dir"))
        );
    }
}
