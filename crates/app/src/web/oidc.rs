//! OpenID Connect sign-in, for any provider.
//!
//! Two ways in, both checked against the allow-list of `[oidc]`:
//!
//! - **Browsers**: the authorization code flow with PKCE. `/auth/callback` validates the ID token
//!   and starts a server-side session, kept in memory, behind an `HttpOnly`, `SameSite=Lax`
//!   cookie.
//! - **API clients**: `Authorization: Bearer <JWT access token>`, only when `api_audience` is set.
//!   Its audience must differ from the client id, so an ID token never works as an access token.
//!
//! The provider is only needed to sign in. Nothing here runs before the first request, and the
//! controller keeps running without it. When it cannot be reached, sessions that exist stay
//! valid, and new sign-ins fail closed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::{JwkSet, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Host;

use super::basic_auth_ok;
use crate::config::OidcConfig;

const SESSION_COOKIE: &str = "energie_session";
const LOGIN_COOKIE: &str = "energie_login";
const LOGIN_TTL: Duration = Duration::from_secs(600);
const MAX_PENDING: usize = 256;
const MAX_SESSIONS: usize = 1024;
/// Unknown key ids are attacker-controlled: they refetch the keys at most this often.
const MIN_KEY_REFRESH: Duration = Duration::from_secs(30);
const MAX_KEY_AGE: Duration = Duration::from_secs(3600);
/// Asymmetric only: an HMAC or `none` token is refused before any key is looked at.
const ALGORITHMS: [Algorithm; 8] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
];

#[derive(Clone, Deserialize)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Default)]
struct Keys {
    keys: Vec<(Option<String>, DecodingKey)>,
    fetched: Option<Instant>,
}

struct Pending {
    nonce: String,
    verifier: String,
    next: String,
    at: Instant,
}

struct Session {
    user: String,
    expires: Instant,
}

/// The provider connection, the logins in progress and the sessions.
pub struct Oidc {
    cfg: OidcConfig,
    client_secret: Option<String>,
    http: reqwest::Client,
    metadata: RwLock<Option<Metadata>>,
    keys: RwLock<Keys>,
    pending: Mutex<HashMap<String, Pending>>,
    sessions: Mutex<HashMap<String, Session>>,
}

/// Whether a URL is HTTPS, or on this machine.
pub(crate) fn secure_url(url: &str) -> bool {
    Url::parse(url).is_ok_and(|u| match u.scheme() {
        "https" => true,
        "http" => match u.host() {
            Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        },
        _ => false,
    })
}

fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A same-site path to return to; anything else goes to the start page.
fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains(['\\', '\r', '\n']) => n.to_string(),
        _ => "/".to_string(),
    }
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

