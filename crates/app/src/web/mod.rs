//! The dashboard and JSON API (axum).
//!
//! | Route | |
//! | --- | --- |
//! | `GET /` | Dutch dashboard: server-rendered controls + initial state, live via SSE |
//! | `GET /api/state` | the latest [`Snapshot`] |
//! | `GET /api/settings` | settings, whether they are editable, and the control metadata |
//! | `POST /api/settings` | a partial settings change (`409` in shadow lockstep) |
//! | `GET /api/shadow` | the shadow comparison stats (`404` outside shadow mode) |
//! | `GET /api/entities` | every entity with its metadata and value ([`crate::entities`]) |
//! | `POST /api/entities/{id}` | `{"value": …}`: set a settable entity |
//! | `GET /api/events` | Server-Sent Events, one `state` event per tick; its `entities` field holds the entity values that changed since the stream's last event (all of them in the first) |
//! | `GET /api/history` | the history store: series and tiers, or `?series=a,b:max&from=&to=&step=` |
//! | `GET /api/prices` | today's (and tomorrow's) offtake and injection quarter prices |
//! | `GET /api/cloud` | `{id: {kind, auth}}`: the sign-in state of every device that logs in with a password |
//! | `POST /api/cloud/{id}/credentials` | new credentials for device `id` (loopback peers only; `energie credentials set`): stored in the state dir, then a login; answers the sign-in state |
//! | `GET /assets/*` | the page's stylesheet and scripts (compiled in) |
//! | `GET /healthz` | `200` while ticking, `503` when stale or not started |

pub mod fields;
pub mod oidc;
pub mod prices;

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use energie_core::time::TZ;
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::entities::{SetError, changes, values};
use crate::history::SharedHistory;
use crate::history::tier::Pick;
use crate::runtime::{Runtime, Snapshot};

#[derive(Clone)]
pub struct AppState {
    pub rt: Arc<Mutex<Runtime>>,
    pub snap: watch::Receiver<Arc<Snapshot>>,
    /// `/healthz` turns 503 when the last tick is older than this.
    pub stale_after: Duration,
    /// Turns true at shutdown; open event streams end then.
    pub stop: watch::Receiver<bool>,
    pub history: Option<SharedHistory>,
}

pub fn router(st: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(state))
        .route("/api/settings", get(settings_get).post(settings_post))
        .route("/api/shadow", get(shadow))
        .route("/api/control", get(control_get).post(control_post))
        .route("/api/entities", get(entities_get))
        .route("/api/entities/{id}", axum::routing::post(entity_post))
        .route("/api/events", get(events))
        .route("/api/history", get(history))
        .route("/api/prices", get(prices_get))
        .route("/api/cloud", get(cloud_get))
        .route("/api/cloud/{id}/credentials", axum::routing::post(credentials_post))
        .route("/assets/app.css", get(|| asset("text/css; charset=utf-8", include_str!("assets/app.css"))))
        .route("/assets/app.js", get(|| asset("text/javascript; charset=utf-8", include_str!("assets/app.js"))))
        .route("/assets/charts.js", get(|| asset("text/javascript; charset=utf-8", include_str!("assets/charts.js"))))
        .route("/healthz", get(healthz))
        .with_state(st)
}

/// Put the configured sign-in in front of the router. The config guarantees that nothing reachable
/// from the network ends up with neither.
pub fn protect(router: Router, password: Option<String>, oidc: Option<Arc<oidc::Oidc>>) -> Router {
    match (password, oidc) {
        (password, Some(oidc)) => oidc::protect(router, password, oidc),
        (Some(password), None) => require_password(router, password),
        (None, None) => router,
    }
}

/// HTTP Basic in front of the router (any user name, compared in constant time). `/healthz`
/// stays open for monitors, and the `energie credentials` command on the host itself keeps
/// working: those routes are served to loopback peers, and the credentials route refuses
/// everyone else on its own.
pub fn require_password(router: Router, password: String) -> Router {
    let password: Arc<str> = password.into();
    router.layer(axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
        let password = password.clone();
        async move {
            let local_cli = req.uri().path().starts_with("/api/cloud")
                && req
                    .extensions()
                    .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                    .is_some_and(|peer| peer.0.ip().is_loopback());
            if req.uri().path() == "/healthz"
                || local_cli
                || basic_auth_ok(req.headers().get(header::AUTHORIZATION), &password)
            {
                return next.run(req).await;
            }
            (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Basic realm=\"energie\"")], "password required\n")
                .into_response()
        }
    }))
}

/// Whether an `Authorization: Basic …` header carries `password`.
fn basic_auth_ok(value: Option<&header::HeaderValue>, password: &str) -> bool {
    use base64::Engine;
    let Some(encoded) = value.and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Basic ")) else {
        return false;
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Some(colon) = decoded.iter().position(|b| *b == b':') else {
        return false;
    };
    let (given, want) = (&decoded[colon + 1..], password.as_bytes());
    let diff = given.iter().zip(want).fold(given.len() ^ want.len(), |d, (a, b)| d | usize::from(a ^ b));
    diff == 0
}

