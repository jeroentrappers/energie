//! Home Assistant WebSocket API client.
//!
//! One session: `auth_required` → `auth` (long-lived token) → `auth_ok`, then
//! `subscribe_events(state_changed)` and `get_states`. The snapshot replaces the store, events
//! update it. Service calls queued on the channel are sent as `call_service` (control mode
//! only; the shadow runtime never queues any). A ping every `ping_every` keeps NAT and HA's
//! idle timeout happy. Any error ends the session; [`run`] reconnects with exponential backoff,
//! reset after a successful authentication.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use energie_core::time::{TZ, Ts};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use super::entity::{EntityState, HaStore, parse_time};
use crate::error::{Error, Result};

pub type SharedStore = Arc<Mutex<HaStore>>;

/// A `call_service` request.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceCall {
    pub domain: String,
    pub service: String,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub url: String,
    pub token: String,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub ping_every: Duration,
}

impl ClientConfig {
    pub fn new(url: String, token: String) -> Self {
        ClientConfig {
            url,
            token,
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            ping_every: Duration::from_secs(30),
        }
    }
}

/// How a session ended.
#[derive(Debug, PartialEq)]
struct Ended {
    authed: bool,
    error: Error,
}

const SUB_ID: u64 = 1;
const STATES_ID: u64 = 2;

/// Run sessions until `shutdown` turns true.
pub async fn run(
    cfg: ClientConfig,
    store: SharedStore,
    mut calls: mpsc::UnboundedReceiver<ServiceCall>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut backoff = cfg.backoff_min;
    loop {
        let ended = tokio::select! {
            e = session(&cfg, &store, &mut calls) => e,
            _ = shutdown.wait_for(|s| *s) => return,
        };
        store.lock().expect("store lock").connected = false;
        if ended.authed {
            backoff = cfg.backoff_min;
        }
        tracing::warn!(error = %ended.error, retry_in = ?backoff, "home assistant connection lost");
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.wait_for(|s| *s) => return,
        }
        backoff = (backoff * 2).min(cfg.backoff_max);
    }
}

pub(crate) type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

pub(crate) async fn next_json(ws: &mut Ws) -> Result<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(t))) => return Ok(serde_json::from_str(&t)?),
            Some(Ok(Message::Close(_))) | None => return Err(Error::new("connection closed")),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

pub(crate) async fn send(ws: &mut Ws, v: Value) -> Result<()> {
    ws.send(Message::text(v.to_string())).await?;
    Ok(())
}

async fn session(cfg: &ClientConfig, store: &SharedStore, calls: &mut mpsc::UnboundedReceiver<ServiceCall>) -> Ended {
    let mut ws = match authenticate(cfg).await {
        Ok(ws) => ws,
        Err(error) => return Ended { authed: false, error },
    };
    tracing::info!(url = %cfg.url, "home assistant authenticated");
    let Err(error) = serve(&mut ws, cfg, store, calls).await;
    Ended { authed: true, error }
}

pub(crate) async fn authenticate(cfg: &ClientConfig) -> Result<Ws> {
    let (mut ws, _) = tokio_tungstenite::connect_async(&cfg.url).await?;
    let hello = next_json(&mut ws).await?;
    if hello["type"] != "auth_required" {
        return Err(Error::new(format!("unexpected greeting: {hello}")));
    }
    send(&mut ws, json!({"type": "auth", "access_token": cfg.token})).await?;
    let reply = next_json(&mut ws).await?;
    match reply["type"].as_str() {
        Some("auth_ok") => Ok(ws),
        _ => Err(Error::new(format!("authentication failed: {}", reply["message"].as_str().unwrap_or("?")))),
    }
}

/// The authenticated part of a session. Only returns on error.
async fn serve(
    ws: &mut Ws,
    cfg: &ClientConfig,
    store: &SharedStore,
    calls: &mut mpsc::UnboundedReceiver<ServiceCall>,
) -> Result<std::convert::Infallible> {
    send(ws, json!({"id": SUB_ID, "type": "subscribe_events", "event_type": "state_changed"})).await?;
    send(ws, json!({"id": STATES_ID, "type": "get_states"})).await?;
    let mut next_id = STATES_ID + 1;
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + cfg.ping_every, cfg.ping_every);
    let mut calls_open = true;
    loop {
        tokio::select! {
            msg = next_json(ws) => handle(store, msg?)?,
            call = calls.recv(), if calls_open => match call {
                Some(c) => {
                    send(ws, json!({"id": next_id, "type": "call_service", "domain": c.domain, "service": c.service, "service_data": c.data})).await?;
                    next_id += 1;
                }
                None => calls_open = false,
            },
            _ = ping.tick() => {
                send(ws, json!({"id": next_id, "type": "ping"})).await?;
                next_id += 1;
            }
        }
    }
}