impl Oidc {
    pub fn new(cfg: OidcConfig, client_secret: Option<String>) -> crate::error::Result<Oidc> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Oidc {
            cfg,
            client_secret,
            http,
            metadata: RwLock::new(None),
            keys: RwLock::new(Keys::default()),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        })
    }

    fn secure_cookies(&self) -> bool {
        self.cfg.redirect_url.starts_with("https:")
    }

    fn cookie_value(&self, name: &str, value: &str, path: &str, max_age: u64) -> String {
        let secure = if self.secure_cookies() { "; Secure" } else { "" };
        format!("{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; SameSite=Lax{secure}")
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, String> {
        let response = self.http.get(url).send().await.map_err(|e| format!("{url}: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("{url}: HTTP {}", response.status()));
        }
        response.json().await.map_err(|e| format!("{url}: {e}"))
    }

    /// Reach the provider once, for the log at start.
    pub async fn check(&self) -> Result<(), String> {
        self.metadata().await.map(|_| ())
    }

    /// The provider's metadata, fetched once. Its endpoints must be as secure as the issuer.
    async fn metadata(&self) -> Result<Metadata, String> {
        if let Some(m) = self.metadata.read().expect("metadata lock").clone() {
            return Ok(m);
        }
        let url = format!("{}/.well-known/openid-configuration", self.cfg.issuer.trim_end_matches('/'));
        let m: Metadata = self.get_json(&url).await?;
        if m.issuer != self.cfg.issuer {
            return Err(format!("the provider calls itself {:?}, not {:?}", m.issuer, self.cfg.issuer));
        }
        for endpoint in [&m.authorization_endpoint, &m.token_endpoint, &m.jwks_uri] {
            if !secure_url(endpoint) {
                return Err(format!("{endpoint}: not HTTPS"));
            }
        }
        *self.metadata.write().expect("metadata lock") = Some(m.clone());
        Ok(m)
    }

    async fn refresh_keys(&self) -> Result<(), String> {
        if self.keys.read().expect("keys lock").fetched.is_some_and(|t| t.elapsed() < MIN_KEY_REFRESH) {
            return Ok(());
        }
        let meta = self.metadata().await?;
        let set: JwkSet = self.get_json(&meta.jwks_uri).await?;
        let keys = set
            .keys
            .iter()
            .filter(|k| k.common.public_key_use != Some(PublicKeyUse::Encryption))
            .filter_map(|k| Some((k.common.key_id.clone(), DecodingKey::from_jwk(k).ok()?)))
            .collect();
        *self.keys.write().expect("keys lock") = Keys { keys, fetched: Some(Instant::now()) };
        Ok(())
    }

    fn cached_key(&self, kid: Option<&str>) -> Option<DecodingKey> {
        let keys = self.keys.read().expect("keys lock");
        match kid {
            Some(kid) => keys.keys.iter().find(|(id, _)| id.as_deref() == Some(kid)).map(|(_, k)| k.clone()),
            None if keys.keys.len() == 1 => Some(keys.keys[0].1.clone()),
            None => None,
        }
    }

    async fn key(&self, kid: Option<&str>) -> Result<DecodingKey, String> {
        let age = self.keys.read().expect("keys lock").fetched.map(|t| t.elapsed());
        if age.is_none_or(|a| a > MAX_KEY_AGE) {
            // A provider that is down leaves the keys we have in use.
            if let Err(e) = self.refresh_keys().await {
                tracing::warn!(error = %e, "oidc: could not refresh the signing keys");
            }
        }
        if let Some(k) = self.cached_key(kid) {
            return Ok(k);
        }
        self.refresh_keys().await?;
        self.cached_key(kid).ok_or_else(|| "no signing key for this token".to_string())
    }

    async fn verify(&self, token: &str, audience: &str) -> Result<Value, String> {
        let head = decode_header(token).map_err(|e| e.to_string())?;
        if !ALGORITHMS.contains(&head.alg) {
            return Err(format!("signature algorithm {:?} is not accepted", head.alg));
        }
        let key = self.key(head.kid.as_deref()).await?;
        let mut v = Validation::new(head.alg);
        v.set_issuer(&[&self.cfg.issuer]);
        v.set_audience(&[audience]);
        v.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        v.validate_nbf = true;
        decode::<Value>(token, &key, &v).map(|t| t.claims).map_err(|e| e.to_string())
    }

    /// Whether the claims match the allow-list. A provider account alone is not enough.
    fn allowed(&self, claims: &Value) -> bool {
        let sub = claims["sub"].as_str().unwrap_or_default();
        if !sub.is_empty() && self.cfg.allowed_subjects.iter().any(|s| s == sub) {
            return true;
        }
        let verified =
            matches!(&claims["email_verified"], Value::Bool(true)) || claims["email_verified"].as_str() == Some("true");
        if verified
            && let Some(email) = claims["email"].as_str()
            && self.cfg.allowed_emails.iter().any(|e| e.eq_ignore_ascii_case(email))
        {
            return true;
        }
        claims[self.cfg.groups_claim.as_str()].as_array().is_some_and(|gs| {
            gs.iter().filter_map(Value::as_str).any(|g| self.cfg.allowed_groups.iter().any(|a| a == g))
        })
    }

    fn remember(&self, state: String, p: Pending) {
        let mut pending = self.pending.lock().expect("pending lock");
        pending.retain(|_, v| v.at.elapsed() < LOGIN_TTL);
        if pending.len() >= MAX_PENDING
            && let Some(oldest) = pending.iter().min_by_key(|(_, v)| v.at).map(|(k, _)| k.clone())
        {
            pending.remove(&oldest);
        }
        pending.insert(state, p);
    }

    fn take_pending(&self, state: &str) -> Option<Pending> {
        self.pending.lock().expect("pending lock").remove(state).filter(|p| p.at.elapsed() < LOGIN_TTL)
    }

    fn new_session(&self, user: String) -> String {
        let id = random_token();
        let mut sessions = self.sessions.lock().expect("sessions lock");
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        if sessions.len() >= MAX_SESSIONS
            && let Some(first) = sessions.iter().min_by_key(|(_, s)| s.expires).map(|(k, _)| k.clone())
        {
            sessions.remove(&first);
        }
        let expires = now + Duration::from_secs(self.cfg.session_hours * 3600);
        sessions.insert(id.clone(), Session { user, expires });
        id
    }

    fn session_user(&self, id: &str) -> Option<String> {
        let sessions = self.sessions.lock().expect("sessions lock");
        sessions.get(id).filter(|s| s.expires > Instant::now()).map(|s| s.user.clone())
    }

    fn session_response(&self, user: String, next: &str) -> Response {
        let id = self.new_session(user);
        let max_age = self.cfg.session_hours * 3600;
        let mut r = (StatusCode::SEE_OTHER, [(header::LOCATION, next.to_string())]).into_response();
        for c in [self.cookie_value(SESSION_COOKIE, &id, "/", max_age), self.cookie_value(LOGIN_COOKIE, "", "/auth", 0)]
        {
            r.headers_mut().append(header::SET_COOKIE, c.parse().expect("cookie text is ASCII"));
        }
        r
    }

    async fn exchange(&self, meta: &Metadata, code: &str, verifier: &str) -> Result<String, String> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.cfg.redirect_url.as_str()),
            ("client_id", self.cfg.client_id.as_str()),
            ("code_verifier", verifier),
        ];
        let mut request = self.http.post(&meta.token_endpoint);
        if let Some(secret) = &self.client_secret {
            if meta.token_endpoint_auth_methods_supported.iter().any(|m| m == "client_secret_post") {
                form.push(("client_secret", secret.as_str()));
            } else {
                request = request.basic_auth(&self.cfg.client_id, Some(secret));
            }
        }
        let response = request.form(&form).send().await.map_err(|e| format!("token endpoint: {e}"))?;
        let status = response.status();
        let body: Value = response.json().await.map_err(|e| format!("token endpoint: {e}"))?;
        if !status.is_success() {
            return Err(format!("token endpoint: HTTP {status}: {}", body["error"]));
        }
        body["id_token"].as_str().map(str::to_string).ok_or_else(|| "no id_token in the response".to_string())
    }
}