/// The snapshot as sent to the browser: without the 96–192 price slots (their count stays).
pub fn live_json(s: &Snapshot) -> Value {
    let mut v = serde_json::to_value(s).expect("Snapshot serialises");
    let slots = s.world.prices.afname_slots.len();
    v["world"]["prices"]["afname_slots"] = json!(slots);
    v
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The controls, rendered on the server with the current values.
pub fn render_controls(settings: &Value, editable: bool) -> String {
    let mut groups: Vec<&str> = fields::fields().iter().map(|f| f.group).collect();
    groups.dedup();
    render_groups(settings, editable, &groups)
}

/// The controls of the given groups, each a fieldset, with the derived readouts that follow them.
pub fn render_groups(settings: &Value, editable: bool, groups: &[&str]) -> String {
    let dis = if editable { "" } else { " disabled" };
    let mut out = String::new();
    let fs = fields::fields();
    let readouts = fields::readouts();
    for &g in groups {
        out.push_str(&format!("<fieldset><legend>{}</legend>", esc(g)));
        for f in fs.iter().filter(|f| f.group == g) {
            let cur = &settings[f.key];
            let input = match f.kind {
                "select" => {
                    let opts: String = f
                        .options
                        .iter()
                        .map(|o| {
                            let v = o.value.to_string();
                            let sel = if &o.value == cur { " selected" } else { "" };
                            format!("<option value=\"{}\"{sel}>{}</option>", esc(&v), esc(&o.label))
                        })
                        .collect();
                    format!("<select name=\"{}\"{dis}>{opts}</select>", f.key)
                }
                "bool" => {
                    let chk = if cur == &Value::Bool(true) { " checked" } else { "" };
                    format!("<input type=\"checkbox\" name=\"{}\"{chk}{dis}>", f.key)
                }
                _ => format!("<input type=\"number\" name=\"{}\" step=\"{}\" value=\"{}\"{dis}>", f.key, f.step, cur),
            };
            let unit = if f.unit.is_empty() { String::new() } else { format!(" ({})", esc(f.unit)) };
            out.push_str(&format!("<label class=\"row\"><span>{}{unit}</span>{input}</label>", esc(f.label)));
            for r in readouts.iter().filter(|r| r.after == f.key) {
                out.push_str(&format!(
                    "<div class=\"row\"><span>{}</span><output data-d=\"{}\" data-unit=\"{}\" data-dec=\"{}\">–</output></div>",
                    esc(r.label),
                    r.derived,
                    esc(r.unit),
                    r.decimals
                ));
            }
        }
        out.push_str("</fieldset>");
    }
    out
}

/// The whole page for one snapshot.
pub fn render_page(s: &Snapshot) -> String {
    let settings = serde_json::to_value(&s.settings).expect("Settings serialise");
    let ro = if s.settings_editable {
        ""
    } else {
        "Alleen-lezen: in shadow-lockstep komen de instellingen uit Home Assistant."
    };
    include_str!("page.html")
        .replace("{{MODE}}", s.mode)
        .replace("{{READONLY}}", ro)
        .replace("{{CONTROLS_MODUS}}", &render_groups(&settings, s.settings_editable, &[fields::MODUS]))
        .replace("{{CONTROLS}}", &render_controls(&settings, s.settings_editable))
        .replace("{{STATE}}", &live_json(s).to_string().replace("</", "<\\/"))
}

async fn asset(kind: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

#[derive(Debug, Deserialize)]
struct HistoryParams {
    series: Option<String>,
    from: Option<String>,
    to: Option<String>,
    step: Option<i64>,
}

/// A time parameter: Unix seconds or RFC 3339.
fn param_time(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok().or_else(|| crate::ha::entity::parse_time(s).map(|t| t.timestamp()))
}

/// `series=a,b:max` → ids with the value to pick (default the mean).
fn parse_series(s: &str) -> Result<Vec<(String, Pick)>, String> {
    s.split(',')
        .filter(|x| !x.trim().is_empty())
        .map(|x| {
            let (id, agg) = x.trim().split_once(':').unwrap_or((x.trim(), "mean"));
            Pick::parse(agg).map(|p| (id.to_string(), p)).ok_or_else(|| format!("onbekende aggregatie: {agg}"))
        })
        .collect()
}

async fn history(State(st): State<AppState>, Query(q): Query<HistoryParams>) -> Response {
    let Some(h) = &st.history else {
        return (StatusCode::NOT_FOUND, "geen historiek (history.enabled = false)").into_response();
    };
    let h = h.lock().expect("history lock");
    let Some(series) = q.series else {
        return Json(json!({"series": h.meta(), "tiers": h.tiers(), "stats": h.stats})).into_response();
    };
    let bad = |m: String| (StatusCode::BAD_REQUEST, m).into_response();
    let want = match parse_series(&series) {
        Ok(w) => w,
        Err(e) => return bad(e),
    };
    // The tick clock's now: virtual in simulate mode.
    let now = st.snap.borrow().now.as_deref().and_then(crate::ha::entity::parse_time).map(|t| t.timestamp());
    let now = now.unwrap_or_else(|| chrono::Utc::now().timestamp());
    let time = |p: &Option<String>, default: i64| match p {
        None => Ok(default),
        Some(s) => param_time(s).ok_or_else(|| format!("ongeldige tijd: {s}")),
    };
    let to = match time(&q.to, now) {
        Ok(t) => t,
        Err(e) => return bad(e),
    };
    let from = match time(&q.from, to - 86_400) {
        Ok(t) => t,
        Err(e) => return bad(e),
    };
    match h.query(&want, from, to, q.step.unwrap_or(300)) {
        Ok(out) => Json(out).into_response(),
        Err(e) => bad(e.0),
    }
}

async fn prices_get(State(st): State<AppState>) -> Json<prices::Curve> {
    Json(prices::curve(&st.snap.borrow().world.prices))
}

async fn index(State(st): State<AppState>) -> Html<String> {
    Html(render_page(&st.snap.borrow()))
}

async fn state(State(st): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(&**st.snap.borrow()).expect("Snapshot serialises"))
}

async fn settings_get(State(st): State<AppState>) -> Json<Value> {
    let s = st.snap.borrow().clone();
    Json(json!({"editable": s.settings_editable, "values": s.settings, "fields": fields::fields()}))
}

async fn settings_post(State(st): State<AppState>, Json(patch): Json<Value>) -> Response {
    let mut rt = st.rt.lock().expect("runtime lock");
    if !rt.settings_editable() {
        return (StatusCode::CONFLICT, "instellingen komen uit Home Assistant (shadow lockstep)").into_response();
    }
    let now = chrono::Utc::now().with_timezone(&TZ);
    match rt.apply_settings(now, &patch) {
        Ok(s) => Json(s).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.0).into_response(),
    }
}

async fn shadow(State(st): State<AppState>) -> Response {
    match &st.snap.borrow().shadow {
        Some(s) => Json(s.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "geen shadow-modus").into_response(),
    }
}