/// One read-only `get_states` in a session of its own: every entity's state, nothing
/// subscribed and nothing sent besides the request (the settings import).
pub async fn fetch_states(cfg: &ClientConfig) -> Result<Vec<EntityState>> {
    let mut ws = authenticate(cfg).await?;
    send(&mut ws, json!({"id": 1, "type": "get_states"})).await?;
    loop {
        let m = next_json(&mut ws).await?;
        if m["type"] == "result" && m["id"] == 1 {
            if m["success"] != true {
                return Err(Error::new(format!("get_states failed: {}", m["error"])));
            }
            return Ok(serde_json::from_value(m["result"].clone())?);
        }
    }
}

fn now() -> Ts {
    chrono::Utc::now().with_timezone(&TZ)
}

fn handle(store: &SharedStore, msg: Value) -> Result<()> {
    match msg["type"].as_str() {
        Some("result") if msg["success"] != true => {
            if msg["id"] == SUB_ID || msg["id"] == STATES_ID {
                return Err(Error::new(format!("request {} failed: {}", msg["id"], msg["error"])));
            }
            tracing::warn!(id = %msg["id"], error = %msg["error"], "service call failed");
        }
        Some("result") if msg["id"] == STATES_ID => {
            let states: Vec<EntityState> = serde_json::from_value(msg["result"].clone())?;
            tracing::info!(entities = states.len(), "home assistant snapshot");
            store.lock().expect("store lock").replace_all(states);
        }
        Some("event") => {
            let data = &msg["event"]["data"];
            let id = data["entity_id"].as_str().unwrap_or_default();
            let new: Option<EntityState> = serde_json::from_value(data["new_state"].clone()).ok();
            let at = msg["event"]["time_fired"].as_str().and_then(parse_time).unwrap_or_else(now);
            store.lock().expect("store lock").apply_change(at, id, new);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ha::entity::Lookup;
    use tokio::net::TcpListener;
    use tokio_tungstenite::WebSocketStream;

    pub(crate) type ServerWs = WebSocketStream<tokio::net::TcpStream>;

    /// The next text message as JSON; `None` once the client is gone.
    pub(crate) async fn recv(ws: &mut ServerWs) -> Option<Value> {
        while let Some(Ok(m)) = ws.next().await {
            if let Message::Text(t) = m {
                return serde_json::from_str(&t).ok();
            }
        }
        None
    }

    pub(crate) async fn push(ws: &mut ServerWs, v: Value) {
        let _ = ws.send(Message::text(v.to_string())).await;
    }

    pub(crate) type After = Arc<dyn Fn(usize, ServerWs) -> futures_util::future::BoxFuture<'static, ()> + Send + Sync>;

    /// A scripted fake HA: authenticates `good-token`, answers subscribe and get_states, then
    /// hands the connection to `after` (connection number, socket).
    pub(crate) async fn fake_ha<F, Fut>(states: Value, after: F) -> (String, Arc<std::sync::atomic::AtomicUsize>)
    where
        F: Fn(usize, ServerWs) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        fake_ha_dyn(states, Arc::new(move |n, ws| Box::pin(after(n, ws)))).await
    }

    async fn fake_ha_dyn(states: Value, after: After) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = count.clone();
        tokio::spawn(async move {
            let mut n = 0;
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                n += 1;
                counter.store(n, std::sync::atomic::Ordering::SeqCst);
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                push(&mut ws, json!({"type": "auth_required", "ha_version": "2026.11.0"})).await;
                let auth = recv(&mut ws).await.unwrap();
                if auth["access_token"] != "good-token" {
                    push(&mut ws, json!({"type": "auth_invalid", "message": "Invalid access token"})).await;
                    continue;
                }
                push(&mut ws, json!({"type": "auth_ok", "ha_version": "2026.11.0"})).await;
                let mut gs = recv(&mut ws).await.unwrap();
                if gs["type"] == "subscribe_events" {
                    assert_eq!(gs["event_type"], "state_changed");
                    push(&mut ws, json!({"id": gs["id"], "type": "result", "success": true, "result": null})).await;
                    gs = recv(&mut ws).await.unwrap();
                }
                assert_eq!(gs["type"], "get_states");
                push(&mut ws, json!({"id": gs["id"], "type": "result", "success": true, "result": states.clone()}))
                    .await;
                let a = after.clone();
                tokio::spawn(async move { a(n, ws).await });
            }
        });
        (url, count)
    }

    pub(crate) fn fast(url: String, token: &str) -> ClientConfig {
        ClientConfig {
            url,
            token: token.into(),
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(80),
            ping_every: Duration::from_secs(3600),
        }
    }

    /// Wait (at most 5 s) until `f` holds.
    pub(crate) async fn until(f: impl FnMut() -> bool) {
        until_dyn(Box::new(f)).await
    }

    async fn until_dyn(mut f: Box<dyn FnMut() -> bool + '_>) {
        let wait = async {
            while !f() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait).await.expect("condition not reached");
    }

    fn states() -> Value {
        json!([
            {"entity_id": "sensor.p1_meter_vermogen", "state": "3066.0", "attributes": {"unit_of_measurement": "W"}},
            {"entity_id": "select.smartevse_1234_mode", "state": "Normal", "attributes": {}}
        ])
    }

    type Client = (SharedStore, mpsc::UnboundedSender<ServiceCall>, watch::Sender<bool>, tokio::task::JoinHandle<()>);

    fn spawn_client(cfg: ClientConfig) -> Client {
        let store: SharedStore = Arc::new(Mutex::new(HaStore::watching(["select.smartevse_1234_mode".to_string()])));
        let (tx, rx) = mpsc::unbounded_channel();
        let (stop, stop_rx) = watch::channel(false);
        let h = tokio::spawn(run(cfg, store.clone(), rx, stop_rx));
        (store, tx, stop, h)
    }

    /// With a valid token the client mirrors get_states and then follows state_changed events.
    #[tokio::test]
    async fn auth_ok_then_snapshot_then_events() {
        let (done_tx, mut done) = mpsc::unbounded_channel::<()>();
        let (url, _h) = fake_ha(states(), move |_, mut ws| {
            let done_tx = done_tx.clone();
            async move {
                push(
                    &mut ws,
                    json!({"id": 1, "type": "event", "event": {"event_type": "state_changed",
                "time_fired": "2026-11-07T12:46:12.66+00:00",
                "data": {"entity_id": "select.smartevse_1234_mode",
                         "old_state": {"entity_id": "select.smartevse_1234_mode", "state": "Normal"},
                         "new_state": {"entity_id": "select.smartevse_1234_mode", "state": "Off", "attributes": {}}}}}),
                )
                .await;
                push(
                    &mut ws,
                    json!({"id": 1, "type": "event", "event": {"event_type": "state_changed",
                "data": {"entity_id": "sensor.p1_meter_vermogen", "new_state": null}}}),
                )
                .await;
                push(&mut ws, json!({"id": 99, "type": "pong"})).await;
                ws.send(Message::Binary(vec![0u8].into())).await.unwrap();
                ws.send(Message::Ping(vec![1u8].into())).await.unwrap();
                while recv(&mut ws).await.is_some() {}
                done_tx.send(()).unwrap();
            }
        })
        .await;
        let (store, _tx, stop, h) = spawn_client(fast(url, "good-token"));
        until(|| store.lock().unwrap().events >= 2).await;
        {
            let s = store.lock().unwrap();
            assert!(s.connected);
            assert_eq!(s.snapshots, 1);
            assert_eq!(s.get("select.smartevse_1234_mode").unwrap().state, "Off");
            assert!(s.get("sensor.p1_meter_vermogen").is_none(), "removed by a null new_state");
            assert_eq!(s.changes[0].at.to_rfc3339(), "2026-11-07T13:46:12.660+01:00");
        }
        stop.send(true).unwrap();
        h.await.unwrap();
        done.recv().await.unwrap();
    }

    /// A bad token is refused; that client stays disconnected and keeps retrying with backoff,
    /// while a client with the right token on the same HA connects.
    #[tokio::test]
    async fn auth_invalid_keeps_retrying_without_connecting() {
        let (url, attempts) = fake_ha(states(), |_, ws| async move { drop(ws) }).await;
        let (store, _tx, stop, h) = spawn_client(fast(url.clone(), "bad-token"));
        until(|| attempts.load(std::sync::atomic::Ordering::SeqCst) >= 3).await;
        let (good, _gtx, gstop, gh) = spawn_client(fast(url, "good-token"));
        until(|| good.lock().unwrap().snapshots >= 1).await;
        gstop.send(true).unwrap();
        gh.await.unwrap();
        assert!(!store.lock().unwrap().connected);
        assert_eq!(store.lock().unwrap().snapshots, 0);
        stop.send(true).unwrap();
        h.await.unwrap();
    }

    /// Stopping during the reconnect backoff ends the client at once.
    #[tokio::test]
    async fn stop_during_backoff_returns_promptly() {
        let mut cfg = fast("ws://127.0.0.1:1/api/websocket".into(), "t");
        cfg.backoff_min = Duration::from_secs(30);
        let (_store, _tx, stop, h) = spawn_client(cfg);
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), h).await.unwrap().unwrap();
    }

    /// When HA drops the connection the client reconnects and takes a fresh snapshot.
    #[tokio::test]
    async fn reconnects_after_the_server_closes() {
        let (url, _h) = fake_ha(states(), |n, mut ws| async move {
            match n {
                // A clean close.
                1 => ws.close(None).await.unwrap(),
                // A TCP reset without a close handshake.
                2 => drop(ws),
                _ => while recv(&mut ws).await.is_some() {},
            }
        })
        .await;
        let (store, _tx, stop, h) = spawn_client(fast(url, "good-token"));
        until(|| store.lock().unwrap().snapshots >= 3).await;
        until(|| store.lock().unwrap().connected).await;
        stop.send(true).unwrap();
        h.await.unwrap();
    }

    /// Queued service calls go out as call_service; a failed call is only logged; pings are
    /// sent on schedule; a closed call channel does not end the session.
    #[tokio::test]
    async fn service_calls_and_pings_are_sent() {
        let (seen_tx, mut seen_rx) = mpsc::unbounded_channel::<Value>();
        let (url, _h) = fake_ha(states(), move |_, mut ws| {
            let seen = seen_tx.clone();
            async move {
                while let Some(m) = recv(&mut ws).await {
                    if m["type"] == "call_service" {
                        push(
                            &mut ws,
                            json!({"id": m["id"], "type": "result", "success": false, "error": {"code": "x"}}),
                        )
                        .await;
                    }
                    let _ = seen.send(m);
                }
                let _ = seen.send(json!({"type": "gone"}));
            }
        })
        .await;
        let mut cfg = fast(url, "good-token");
        cfg.ping_every = Duration::from_millis(50);
        let (store, tx, stop, h) = spawn_client(cfg);
        until(|| store.lock().unwrap().connected).await;
        tx.send(ServiceCall {
            domain: "select".into(),
            service: "select_option".into(),
            data: json!({"entity_id": "select.smartevse_1234_mode", "option": "Normal"}),
        })
        .unwrap();
        drop(tx);
        let mut got_call = false;
        let mut got_ping = false;
        while !(got_call && got_ping) {
            let m = tokio::time::timeout(Duration::from_secs(2), seen_rx.recv()).await.unwrap().unwrap();
            if m["type"] == "call_service" {
                assert_eq!(m["domain"], "select");
                assert_eq!(m["service_data"]["option"], "Normal");
                got_call = true;
            }
            got_ping |= m["type"] == "ping";
        }
        assert!(store.lock().unwrap().connected);
        stop.send(true).unwrap();
        h.await.unwrap();
        while seen_rx.recv().await.unwrap()["type"] != "gone" {}
    }

    /// A failing subscribe, a bad greeting or an unreachable server end the session.
    #[tokio::test]
    async fn protocol_errors_end_the_session() {
        // Subscribe refused.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            push(&mut ws, json!({"type": "auth_required"})).await;
            recv(&mut ws).await;
            push(&mut ws, json!({"type": "auth_ok"})).await;
            recv(&mut ws).await;
            push(&mut ws, json!({"id": 1, "type": "result", "success": false, "error": {"code": "unauthorized"}}))
                .await;
            ws.send(Message::Binary(vec![1u8].into())).await.unwrap();
            while recv(&mut ws).await.is_some() {}
        });
        let store: SharedStore = Arc::new(Mutex::new(HaStore::default()));
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let e = session(&fast(url, "good-token"), &store, &mut rx).await;
        assert!(e.authed);
        assert!(e.error.0.contains("unauthorized"), "{}", e.error);

        // A server that does not speak HA.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            push(&mut ws, json!({"type": "hello"})).await;
        });
        let e = session(&fast(url, "good-token"), &store, &mut rx).await;
        assert!(!e.authed);
        assert!(e.error.0.contains("greeting"));

        // Nobody listening.
        let e = session(&fast("ws://127.0.0.1:1/api/websocket".into(), "t"), &store, &mut rx).await;
        assert!(!e.authed);

        // A server that hangs up mid-handshake.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/websocket", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = ws.close(None).await;
        });
        let e = session(&fast(url, "t"), &store, &mut rx).await;
        assert!(e.error.0.contains("closed"), "{}", e.error);
    }

    /// Unknown message types and results for other ids are ignored.
    #[test]
    fn unrelated_messages_are_ignored() {
        let store: SharedStore = Arc::new(Mutex::new(HaStore::default()));
        handle(&store, json!({"type": "result", "id": 7, "success": true})).unwrap();
        handle(&store, json!({"type": "something"})).unwrap();
        assert!(handle(&store, json!({"type": "result", "id": 2, "success": true, "result": 5})).is_err());
        assert_eq!(store.lock().unwrap().snapshots, 0);
        assert!(ClientConfig::new("u".into(), "t".into()).ping_every > Duration::from_secs(1));
    }
}