struct Guard {
    password: Option<String>,
    oidc: Arc<Oidc>,
}

/// Put sign-in in front of the router: an OIDC session, a bearer token, or the optional password.
pub fn protect(router: Router, password: Option<String>, oidc: Arc<Oidc>) -> Router {
    let guard = Arc::new(Guard { password, oidc });
    let mut auth = Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/auth/logged-out", get(logged_out))
        .route("/auth/me", get(me));
    if guard.password.is_some() {
        auth = auth.route("/auth/basic", get(password_login));
    }
    router.merge(auth.with_state(guard.clone())).layer(middleware::from_fn_with_state(guard, check))
}

fn origin_ok(oidc: &Oidc, method: &Method, headers: &HeaderMap) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    Url::parse(&oidc.cfg.redirect_url).is_ok_and(|u| u.origin().ascii_serialization() == origin)
}

async fn check(State(g): State<Arc<Guard>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if matches!(path.as_str(), "/healthz" | "/auth/login" | "/auth/callback" | "/auth/logged-out" | "/auth/basic") {
        return next.run(req).await;
    }
    let local_cli = path.starts_with("/api/cloud")
        && req.extensions().get::<ConnectInfo<std::net::SocketAddr>>().is_some_and(|peer| peer.0.ip().is_loopback());
    if local_cli {
        return next.run(req).await;
    }
    let authorization = req.headers().get(header::AUTHORIZATION).cloned();
    if let Some(password) = &g.password
        && basic_auth_ok(authorization.as_ref(), password)
    {
        return next.run(req).await;
    }
    if let Some(token) = authorization.as_ref().and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")) {
        // A bearer token that fails is refused outright, whatever cookie came with it.
        if let Some(audience) = &g.oidc.cfg.api_audience {
            match g.oidc.verify(token.trim(), audience).await {
                Ok(claims) if g.oidc.allowed(&claims) => return next.run(req).await,
                Ok(claims) => tracing::warn!(sub = %claims["sub"], "oidc: token holder is not on the allow-list"),
                Err(e) => tracing::info!(error = %e, "oidc: bearer token refused"),
            }
        }
        return unauthorized();
    }
    if let Some(id) = cookie(req.headers(), SESSION_COOKIE)
        && g.oidc.session_user(id).is_some()
    {
        if !origin_ok(&g.oidc, req.method(), req.headers()) {
            return (StatusCode::FORBIDDEN, "cross-origin request refused\n").into_response();
        }
        return next.run(req).await;
    }
    let navigation = matches!(*req.method(), Method::GET | Method::HEAD)
        && !path.starts_with("/api/")
        && !path.starts_with("/assets/")
        && !path.starts_with("/auth/");
    if navigation {
        let target = req.uri().path_and_query().map_or("/", |p| p.as_str());
        let mut url = Url::parse("http://x/auth/login").expect("valid URL");
        url.query_pairs_mut().append_pair("next", target);
        let location = format!("{}?{}", url.path(), url.query().unwrap_or_default());
        return (StatusCode::FOUND, [(header::LOCATION, location)]).into_response();
    }
    unauthorized()
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "sign-in required\n").into_response()
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    (
        status,
        Html(format!(
            "<!doctype html><html lang=\"nl\"><meta charset=\"utf-8\"><title>{title}</title>\
             <body style=\"font-family:sans-serif;margin:3em\"><h1>{title}</h1>{body}</body></html>"
        )),
    )
        .into_response()
}