async fn control_get(State(st): State<AppState>) -> Response {
    match &st.snap.borrow().control {
        Some(c) => Json(c.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "geen control-modus").into_response(),
    }
}

/// `{"enabled": false}` stops all actuation; `{"device": "solis", "enabled": true}` one device.
async fn control_post(State(st): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(on) = body.get("enabled").and_then(Value::as_bool) else {
        return (StatusCode::BAD_REQUEST, "verwacht {\"enabled\": true|false, \"device\"?: …}").into_response();
    };
    let device = match body.get("device").and_then(Value::as_str) {
        None => None,
        Some(name) => match crate::control::Control::switch_of(name) {
            Some(Some(d)) => Some(d),
            _ => return (StatusCode::BAD_REQUEST, format!("onbekend apparaat: {name}")).into_response(),
        },
    };
    let r = st.rt.lock().expect("runtime lock").set_control(device, on);
    match r {
        Ok(()) => Json(json!(st.snap.borrow().control)).into_response(),
        Err(e) => (StatusCode::CONFLICT, e.0).into_response(),
    }
}

async fn entities_get(State(st): State<AppState>) -> Json<Value> {
    Json(serde_json::to_value(&*st.snap.borrow().entities).expect("entities serialise"))
}

/// `{device id: {kind, auth}}` for the devices that log in with a password.
async fn cloud_get(State(st): State<AppState>) -> Json<Value> {
    let rt = st.rt.lock().expect("runtime lock");
    let all: serde_json::Map<String, Value> = rt
        .devices
        .iter()
        .filter_map(|d| d.health().auth.map(|a| (d.id().to_string(), json!({"kind": d.kind(), "auth": a}))))
        .collect();
    Json(Value::Object(all))
}

/// How long a credentials change may take (the login included).
const CREDENTIALS_TIMEOUT: Duration = Duration::from_secs(90);

/// New credentials for one device. Only from this host (the CLI): the body holds a password,
/// so it is never logged, echoed or accepted over the network.
async fn credentials_post(
    State(st): State<AppState>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    if !peer.is_some_and(|p| p.0.0.ip().is_loopback()) {
        return (StatusCode::FORBIDDEN, "alleen vanaf deze machine (energie credentials set)").into_response();
    }
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(&body) else {
        return (StatusCode::BAD_REQUEST, "verwacht een JSON-object").into_response();
    };
    let queued = {
        let rt = st.rt.lock().expect("runtime lock");
        let Some(d) = rt.devices.iter().find(|d| d.id() == id) else {
            return (StatusCode::NOT_FOUND, format!("onbekend toestel: {id}")).into_response();
        };
        d.set_credentials(fields)
    };
    let reply = match queued {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, e.0).into_response(),
    };
    match tokio::time::timeout(CREDENTIALS_TIMEOUT, reply).await {
        Ok(Ok(Ok(auth))) => Json(json!({"device": id, "auth": auth})).into_response(),
        Ok(Ok(Err(e))) => (StatusCode::BAD_REQUEST, e).into_response(),
        Ok(Err(_)) | Err(_) => (StatusCode::GATEWAY_TIMEOUT, "geen antwoord van het toestel").into_response(),
    }
}