fn unavailable(g: &Guard) -> Response {
    let fallback =
        if g.password.is_some() { "<p><a href=\"/auth/basic\">Aanmelden met het noodwachtwoord</a></p>" } else { "" };
    page(
        StatusCode::SERVICE_UNAVAILABLE,
        "Aanmelden lukt nu niet",
        &format!("<p>De aanmeldservice is niet bereikbaar.</p>{fallback}"),
    )
}

#[derive(Deserialize)]
struct LoginParams {
    next: Option<String>,
}

async fn login(State(g): State<Arc<Guard>>, Query(q): Query<LoginParams>) -> Response {
    let o = &g.oidc;
    let meta = match o.metadata().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "oidc: provider unavailable");
            return unavailable(&g);
        }
    };
    let Ok(mut url) = Url::parse(&meta.authorization_endpoint) else {
        return unavailable(&g);
    };
    let (state, nonce, verifier) = (random_token(), random_token(), random_token());
    let scope = o.cfg.scopes.join(" ");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &o.cfg.client_id)
        .append_pair("redirect_uri", &o.cfg.redirect_url)
        .append_pair("scope", &scope)
        .append_pair("state", &state)
        .append_pair("nonce", &nonce)
        .append_pair("code_challenge", &pkce_challenge(&verifier))
        .append_pair("code_challenge_method", "S256");
    let login_cookie = o.cookie_value(LOGIN_COOKIE, &state, "/auth", LOGIN_TTL.as_secs());
    let next = safe_next(q.next.as_deref());
    o.remember(state, Pending { nonce, verifier, next, at: Instant::now() });
    (StatusCode::FOUND, [(header::LOCATION, url.to_string()), (header::SET_COOKIE, login_cookie)]).into_response()
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

async fn callback(State(g): State<Arc<Guard>>, headers: HeaderMap, Query(q): Query<CallbackParams>) -> Response {
    let o = &g.oidc;
    let denied = |why: &str| {
        tracing::warn!(reason = why, "oidc: sign-in refused");
        page(StatusCode::FORBIDDEN, "Geen toegang", "<p><a href=\"/auth/login\">Opnieuw proberen</a></p>")
    };
    if q.error.is_some() {
        return denied("the provider returned an error");
    }
    let (Some(code), Some(state)) = (q.code.as_deref(), q.state.as_deref()) else {
        return denied("incomplete callback");
    };
    if cookie(&headers, LOGIN_COOKIE) != Some(state) {
        return denied("state does not match this browser");
    }
    let Some(pending) = o.take_pending(state) else {
        return denied("unknown or expired state");
    };
    let meta = match o.metadata().await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "oidc: provider unavailable");
            return unavailable(&g);
        }
    };
    let id_token = match o.exchange(&meta, code, &pending.verifier).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "oidc: code exchange failed");
            return denied("code exchange failed");
        }
    };
    let claims = match o.verify(&id_token, &o.cfg.client_id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "oidc: ID token refused");
            return denied("invalid ID token");
        }
    };
    if claims["nonce"].as_str() != Some(pending.nonce.as_str()) {
        return denied("nonce does not match");
    }
    if !o.allowed(&claims) {
        tracing::warn!(sub = %claims["sub"], "oidc: account is not on the allow-list");
        return denied("not on the allow-list");
    }
    let user = claims["email"].as_str().or(claims["sub"].as_str()).unwrap_or("?").to_string();
    tracing::info!(user = %user, "oidc: signed in");
    o.session_response(user, &pending.next)
}

/// The emergency way in, when both a password and OIDC are configured: a session for the password.
async fn password_login(State(g): State<Arc<Guard>>, headers: HeaderMap) -> Response {
    let ok = g.password.as_deref().is_some_and(|p| basic_auth_ok(headers.get(header::AUTHORIZATION), p));
    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"energie\"")],
            "password required\n",
        )
            .into_response();
    }
    tracing::info!("signed in with the emergency password");
    g.oidc.session_response("noodwachtwoord".to_string(), "/")
}

async fn logout(State(g): State<Arc<Guard>>, headers: HeaderMap) -> Response {
    if let Some(id) = cookie(&headers, SESSION_COOKIE) {
        g.oidc.sessions.lock().expect("sessions lock").remove(id);
    }
    let cleared = g.oidc.cookie_value(SESSION_COOKIE, "", "/", 0);
    (StatusCode::SEE_OTHER, [(header::LOCATION, "/auth/logged-out".to_string()), (header::SET_COOKIE, cleared)])
        .into_response()
}

async fn logged_out() -> Response {
    page(StatusCode::OK, "Afgemeld", "<p><a href=\"/auth/login\">Opnieuw aanmelden</a></p>")
}

async fn me(State(g): State<Arc<Guard>>, headers: HeaderMap) -> Response {
    let user = cookie(&headers, SESSION_COOKIE).and_then(|id| g.oidc.session_user(id));
    Json(json!({"logout": user.is_some(), "user": user})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use jsonwebtoken::{EncodingKey, Header, encode};
    use tower::ServiceExt;

    const CLIENT: &str = "energie-web";
    const API: &str = "energie-api";

    struct Idp {
        issuer: String,
        key: EncodingKey,
        /// What the token endpoint puts in the next ID token, and what it last received.
        claims: Arc<Mutex<Value>>,
        token_request: Arc<Mutex<String>>,
    }

    fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    impl Idp {
        async fn start() -> Idp {
            let pair = rcgen::KeyPair::generate().unwrap();
            let raw = pair.public_key_raw().to_vec();
            let jwks = json!({"keys": [{
                "kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig", "alg": "ES256",
                "x": URL_SAFE_NO_PAD.encode(&raw[1..33]), "y": URL_SAFE_NO_PAD.encode(&raw[33..65]),
            }]});
            let key = EncodingKey::from_ec_pem(pair.serialize_pem().as_bytes()).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let issuer = format!("http://{}", listener.local_addr().unwrap());
            let meta = json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/authorize"),
                "token_endpoint": format!("{issuer}/token"),
                "jwks_uri": format!("{issuer}/jwks"),
            });
            let claims = Arc::new(Mutex::new(json!({})));
            let token_request = Arc::new(Mutex::new(String::new()));
            let (signer, reply, seen) = (key.clone(), claims.clone(), token_request.clone());
            let app = Router::new()
                .route("/.well-known/openid-configuration", get(move || async move { Json(meta) }))
                .route("/jwks", get(move || async move { Json(jwks) }))
                .route(
                    "/token",
                    post(move |body: String| async move {
                        *seen.lock().unwrap() = body;
                        let claims = reply.lock().unwrap().clone();
                        let token = encode(&es256(), &claims, &signer).unwrap();
                        Json(json!({"id_token": token, "access_token": "opaque"}))
                    }),
                );
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Idp { issuer, key, claims, token_request }
        }

        fn base_claims(&self, aud: &str) -> Value {
            json!({"iss": self.issuer, "aud": aud, "sub": "user-1", "exp": now() + 600})
        }

        fn sign(&self, claims: &Value) -> String {
            encode(&es256(), claims, &self.key).unwrap()
        }
    }

    fn es256() -> Header {
        Header { kid: Some("k1".into()), ..Header::new(Algorithm::ES256) }
    }

    fn config(idp: &Idp) -> OidcConfig {
        OidcConfig {
            issuer: idp.issuer.clone(),
            client_id: CLIENT.into(),
            client_secret_file: None,
            redirect_url: "http://127.0.0.1:8080/auth/callback".into(),
            scopes: vec!["openid".into(), "email".into()],
            allowed_subjects: vec!["user-1".into()],
            allowed_emails: vec!["Owner@Example.org".into()],
            allowed_groups: vec!["energy-admins".into()],
            groups_claim: "groups".into(),
            api_audience: Some(API.into()),
            session_hours: 12,
        }
    }

    fn inner() -> Router {
        Router::new()
            .route("/", get(|| async { "dashboard" }))
            .route("/api/state", get(|| async { "state" }))
            .route("/api/settings", post(|| async { "changed" }))
            .route("/healthz", get(|| async { "ok" }))
    }

    fn app(idp: &Idp, password: Option<&str>) -> Router {
        protect(inner(), password.map(str::to_string), Arc::new(Oidc::new(config(idp), None).unwrap()))
    }

    fn get_req(path: &str) -> axum::http::request::Builder {
        axum::http::Request::builder().uri(path)
    }

    async fn send(app: &Router, r: axum::http::request::Builder) -> Response {
        app.clone().oneshot(r.body(Body::empty()).unwrap()).await.unwrap()
    }

    fn location(r: &Response) -> String {
        r.headers()[header::LOCATION].to_str().unwrap().to_string()
    }

    fn set_cookie(r: &Response, name: &str) -> String {
        let v = r
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap())
            .find(|v| v.starts_with(&format!("{name}=")))
            .unwrap_or_else(|| panic!("no {name} cookie"));
        v.split(';').next().unwrap().split_once('=').unwrap().1.to_string()
    }

    fn query(url: &str, key: &str) -> String {
        Url::parse(url).unwrap().query_pairs().find(|(k, _)| k == key).unwrap().1.into_owned()
    }

    /// Runs the browser flow to the callback; the ID token gets `claims` plus the login's nonce.
    async fn sign_in(app: &Router, idp: &Idp, mut claims: Value) -> Response {
        let start = send(app, get_req("/auth/login?next=/%23ev")).await;
        assert_eq!(start.status(), StatusCode::FOUND);
        let to = location(&start);
        let state = query(&to, "state");
        claims["nonce"] = json!(query(&to, "nonce"));
        *idp.claims.lock().unwrap() = claims;
        let cookie = format!("{LOGIN_COOKIE}={}", set_cookie(&start, LOGIN_COOKIE));
        send(app, get_req(&format!("/auth/callback?code=c1&state={state}")).header(header::COOKIE, cookie)).await
    }

    /// The whole browser flow: sent to the provider with PKCE, back with a session that works,
    /// to the page asked for; signing out ends it.
    #[tokio::test]
    async fn a_browser_signs_in_uses_the_session_and_signs_out() {
        let idp = Idp::start().await;
        let app = app(&idp, None);
        let page = send(&app, get_req("/?x=1")).await;
        assert_eq!(page.status(), StatusCode::FOUND);
        assert_eq!(location(&page), "/auth/login?next=%2F%3Fx%3D1");

        let start = send(&app, get_req("/auth/login")).await;
        let to = location(&start);
        assert!(to.starts_with(&format!("{}/authorize", idp.issuer)), "{to}");
        assert_eq!(query(&to, "code_challenge_method"), "S256");
        assert_eq!(query(&to, "client_id"), CLIENT);

        let done = sign_in(&app, &idp, idp.base_claims(CLIENT)).await;
        assert_eq!(done.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&done), "/#ev");
        let verifier = query(&format!("http://x/?{}", idp.token_request.lock().unwrap()), "code_verifier");
        assert!(verifier.len() >= 43);
        let session = format!("{SESSION_COOKIE}={}", set_cookie(&done, SESSION_COOKIE));
        let raw = done.headers().get_all(header::SET_COOKIE).iter().next().unwrap().to_str().unwrap().to_string();
        assert!(raw.contains("HttpOnly") && raw.contains("SameSite=Lax"), "{raw}");

        let ok = send(&app, get_req("/api/state").header(header::COOKIE, &session)).await;
        assert_eq!(ok.status(), StatusCode::OK);
        let me = send(&app, get_req("/auth/me").header(header::COOKIE, &session)).await;
        let body = me.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("user-1"));

        let out = send(&app, get_req("/auth/logout").method("POST").header(header::COOKIE, &session)).await;
        assert_eq!(location(&out), "/auth/logged-out");
        let gone = send(&app, get_req("/api/state").header(header::COOKIE, &session)).await;
        assert_eq!(gone.status(), StatusCode::UNAUTHORIZED);
    }

    /// Control routes follow the session, and a write from another site is refused.
    #[tokio::test]
    async fn writes_need_a_session_from_the_same_origin() {
        let idp = Idp::start().await;
        let app = app(&idp, None);
        let write = || get_req("/api/settings").method("POST");
        assert_eq!(send(&app, write()).await.status(), StatusCode::UNAUTHORIZED);
        let done = sign_in(&app, &idp, idp.base_claims(CLIENT)).await;
        let session = format!("{SESSION_COOKIE}={}", set_cookie(&done, SESSION_COOKIE));
        assert_eq!(send(&app, write().header(header::COOKIE, &session)).await.status(), StatusCode::OK);
        let same = write().header(header::COOKIE, &session).header(header::ORIGIN, "http://127.0.0.1:8080");
        assert_eq!(send(&app, same).await.status(), StatusCode::OK);
        let cross = write().header(header::COOKIE, &session).header(header::ORIGIN, "https://evil.example");
        assert_eq!(send(&app, cross).await.status(), StatusCode::FORBIDDEN);
    }

    /// An account the provider knows but the allow-list does not gets no session.
    #[tokio::test]
    async fn a_provider_account_alone_is_not_enough() {
        let idp = Idp::start().await;
        let app = app(&idp, None);
        let mut claims = idp.base_claims(CLIENT);
        claims["sub"] = json!("someone-else");
        let r = sign_in(&app, &idp, claims).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        assert!(r.headers().get_all(header::SET_COOKIE).iter().next().is_none());
    }

    /// The allow-list: subject, verified email (any case), or group.
    #[tokio::test]
    async fn the_allow_list_matches_subject_verified_email_or_group() {
        let idp = Idp::start().await;
        let o = Oidc::new(config(&idp), None).unwrap();
        assert!(o.allowed(&json!({"sub": "user-1"})));
        assert!(o.allowed(&json!({"sub": "x", "email": "owner@example.org", "email_verified": true})));
        assert!(!o.allowed(&json!({"sub": "x", "email": "owner@example.org", "email_verified": false})));
        assert!(!o.allowed(&json!({"sub": "x", "email": "owner@example.org"})));
        assert!(o.allowed(&json!({"sub": "x", "groups": ["other", "energy-admins"]})));
        assert!(!o.allowed(&json!({"sub": "x", "groups": ["other"]})));
        assert!(!o.allowed(&json!({})));
    }

    /// A wrong nonce, a wrong audience or an expired token do not start a session.
    #[tokio::test]
    async fn id_tokens_are_checked() {
        let idp = Idp::start().await;
        let app = app(&idp, None);
        for bad in [
            json!({"iss": idp.issuer, "aud": "other-client", "sub": "user-1", "exp": now() + 600}),
            json!({"iss": "https://other.example", "aud": CLIENT, "sub": "user-1", "exp": now() + 600}),
            json!({"iss": idp.issuer, "aud": CLIENT, "sub": "user-1", "exp": now() - 3600}),
        ] {
            assert_eq!(sign_in(&app, &idp, bad).await.status(), StatusCode::FORBIDDEN);
        }
        let start = send(&app, get_req("/auth/login")).await;
        let mut claims = idp.base_claims(CLIENT);
        claims["nonce"] = json!("not-the-nonce");
        *idp.claims.lock().unwrap() = claims;
        let state = query(&location(&start), "state");
        let cookie = format!("{LOGIN_COOKIE}={}", set_cookie(&start, LOGIN_COOKIE));
        let r =
            send(&app, get_req(&format!("/auth/callback?code=c&state={state}")).header(header::COOKIE, cookie)).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }

    /// The callback must come back to the browser that started, and only once.
    #[tokio::test]
    async fn the_state_is_bound_to_the_browser_and_single_use() {
        let idp = Idp::start().await;
        let app = app(&idp, None);
        let start = send(&app, get_req("/auth/login")).await;
        let to = location(&start);
        let state = query(&to, "state");
        let mut claims = idp.base_claims(CLIENT);
        claims["nonce"] = json!(query(&to, "nonce"));
        *idp.claims.lock().unwrap() = claims;
        let callback = format!("/auth/callback?code=c&state={state}");
        assert_eq!(send(&app, get_req(&callback)).await.status(), StatusCode::FORBIDDEN);
        let wrong = get_req(&callback).header(header::COOKIE, format!("{LOGIN_COOKIE}=other"));
        assert_eq!(send(&app, wrong).await.status(), StatusCode::FORBIDDEN);
        let cookie = format!("{LOGIN_COOKIE}={state}");
        assert_eq!(
            send(&app, get_req(&callback).header(header::COOKIE, &cookie)).await.status(),
            StatusCode::SEE_OTHER
        );
        assert_eq!(
            send(&app, get_req(&callback).header(header::COOKIE, &cookie)).await.status(),
            StatusCode::FORBIDDEN
        );
    }

    /// API clients: a good token passes; the wrong audience (an ID token), an expired token, a
    /// token off the allow-list, a forged signature or an HMAC token do not.
    #[tokio::test]
    async fn bearer_tokens_are_verified() {
        let idp = Idp::start().await;
        let app = app(&idp, None);
        let call = |t: String| get_req("/api/state").header(header::AUTHORIZATION, format!("Bearer {t}"));
        assert_eq!(send(&app, call(idp.sign(&idp.base_claims(API)))).await.status(), StatusCode::OK);
        let cases = [
            idp.sign(&idp.base_claims(CLIENT)),
            idp.sign(&json!({"iss": idp.issuer, "aud": API, "sub": "user-1", "exp": now() - 3600})),
            idp.sign(&json!({"iss": idp.issuer, "aud": API, "sub": "stranger", "exp": now() + 600})),
            encode(
                &es256(),
                &idp.base_claims(API),
                &EncodingKey::from_ec_pem(rcgen::KeyPair::generate().unwrap().serialize_pem().as_bytes()).unwrap(),
            )
            .unwrap(),
            encode(&Header::new(Algorithm::HS256), &idp.base_claims(API), &EncodingKey::from_secret(b"guess")).unwrap(),
            "not-a-token".to_string(),
        ];
        for t in cases {
            assert_eq!(send(&app, call(t)).await.status(), StatusCode::UNAUTHORIZED);
        }
    }

    /// Without `api_audience` no bearer token is accepted at all.
    #[tokio::test]
    async fn bearer_tokens_are_off_without_an_api_audience() {
        let idp = Idp::start().await;
        let cfg = OidcConfig { api_audience: None, ..config(&idp) };
        let app = protect(inner(), None, Arc::new(Oidc::new(cfg, None).unwrap()));
        let r =
            get_req("/api/state").header(header::AUTHORIZATION, format!("Bearer {}", idp.sign(&idp.base_claims(API))));
        assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
    }

    /// API calls get a 401, never a login page; the health check stays open.
    #[tokio::test]
    async fn only_page_views_are_redirected() {
        let idp = Idp::start().await;
        let app = app(&idp, None);
        assert_eq!(send(&app, get_req("/api/state")).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(send(&app, get_req("/healthz")).await.status(), StatusCode::OK);
    }

    /// With OIDC alone a password means nothing.
    #[tokio::test]
    async fn without_a_password_basic_auth_is_refused() {
        use base64::engine::general_purpose::STANDARD;
        let idp = Idp::start().await;
        let app = app(&idp, None);
        let basic = format!("Basic {}", STANDARD.encode("u:a-long-test-password"));
        let r = get_req("/api/state").header(header::AUTHORIZATION, basic);
        assert_eq!(send(&app, r).await.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(send(&app, get_req("/auth/basic")).await.status(), StatusCode::NOT_FOUND);
    }

    /// The emergency password works next to OIDC, and gives a browser a session.
    #[tokio::test]
    async fn the_emergency_password_starts_a_session() {
        use base64::engine::general_purpose::STANDARD;
        let idp = Idp::start().await;
        let app = app(&idp, Some("a-long-test-password"));
        let basic = format!("Basic {}", STANDARD.encode("u:a-long-test-password"));
        let wrong = format!("Basic {}", STANDARD.encode("u:wrong"));
        assert_eq!(
            send(&app, get_req("/auth/basic").header(header::AUTHORIZATION, wrong)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let r = send(&app, get_req("/auth/basic").header(header::AUTHORIZATION, &basic)).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let session = format!("{SESSION_COOKIE}={}", set_cookie(&r, SESSION_COOKIE));
        assert_eq!(send(&app, get_req("/api/state").header(header::COOKIE, session)).await.status(), StatusCode::OK);
    }

    /// With the provider down, sign-in fails closed and says so; nothing else is affected.
    #[tokio::test]
    async fn a_provider_that_is_down_fails_closed() {
        let cfg = OidcConfig {
            issuer: "http://127.0.0.1:1".into(),
            client_id: CLIENT.into(),
            client_secret_file: None,
            redirect_url: "http://127.0.0.1:8080/auth/callback".into(),
            scopes: vec!["openid".into()],
            allowed_subjects: vec!["user-1".into()],
            allowed_emails: vec![],
            allowed_groups: vec![],
            groups_claim: "groups".into(),
            api_audience: Some(API.into()),
            session_hours: 12,
        };
        let app = protect(inner(), Some("a-long-test-password".into()), Arc::new(Oidc::new(cfg, None).unwrap()));
        let r = send(&app, get_req("/auth/login")).await;
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = r.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("/auth/basic"));
        assert_eq!(send(&app, get_req("/healthz")).await.status(), StatusCode::OK);
    }

    /// After sign-in only a path on this site is followed.
    #[test]
    fn the_return_path_stays_on_this_site() {
        assert_eq!(safe_next(Some("/#ev")), "/#ev");
        assert_eq!(safe_next(None), "/");
        for bad in ["https://evil.example", "//evil.example", "/\\evil.example", "evil", "/a\r\nX: y"] {
            assert_eq!(safe_next(Some(bad)), "/", "{bad}");
        }
    }

    #[test]
    fn urls_must_be_https_unless_local() {
        assert!(secure_url("https://id.example.org/"));
        assert!(secure_url("http://127.0.0.1:9000/"));
        assert!(secure_url("http://localhost/"));
        assert!(!secure_url("http://192.168.1.10/"));
        assert!(!secure_url("http://id.example.org/"));
        assert!(!secure_url("nonsense"));
    }
}