async fn entity_post(State(st): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(v) = body.get("value").and_then(energie_drivers::entity::Value::from_json) else {
        return (StatusCode::BAD_REQUEST, "verwacht {\"value\": …}").into_response();
    };
    let now = chrono::Utc::now().with_timezone(&TZ);
    let r = st.rt.lock().expect("runtime lock").set_entity(now, &id, &v);
    match r {
        Ok(()) => {
            let snap = st.snap.borrow().clone();
            Json(json!(snap.entities.iter().find(|e| e.id == id))).into_response()
        }
        Err(SetError::Unknown) => (StatusCode::NOT_FOUND, format!("onbekende entiteit: {id}")).into_response(),
        Err(SetError::NotSettable) => {
            (StatusCode::CONFLICT, format!("{id} kan nu niet ingesteld worden")).into_response()
        }
        Err(SetError::Rejected(e)) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn healthz(State(st): State<AppState>) -> Response {
    let s = st.snap.borrow().clone();
    let age = s.made.map(|m| m.elapsed());
    let fresh = s.tick > 0 && age.is_some_and(|a| a <= st.stale_after);
    let body = json!({
        "status": if fresh { "ok" } else if s.tick == 0 { "starting" } else { "stale" },
        "mode": s.mode,
        "tick": s.tick,
        "last_tick_age_s": age.map(|a| a.as_secs_f64()),
        "ha_connected": s.ha.as_ref().map(|h| h.connected),
    });
    let code = if fresh { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(body)).into_response()
}

/// One event stream: the snapshot receiver, the stop signal, and the entity values it sent.
struct EventStream {
    rx: watch::Receiver<Arc<Snapshot>>,
    stop: watch::Receiver<bool>,
    sent: std::collections::BTreeMap<String, energie_drivers::entity::Value>,
}

async fn events(State(st): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = st.snap.clone();
    rx.mark_changed();
    let start = EventStream { rx, stop: st.stop.clone(), sent: Default::default() };
    let stream = futures_util::stream::unfold(start, |mut s| async move {
        tokio::select! {
            r = s.rx.changed() => r.ok()?,
            _ = s.stop.wait_for(|s| *s) => return None,
        }
        let snap = s.rx.borrow_and_update().clone();
        let now = values(&snap.entities);
        let mut data = live_json(&snap);
        data["entities"] = json!(changes(&s.sent, &now));
        s.sent = now;
        Some((Ok(Event::default().event("state").data(data.to_string())), s))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Mode, ShadowConfig};
    use crate::plug::tests::t0;
    use crate::runtime::tests::sim_parts;
    use crate::shadow::Shadow;
    use crate::shadow::log::MismatchLog;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn app(mode: Mode, lockstep: bool) -> (Router, Arc<Mutex<Runtime>>) {
        let (r, rt, stop) = app_with_stop(mode, lockstep);
        std::mem::forget(stop);
        (r, rt)
    }

    fn app_with_stop(mode: Mode, lockstep: bool) -> (Router, Arc<Mutex<Runtime>>, watch::Sender<bool>) {
        let (mut parts, _) = sim_parts(mode, vec![energie_core::command::Command::EvseCurrentDeciA(80)]);
        parts.lockstep = lockstep;
        if mode == Mode::Shadow {
            parts.shadow = Some(Shadow::new(&ShadowConfig::default(), MismatchLog::new(None, 0)));
        }
        let (rt, snap) = Runtime::new(parts);
        let rt = Arc::new(Mutex::new(rt));
        let (stop, stop_rx) = watch::channel(false);
        let st = AppState { rt: rt.clone(), snap, stale_after: Duration::from_secs(10), stop: stop_rx, history: None };
        (router(st), rt, stop)
    }

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, String) {
        let r = app.clone().oneshot(req).await.unwrap();
        let code = r.status();
        let body = r.into_body().collect().await.unwrap().to_bytes();
        (code, String::from_utf8(body.to_vec()).unwrap())
    }

    fn get(uri: &str) -> Request<Body> {
        Request::get(uri).body(Body::empty()).unwrap()
    }

    fn post(uri: &str, body: &str) -> Request<Body> {
        Request::post(uri).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap()
    }

    /// The page is Dutch, carries the controls with current values, and embeds the state.
    #[tokio::test]
    async fn dashboard_page_renders() {
        let (app, rt) = app(Mode::Simulate, false);
        rt.lock().unwrap().step(t0());
        let (code, html) = call(&app, get("/")).await;
        assert_eq!(code, StatusCode::OK);
        assert!(html.contains("<html lang=\"nl\">"));
        assert!(html.contains("Kwartierbudget"));
        assert!(html.contains("<option value=\"[&quot;Buzz&quot;,&quot;Batterij&quot;,&quot;Ev3&quot;]\" selected>"));
        assert!(html.contains("name=\"peak_guard_actief\" checked>"));
        assert!(html.contains("name=\"goedkoop_laden_prijs_drempel\" step=\"0.01\" value=\"0.38\">"));
        assert!(html.contains("prefers-color-scheme: dark"));
        assert!(!html.contains("{{"));
        assert!(html.contains("\"afname_slots\":192"), "13:46: today and tomorrow");
    }

    fn basic(user_password: &str) -> header::HeaderValue {
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD.encode(user_password);
        header::HeaderValue::from_str(&format!("Basic {b}")).unwrap()
    }

    /// The right password in any user's name passes; everything else does not.
    #[test]
    fn basic_auth_checks_the_password() {
        assert!(basic_auth_ok(Some(&basic("anyone:s3cret")), "s3cret"));
        assert!(basic_auth_ok(Some(&basic(":with:colon")), "with:colon"));
        assert!(!basic_auth_ok(Some(&basic("anyone:wrong!")), "s3cret"));
        assert!(!basic_auth_ok(Some(&basic("anyone:s3cret2")), "s3cret"));
        assert!(!basic_auth_ok(Some(&basic("anyone:")), "s3cret"));
        assert!(!basic_auth_ok(Some(&basic("no-colon")), "s3cret"));
        assert!(!basic_auth_ok(Some(&header::HeaderValue::from_static("Bearer s3cret")), "s3cret"));
        assert!(!basic_auth_ok(Some(&header::HeaderValue::from_static("Basic !!!")), "s3cret"));
        assert!(!basic_auth_ok(None, "s3cret"));
    }

    /// With a password every route answers 401 except the health check; without one it passes.
    #[tokio::test]
    async fn the_password_guards_all_but_the_health_check() {
        use tower::ServiceExt;
        let app = require_password(
            Router::new()
                .route("/api/state", axum::routing::get(|| async { "state" }))
                .route("/healthz", axum::routing::get(|| async { "ok" })),
            "s3cret".into(),
        );
        let get_with = |path: &str, auth: Option<header::HeaderValue>| {
            let mut b = axum::http::Request::builder().uri(path);
            if let Some(a) = auth {
                b = b.header(header::AUTHORIZATION, a);
            }
            b.body(axum::body::Body::empty()).unwrap()
        };
        let denied = app.clone().oneshot(get_with("/api/state", None)).await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert!(denied.headers().contains_key(header::WWW_AUTHENTICATE));
        let ok = app.clone().oneshot(get_with("/api/state", Some(basic("u:s3cret")))).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let health = app.oneshot(get_with("/healthz", None)).await.unwrap();
        assert_eq!(health.status(), StatusCode::OK);
    }

    /// In lockstep the controls are rendered disabled with a read-only note.
    #[test]
    fn lockstep_controls_render_read_only() {
        let s = serde_json::to_value(energie_core::settings::Settings::default()).unwrap();
        let html = render_controls(&s, false);
        assert!(html.contains("name=\"peak_guard_actief\" checked disabled>"));
        let snap = Snapshot { settings_editable: false, mode: "shadow", ..Default::default() };
        assert!(render_page(&snap).contains("Alleen-lezen"));
        let off = serde_json::json!({"peak_guard_actief": false});
        assert!(render_controls(&off, true).contains("name=\"peak_guard_actief\">"));
    }

    /// `/api/state` returns the snapshot; `/api/settings` the values and the control list.
    #[tokio::test]
    async fn state_and_settings_endpoints() {
        let (app, rt) = app(Mode::Control, false);
        rt.lock().unwrap().step(t0());
        let (code, body) = call(&app, get("/api/state")).await;
        assert_eq!(code, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["tick"], 1);
        assert_eq!(v["mode"], "control");
        assert_eq!(v["commands"][0]["command"]["EvseCurrentDeciA"], 80);
        let (_, body) = call(&app, get("/api/settings")).await;
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["editable"], true);
        assert_eq!(v["values"]["ev_net_niveau"], "MaxTotPiek");
        assert_eq!(v["fields"].as_array().unwrap().len(), 21);
    }

    /// A settings POST changes and returns the settings; bad input is a 400.
    #[tokio::test]
    async fn settings_post_applies_or_rejects() {
        let (app, rt) = app(Mode::Control, false);
        let (code, body) = call(&app, post("/api/settings", r#"{"ev_net_niveau":"Uit"}"#)).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(rt.lock().unwrap().engine.settings.ev_net_niveau, energie_core::settings::NetNiveau::Uit);
        let (code, body) = call(&app, post("/api/settings", r#"{"nope":1}"#)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(body.contains("nope"));
    }

    /// In shadow lockstep the settings are read-only: 409.
    #[tokio::test]
    async fn settings_post_in_lockstep_is_a_conflict() {
        let (app, _) = app(Mode::Shadow, true);
        let (code, _) = call(&app, post("/api/settings", r#"{"peak_guard_actief":false}"#)).await;
        assert_eq!(code, StatusCode::CONFLICT);
    }

    /// `/api/control` shows and switches the gate in control mode (a 404 and a 409 otherwise).
    #[tokio::test]
    async fn control_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (mut parts, rec) = sim_parts(Mode::Control, vec![]);
        parts.control = Some(crate::control::Control::new(Default::default(), dir.path()));
        let (rt, snap) = Runtime::new(parts);
        let rt = Arc::new(Mutex::new(rt));
        let (_stop, stop) = watch::channel(false);
        let app = router(AppState { rt: rt.clone(), snap, stale_after: Duration::from_secs(10), stop, history: None });
        let (code, body) = call(&app, get("/api/control")).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["enabled"], true);
        let (code, body) = call(&app, post("/api/control", r#"{"enabled":false,"device":"solis"}"#)).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["devices"]["solis"], false);
        assert_eq!(
            call(&app, post("/api/control", r#"{"enabled":false,"device":"enabled"}"#)).await.0,
            StatusCode::BAD_REQUEST
        );
        assert!(rec.0.lock().unwrap().is_empty());
        // Outside control mode there is nothing to switch.
        let (other, _) = super::tests::app(Mode::Simulate, false);
        assert_eq!(call(&other, get("/api/control")).await.0, StatusCode::NOT_FOUND);
        assert_eq!(call(&other, post("/api/control", r#"{"enabled":false}"#)).await.0, StatusCode::CONFLICT);
    }

    /// `/api/shadow` has the stats in shadow mode and is a 404 otherwise.
    #[tokio::test]
    async fn shadow_endpoint() {
        let (app, _) = app(Mode::Shadow, false);
        let (code, body) = call(&app, get("/api/shadow")).await;
        assert_eq!(code, StatusCode::OK);
        assert!(serde_json::from_str::<Value>(&body).unwrap()["fields"].is_array());
        let (other, _) = super::tests::app(Mode::Simulate, false);
        assert_eq!(call(&other, get("/api/shadow")).await.0, StatusCode::NOT_FOUND);
    }

    /// Health: starting before the first tick, ok while ticking, stale when ticks stop.
    #[tokio::test]
    async fn healthz_tracks_ticks() {
        let (app0, rt) = app(Mode::Simulate, false);
        let (code, body) = call(&app0, get("/healthz")).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("starting"));
        rt.lock().unwrap().step(t0());
        let (code, body) = call(&app0, get("/healthz")).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        // A zero staleness budget makes the same snapshot stale.
        let snap = rt.lock().unwrap().snapshot_rx();
        let (_keep, stop) = watch::channel(false);
        let stale = router(AppState { rt: rt.clone(), snap, stale_after: Duration::ZERO, stop, history: None });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let (code, body) = call(&stale, get("/healthz")).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("stale"));
    }

    /// The event stream sends the current state at once and again after each tick.
    #[tokio::test]
    async fn events_stream_state_per_tick() {
        let (app, rt, stop) = app_with_stop(Mode::Simulate, false);
        let r = app.clone().oneshot(get("/api/events")).await.unwrap();
        assert_eq!(r.headers()["content-type"], "text/event-stream");
        let mut body = r.into_body();
        let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(String::from_utf8_lossy(&first).starts_with("event: state\ndata: {"));
        rt.lock().unwrap().step(t0());
        let next = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(String::from_utf8_lossy(&next).contains("\"tick\":1"));
        // Shutdown ends the stream.
        stop.send(true).unwrap();
        assert!(body.frame().await.is_none());
    }

    /// A runtime that records into a history store, behind the router.
    fn app_with_history() -> (Router, Arc<Mutex<Runtime>>, SharedHistory) {
        use crate::history::{History, Retention, series};
        let ret = Retention { dir: None, minute_s: 14 * 86_400, quarter_s: 430 * 86_400, day_s: 1100 * 86_400 };
        let h: SharedHistory = Arc::new(Mutex::new(History::open(series::meta(), &ret, t0().timestamp()).unwrap()));
        let (mut parts, _) = sim_parts(Mode::Simulate, vec![]);
        parts.history = Some(h.clone());
        let (rt, snap) = Runtime::new(parts);
        let rt = Arc::new(Mutex::new(rt));
        let (stop, stop_rx) = watch::channel(false);
        std::mem::forget(stop);
        let st = AppState {
            rt: rt.clone(),
            snap,
            stale_after: Duration::from_secs(10),
            stop: stop_rx,
            history: Some(h.clone()),
        };
        (router(st), rt, h)
    }

    /// The stylesheet and scripts are compiled in and served with their types.
    #[tokio::test]
    async fn assets_are_served() {
        let (app, _) = app(Mode::Simulate, false);
        for (path, kind, needle) in [
            ("/assets/app.css", "text/css", ".chart"),
            ("/assets/app.js", "text/javascript", "/api/history"),
            ("/assets/charts.js", "text/javascript", "const Charts"),
        ] {
            let r = app.clone().oneshot(get(path)).await.unwrap();
            assert_eq!(r.status(), StatusCode::OK, "{path}");
            assert!(r.headers()["content-type"].to_str().unwrap().starts_with(kind));
            let body = r.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains(needle), "{path}");
        }
        let (_, html) = call(&app, get("/")).await;
        for asset in ["/assets/app.css", "/assets/app.js", "/assets/charts.js"] {
            assert!(html.contains(asset), "{asset}");
        }
    }

    /// The page has the tabs of the HA dashboard, the Modus & prioriteit group a second time on
    /// the EV tab, and the derived readouts under their controls.
    #[tokio::test]
    async fn page_has_the_views_and_grouped_controls() {
        let (app, rt) = app(Mode::Simulate, false);
        rt.lock().unwrap().step(t0());
        let (_, html) = call(&app, get("/")).await;
        for tab in ["Overzicht", "Capaciteitstarief", "Prijzen", "Batterij &amp; Solis", "EV laden", "Systeem"] {
            assert!(html.contains(&format!(">{tab}</a>")), "{tab}");
        }
        assert_eq!(html.matches("<legend>Modus &amp; prioriteit</legend>").count(), 2);
        assert_eq!(html.matches("<legend>Instelbare drempels</legend>").count(), 1);
        assert!(html.contains("<output data-d=\"capaciteit_maandtarget_kw\" data-unit=\"kW\" data-dec=\"3\">"));
    }

    /// `/api/history`: the series list, queries by id with an aggregate, and bad requests.
    #[tokio::test]
    async fn history_endpoint() {
        let (app, rt, h) = app_with_history();
        let (code, _) = call(&super::tests::app(Mode::Simulate, false).0, get("/api/history")).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "no store");
        for s in 0..180 {
            rt.lock().unwrap().step(t0() + chrono::Duration::seconds(s));
        }
        assert_eq!(h.lock().unwrap().stats.samples, 180);
        let (code, body) = call(&app, get("/api/history")).await;
        assert_eq!(code, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert!(v["series"].as_array().unwrap().iter().any(|s| s["id"] == "p1_w"));
        assert_eq!(v["tiers"][1]["name"], "minute");
        assert_eq!(v["stats"]["samples"], 180);

        let from = t0().timestamp();
        let uri = format!("/api/history?series=p1_w,soc_pct:last,&from={from}&step=60");
        let (code, body) = call(&app, get(&uri)).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["step"], 60);
        assert_eq!(v["to"], from + 179, "to defaults to the last tick");
        assert_eq!(v["series"][1]["agg"], "last");
        assert_eq!(v["series"][0]["v"].as_array().unwrap().len(), 3);
        assert!(v["series"][0]["v"][0].is_number());
        // RFC 3339 times; the default range is the day before `to`.
        let (code, body) = call(&app, get("/api/history?series=pv_w&to=2026-11-07T14:00:00%2B01:00&step=3600")).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["t"].as_array().unwrap().len(), 24);
        for bad in [
            "/api/history?series=p1_w:median",
            "/api/history?series=p1_w&from=gisteren",
            "/api/history?series=p1_w&to=morgen",
            "/api/history?series=bestaat_niet",
        ] {
            let (code, body) = call(&app, get(bad)).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{bad}: {body}");
        }
    }

    /// Before the first tick the query range ends at the wall clock.
    #[tokio::test]
    async fn history_before_the_first_tick_uses_the_wall_clock() {
        let (app, _, _) = app_with_history();
        let (code, body) = call(&app, get("/api/history?series=p1_w&step=3600")).await;
        assert_eq!(code, StatusCode::OK);
        let to = serde_json::from_str::<Value>(&body).unwrap()["to"].as_i64().unwrap();
        assert!((to - chrono::Utc::now().timestamp()).abs() < 60);
    }

    /// `/api/prices`: the offtake slots with their injection prices and the extremes.
    #[tokio::test]
    async fn prices_endpoint() {
        let (app, rt) = app(Mode::Simulate, false);
        rt.lock().unwrap().step(t0());
        let (code, body) = call(&app, get("/api/prices")).await;
        assert_eq!(code, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["afname"].as_array().unwrap().len(), 192);
        assert_eq!(v["injectie"].as_array().unwrap().len(), 192);
        assert!(v["laagste"]["price"].as_f64().unwrap() <= v["hoogste"]["price"].as_f64().unwrap());
    }

    /// The stream also ends when the runtime is gone.
    #[tokio::test]
    async fn events_stream_ends_with_the_runtime() {
        let (parts, _) = sim_parts(Mode::Simulate, vec![]);
        let (rt, snap) = Runtime::new(parts);
        let (_stop_tx, stop) = watch::channel(false);
        let rt = Arc::new(Mutex::new(rt));
        let app = router(AppState { rt: rt.clone(), snap, stale_after: Duration::from_secs(1), stop, history: None });
        let mut body = app.oneshot(get("/api/events")).await.unwrap().into_body();
        body.frame().await.unwrap().unwrap();
        drop(Arc::try_unwrap(rt).ok().unwrap());
        assert!(body.frame().await.is_none());
    }
    /// `/api/entities` lists every entity with its value; the World by role, the derived
    /// fields and the settings.
    #[tokio::test]
    async fn entities_endpoint_lists_the_registry() {
        let (app, rt) = app(Mode::Control, false);
        rt.lock().unwrap().step(t0());
        let (code, body) = call(&app, get("/api/entities")).await;
        assert_eq!(code, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        let all = v.as_array().unwrap();
        let get = |id: &str| all.iter().find(|e| e["id"] == id).unwrap_or_else(|| panic!("{id}")).clone();
        let p1 = get("world_grid_meter_power_w");
        assert_eq!((p1["group"].as_str(), p1["unit"].as_str()), (Some("world"), Some("W")));
        assert!(p1["value"].is_number());
        assert_eq!(get("world_charger_buzz_override_a")["control"]["type"], "number");
        assert_eq!(get("setting_ev_net_niveau")["value"], "Max tot piek");
        assert!(get("derived_pv_vermogen_totaal_w")["value"].is_number());
    }

    /// Setting entities: a setting is applied, a World field becomes its command on the
    /// device's actuator; unknown, read-only and bad values are refused.
    #[tokio::test]
    async fn entities_can_be_set() {
        let (parts, rec) = sim_parts(Mode::Control, vec![]);
        let (rt, snap) = Runtime::new(parts);
        let rt = Arc::new(Mutex::new(rt));
        let (_stop, stop) = watch::channel(false);
        let app = router(AppState { rt: rt.clone(), snap, stale_after: Duration::from_secs(10), stop, history: None });
        rt.lock().unwrap().step(t0());
        let (code, body) = call(&app, post("/api/entities/setting_ev_net_niveau", r#"{"value":"Uit"}"#)).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["value"], "Uit");
        assert_eq!(rt.lock().unwrap().engine.settings.ev_net_niveau, energie_core::settings::NetNiveau::Uit);
        let (code, body) = call(&app, post("/api/entities/world_charger_buzz_override_a", r#"{"value":10}"#)).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(*rec.0.lock().unwrap(), vec![energie_core::command::Command::EvseCurrentDeciA(100)]);
        assert_eq!(
            rt.lock().unwrap().snapshot_rx().borrow().commands[0].command,
            energie_core::command::Command::EvseCurrentDeciA(100)
        );
        let (code, body) = call(&app, post("/api/entities/world_bestaat_niet", r#"{"value":1}"#)).await;
        assert_eq!((code, body.as_str()), (StatusCode::NOT_FOUND, "onbekende entiteit: world_bestaat_niet"));
        let (code, _) = call(&app, post("/api/entities/world_grid_meter_power_w", r#"{"value":1}"#)).await;
        assert_eq!(code, StatusCode::CONFLICT);
        let (code, body) = call(&app, post("/api/entities/world_charger_ev3_set_current_a", r#"{"value":40}"#)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(body.contains("does not fit"), "{body}");
        let (code, _) = call(&app, post("/api/entities/world_charger_ev3_set_current_a", r#"{"waarde":10}"#)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }

    /// A car whose driver makes its AC charging limit settable; records what it is asked.
    #[derive(Default)]
    struct Car(Mutex<Vec<(String, energie_drivers::entity::Value)>>);

    impl crate::plug::DeviceView for Car {
        fn id(&self) -> &str {
            "ev3"
        }
        fn kind(&self) -> &str {
            "kia_connect"
        }
        fn slots(&self) -> &[energie_drivers::plugin::Slot] {
            &[energie_drivers::plugin::Slot::CarEv3]
        }
        fn world(&self) -> energie_core::world::World {
            energie_core::world::World::default()
        }
        fn extra(&self) -> Vec<energie_drivers::entity::Entity> {
            let mut e = energie_drivers::entity::Entity::sensor("ac_limit", "AC-laadlimiet", Some(100.0).into());
            e.control =
                Some(energie_drivers::entity::Control::Number { min: Some(50.0), max: Some(100.0), step: 10.0 });
            vec![e]
        }
        fn health(&self) -> crate::plug::DriverHealth {
            crate::plug::DriverHealth::default()
        }
        fn settable(&self) -> bool {
            true
        }
        fn set(&self, key: &str, value: &energie_drivers::entity::Value) -> crate::error::Result<()> {
            self.0.lock().unwrap().push((key.to_string(), value.clone()));
            Ok(())
        }
    }

    /// A device's own settable entity goes to that device, not through a role; values its
    /// control does not take are refused before.
    #[tokio::test]
    async fn device_entities_go_to_their_device() {
        let (mut parts, rec) = sim_parts(Mode::Control, vec![]);
        let car = Arc::new(Car::default());
        assert_eq!(crate::plug::DeviceView::kind(car.as_ref()), "kia_connect");
        parts.devices.push(car.clone());
        let (rt, snap) = Runtime::new(parts);
        let rt = Arc::new(Mutex::new(rt));
        let (_stop, stop) = watch::channel(false);
        let app = router(AppState { rt: rt.clone(), snap, stale_after: Duration::from_secs(10), stop, history: None });
        rt.lock().unwrap().step(t0());
        let (code, body) = call(&app, post("/api/entities/ev3_ac_limit", r#"{"value":80}"#)).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert_eq!(
            *car.0.lock().unwrap(),
            vec![("ac_limit".to_string(), energie_drivers::entity::Value::Number(80.0))]
        );
        assert!(rec.0.lock().unwrap().is_empty(), "no controller command");
        let (code, _) = call(&app, post("/api/entities/ev3_ac_limit", r#"{"value":45}"#)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let (code, _) = call(&app, post("/api/entities/ev3_soc", r#"{"value":45}"#)).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(car.0.lock().unwrap().len(), 1);
    }

    /// A device whose credentials answer never comes.
    struct Gone;

    impl crate::plug::DeviceView for Gone {
        fn id(&self) -> &str {
            "gone"
        }
        fn kind(&self) -> &str {
            "cupra_data_act"
        }
        fn slots(&self) -> &[energie_drivers::plugin::Slot] {
            &[]
        }
        fn world(&self) -> energie_core::world::World {
            energie_core::world::World::default()
        }
        fn extra(&self) -> Vec<energie_drivers::entity::Entity> {
            Vec::new()
        }
        fn health(&self) -> crate::plug::DriverHealth {
            crate::plug::DriverHealth::default()
        }
        fn set_credentials(
            &self,
            _: serde_json::Map<String, Value>,
        ) -> crate::error::Result<crate::drivers::CredentialsReply> {
            Ok(tokio::sync::oneshot::channel().1)
        }
    }

    fn from(req: Request<Body>, ip: [u8; 4]) -> Request<Body> {
        let mut req = req;
        let addr = std::net::SocketAddr::from((ip, 40000));
        req.extensions_mut().insert(axum::extract::ConnectInfo(addr));
        req
    }

    /// New credentials reach a real Kia driver, in shadow mode too, but only from loopback:
    /// stored in the state dir, then a login (here against a service that knows nothing). The
    /// sign-in state is in `/api/cloud`, the device's health and its `auth` entity.
    #[tokio::test]
    async fn credentials_reach_the_cloud_driver_from_loopback_only() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, Router::new()).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join("cloud-kia.json");
        std::fs::write(&creds, r#"{"username":"u","password":"p","region":1,"brand":1}"#).unwrap();
        let text = format!(
            "mode = \"shadow\"\nstate_dir = {:?}\n[ha]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"/nonexistent/t\"\n[[device]]\nid = \"ev3\"\nkind = \"kia_connect\"\nrole = \"car:ev3\"\n\
             credentials_file = {creds:?}\nstate_dir = {:?}\npoll_ms = 60000\n\
             endpoints = {{ api_base = \"{base}\", login_host = \"{base}\", cci_api = \"{base}\" }}\n",
            dir.path(),
            dir.path()
        );
        let cfg = crate::config::Config::parse(&text).unwrap();
        let drivers = crate::drivers::Drivers::new(&cfg).unwrap();
        let (mut parts, _) = sim_parts(Mode::Shadow, vec![]);
        parts.devices = crate::plug::DriverFactory::devices(&drivers);
        parts.devices.push(Arc::new(Car::default()));
        parts.devices.push(Arc::new(Gone));
        let (rt, snap) = Runtime::new(parts);
        let rt = Arc::new(Mutex::new(rt));
        let (_stop, stop) = watch::channel(false);
        let app = router(AppState { rt: rt.clone(), snap, stale_after: Duration::from_secs(10), stop, history: None });

        let (code, body) = call(&app, get("/api/cloud")).await;
        assert_eq!(code, StatusCode::OK);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (v["ev3"]["kind"].as_str(), v["ev3"]["auth"]["credentials"].as_str()),
            (Some("kia_connect"), Some("etc"))
        );
        assert!(v.get("gone").is_none(), "no sign-in state, not listed");
        let gone = (crate::plug::DeviceView::kind(&Gone), crate::plug::DeviceView::slots(&Gone).len());
        assert_eq!(gone, ("cupra_data_act", 0));

        let good = r#"{"username":"new@example.invalid","password":"hunter2"}"#;
        let url = "/api/cloud/ev3/credentials";
        for req in [post(url, good), from(post(url, good), [192, 168, 1, 5])] {
            let (code, body) = call(&app, req).await;
            assert_eq!(code, StatusCode::FORBIDDEN, "{body}");
        }
        let lo = [127, 0, 0, 1];
        assert_eq!(call(&app, from(post(url, "[1]"), lo)).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(call(&app, from(post("/api/cloud/nope/credentials", good), lo)).await.0, StatusCode::NOT_FOUND);
        let (code, body) = call(&app, from(post("/api/cloud/ev3_car/credentials", good), lo)).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{body}");
        let (code, body) = call(&app, from(post(url, r#"{"username":"u","password":"hunter2","x":"1"}"#), lo)).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(body.contains("\"x\"") && !body.contains("hunter2"), "{body}");
        let (code, body) = call(&app, from(post("/api/cloud/gone/credentials", good), lo)).await;
        assert_eq!(code, StatusCode::GATEWAY_TIMEOUT, "{body}");

        let (code, body) = call(&app, from(post(url, good), lo)).await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert!(!body.contains("hunter2"));
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((v["device"].as_str(), v["auth"]["credentials"].as_str()), (Some("ev3"), Some("state")));
        assert!(v["auth"]["last_result"].as_str().unwrap().contains("no RSA key"), "{v}");
        let stored = std::fs::read_to_string(dir.path().join("credentials/ev3.json")).unwrap();
        assert!(stored.contains("new@example.invalid"));

        let ev3 = rt.lock().unwrap().devices.iter().find(|d| d.id() == "ev3").unwrap().clone();
        assert_eq!(ev3.health().auth.unwrap().credentials, "state");
        let auth = ev3.extra().into_iter().find(|e| e.key == "auth").unwrap();
        assert_eq!(auth.value, energie_drivers::entity::Value::Text("wacht".into()));
    }

    /// A device that takes no credentials says so.
    #[tokio::test]
    async fn credentials_for_a_device_without_a_login() {
        let (mut parts, _) = sim_parts(Mode::Shadow, vec![]);
        parts.devices.push(Arc::new(Car::default()));
        let (rt, snap) = Runtime::new(parts);
        let (_stop, stop) = watch::channel(false);
        let app = router(AppState {
            rt: Arc::new(Mutex::new(rt)),
            snap,
            stale_after: Duration::from_secs(10),
            stop,
            history: None,
        });
        let (code, body) =
            call(&app, from(post("/api/cloud/ev3/credentials", r#"{"username":"u","password":"p"}"#), [127, 0, 0, 1]))
                .await;
        assert_eq!((code, body.as_str()), (StatusCode::BAD_REQUEST, "ev3: takes no credentials"));
    }

    /// In shadow mode nothing on a device is settable; in lockstep the settings are not either.
    #[tokio::test]
    async fn shadow_entities_are_read_only() {
        let (app, rt) = app(Mode::Shadow, true);
        rt.lock().unwrap().step(t0());
        let (code, _) = call(&app, post("/api/entities/world_charger_buzz_override_a", r#"{"value":10}"#)).await;
        assert_eq!(code, StatusCode::CONFLICT);
        let (code, _) = call(&app, post("/api/entities/setting_peak_guard_actief", r#"{"value":false}"#)).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(rt.lock().unwrap().engine.settings.peak_guard_actief);
    }

    /// The event stream carries the entity values: all of them first, then only the changes.
    #[tokio::test]
    async fn events_carry_entity_changes() {
        let (app, rt, _stop) = app_with_stop(Mode::Simulate, false);
        let mut body = app.clone().oneshot(get("/api/events")).await.unwrap().into_body();
        let data = |b: &[u8]| -> Value {
            let t = String::from_utf8_lossy(b).to_string();
            serde_json::from_str(t.lines().find_map(|l| l.strip_prefix("data: ")).unwrap()).unwrap()
        };
        let first = data(&body.frame().await.unwrap().unwrap().into_data().unwrap());
        let all = first["entities"].as_object().unwrap().len();
        assert!(all > 150, "{all}");
        rt.lock().unwrap().step(t0());
        let next = data(&body.frame().await.unwrap().unwrap().into_data().unwrap());
        let changed = next["entities"].as_object().unwrap();
        assert!(!changed.is_empty() && changed.len() < all, "{}", changed.len());
        assert!(changed.contains_key("world_grid_meter_power_w"));
    }
}
