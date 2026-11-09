//! The bridge against a broker (`rumqttc`): `<prefix>/status` `online` with an `offline`
//! last will, a full republish on every connect and when HA comes online, the snapshot
//! stream throttled by the [`Publisher`], `/set` messages to the runtime, reconnects with
//! exponential backoff, and `offline` at a clean stop.

use std::sync::Arc;
use std::time::Duration;

use energie_drivers::entity::Value;
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use tokio::sync::watch;
use tokio::time::Instant;

use super::{Msg, Publisher, parse_set};
use crate::entities::SetError;
use crate::runtime::Snapshot;

/// Sets an entity the way `POST /api/entities/<id>` does.
pub type Setter = Arc<dyn Fn(&str, &Value) -> Result<(), SetError> + Send + Sync>;

/// The broker connection.
#[derive(Debug, Clone)]
pub struct Link {
    pub host: String,
    pub port: u16,
    pub client_id: String,
    pub credentials: Option<(String, String)>,
    pub keep_alive: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
}

/// Queue `msgs` (never blocks: the event loop runs in the same task); a full queue drops the
/// message and says so.
fn send(client: &AsyncClient, msgs: Vec<Msg>) {
    for m in msgs {
        if let Err(e) = client.try_publish(m.topic.as_str(), QoS::AtLeastOnce, m.retain, m.payload) {
            tracing::warn!(topic = %m.topic, error = %e, "mqtt bridge: message dropped");
        }
    }
}

/// What a `/set` did, for the log.
fn apply(p: &Publisher, snap: &Snapshot, set: &Setter, topic: &str, payload: &str) -> Result<String, String> {
    let (id, v) = parse_set(&p.cfg, &snap.entities, topic, payload)?;
    match set(&id, &v) {
        Ok(()) => Ok(id),
        Err(SetError::Unknown) => Err(format!("{id}: unknown")),
        Err(SetError::NotSettable) => Err(format!("{id}: not settable now")),
        Err(SetError::Rejected(e)) => Err(e),
    }
}

/// Resolves once `stop` is true (or its sender is gone).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

/// Run until `stop`.
pub async fn run(
    link: Link,
    mut p: Publisher,
    mut snap: watch::Receiver<Arc<Snapshot>>,
    set: Setter,
    mut stop: watch::Receiver<bool>,
) {
    let mut opts = MqttOptions::new(link.client_id.clone(), link.host.clone(), link.port);
    opts.set_keep_alive(link.keep_alive);
    opts.set_last_will(LastWill::new(p.cfg.status_topic(), "offline", QoS::AtLeastOnce, true));
    if let Some((u, pw)) = &link.credentials {
        opts.set_credentials(u.clone(), pw.clone());
    }
    let (client, mut events) = AsyncClient::new(opts, 8192);
    let t0 = Instant::now();
    let ha_status = p.cfg.ha_status_topic();
    let mut connected = false;
    let mut backoff = link.backoff_min;
    let mut stop_wait = stop.clone();
    loop {
        tokio::select! {
            _ = stopped(&mut stop) => break,
            ev = events.poll() => match ev {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    tracing::info!(broker = %format!("{}:{}", link.host, link.port), "mqtt bridge connected");
                    connected = true;
                    backoff = link.backoff_min;
                    if p.cfg.allow_set {
                        let _ = client.try_subscribe(p.cfg.set_filter(), QoS::AtLeastOnce);
                    }
                    if let Some(t) = &ha_status {
                        let _ = client.try_subscribe(t.as_str(), QoS::AtLeastOnce);
                    }
                    p.reset();
                    send(&client, vec![p.online()]);
                    let s = snap.borrow().clone();
                    send(&client, p.tick(&s, t0.elapsed().as_secs_f64()));
                }
                Ok(Event::Incoming(Packet::Publish(m))) => {
                    let payload = String::from_utf8_lossy(&m.payload).into_owned();
                    let s = snap.borrow().clone();
                    if ha_status.as_deref() == Some(m.topic.as_str()) {
                        if payload == "online" {
                            tracing::info!("home assistant is online: discovery and states again");
                            p.reset();
                            send(&client, p.tick(&s, t0.elapsed().as_secs_f64()));
                        }
                    } else {
                        match apply(&p, &s, &set, &m.topic, &payload) {
                            Ok(id) => tracing::info!(entity = %id, value = %payload, "set over mqtt"),
                            Err(e) => tracing::warn!(topic = %m.topic, error = %e, "mqtt set refused"),
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    if connected {
                        tracing::warn!(error = %e, "mqtt bridge connection lost");
                    }
                    connected = false;
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = stopped(&mut stop_wait) => break,
                    }
                    backoff = (backoff * 2).min(link.backoff_max);
                }
            },
            r = snap.changed(), if connected => {
                if r.is_err() {
                    break;
                }
                let s = snap.borrow_and_update().clone();
                send(&client, p.tick(&s, t0.elapsed().as_secs_f64()));
            }
        }
    }
    if connected {
        send(&client, vec![p.offline()]);
        let _ = client.try_disconnect();
        // Let the event loop flush `offline` and the DISCONNECT.
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Ok(ev) = events.poll().await {
                if matches!(ev, Event::Outgoing(rumqttc::Outgoing::Disconnect)) {
                    break;
                }
            }
        })
        .await;
    }
    tracing::info!("mqtt bridge stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::BridgeConfig;
    use crate::bridge::tests::{cfg, snap};
    use energie_drivers::entity::{Control, Entity};
    use energie_drivers::sim::mqtt::Broker;
    use energie_drivers::sim::proxy::TcpProxy;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    type Seen = Arc<Mutex<BTreeMap<String, String>>>;

    /// A client id no other test uses.
    fn id(what: &str) -> String {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        format!("{what}-{}", N.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
    }

    /// A full queue drops the message instead of blocking the task.
    #[tokio::test]
    async fn a_full_queue_drops() {
        let (c, _events) = AsyncClient::new(MqttOptions::new("x", "127.0.0.1", 1), 1);
        let m = |t: &str| Msg { topic: t.into(), payload: "1".into(), retain: false };
        send(&c, vec![m("a"), m("b"), m("c")]);
    }

    /// A client on the test broker that keeps the last payload per topic (retained included).
    async fn observer(addr: std::net::SocketAddr, filter: &str) -> (AsyncClient, Seen) {
        let mut o = MqttOptions::new(id("obs"), addr.ip().to_string(), addr.port());
        o.set_keep_alive(Duration::from_secs(5));
        let (c, mut ev) = AsyncClient::new(o, 100);
        c.subscribe(filter, QoS::AtLeastOnce).await.unwrap();
        let seen: Seen = Arc::default();
        let s2 = seen.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(Event::Incoming(Packet::Publish(m))) = ev.poll().await {
                    s2.lock().unwrap().insert(m.topic.clone(), String::from_utf8_lossy(&m.payload).into_owned());
                }
            }
        });
        (c, seen)
    }

    fn link(addr: std::net::SocketAddr) -> Link {
        Link {
            host: addr.ip().to_string(),
            port: addr.port(),
            client_id: id("energie-test"),
            credentials: None,
            keep_alive: Duration::from_secs(5),
            backoff_min: Duration::from_millis(20),
            backoff_max: Duration::from_millis(80),
        }
    }

    fn entities(v: f64) -> Vec<Entity> {
        let mut s = Entity::sensor("batterij_reservering_w", "Reservering", Value::Number(v)).in_group("setting");
        s.control = Some(Control::Number { min: Some(0.0), max: None, step: 50.0 });
        vec![s, Entity::sensor("grid_meter_power_w", "Vermogen", Value::Number(3066.0)).in_group("world")]
    }

    async fn until(seen: &Seen, f: impl Fn(&BTreeMap<String, String>) -> bool) {
        crate::ha::ws::tests::until(|| f(&seen.lock().unwrap())).await;
    }

    /// Against an embedded broker: status online, every value retained with its attributes,
    /// the summary, the discovery; a `/set` reaches the setter (bad ones do not); a new
    /// snapshot is published; HA's `online` republishes; a cut connection brings the last
    /// will `offline`, the reconnect `online` again; a clean stop says `offline`.
    #[tokio::test]
    async fn publishes_sets_and_reconnects() {
        let broker = Broker::start(None).await;
        let proxy = TcpProxy::start(broker.addr).await;
        let (obs, seen) = observer(broker.addr, "#").await;
        let (tx, rx) = watch::channel(Arc::new(snap(entities(700.0))));
        let calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let c2 = calls.clone();
        let setter: Setter = Arc::new(move |id, v| {
            c2.lock().unwrap().push((id.to_string(), v.clone()));
            if *v == Value::Number(75.0) {
                Err(SetError::Rejected("75 is not on the 50 W grid".into()))
            } else {
                Ok(())
            }
        });
        let bc = BridgeConfig { discovery_prefix: Some("homeassistant".into()), ..cfg() };
        let (stop, stop_rx) = watch::channel(false);
        let h = tokio::spawn(run(link(proxy.addr), Publisher::new(bc), rx, setter, stop_rx));
        until(&seen, |s| s.get("energie/status").is_some_and(|v| v == "online")).await;
        until(&seen, |s| s.contains_key("energie/summary")).await;
        until(&seen, |s| s.get("energie/setting/batterij_reservering_w").is_some_and(|v| v == "700.0")).await;
        {
            let s = seen.lock().unwrap();
            assert!(s["energie/world/grid_meter_power_w/attributes"].contains("\"name\":\"Vermogen\""));
            assert!(
                s["homeassistant/number/energie/setting_batterij_reservering_w/config"]
                    .contains("number.batterij_reservering_w")
            );
        }
        for (topic, payload) in [
            ("energie/setting/batterij_reservering_w/set", "750"),
            ("energie/setting/batterij_reservering_w/set", "75"),
            ("energie/setting/batterij_reservering_w/set", "veel"),
            ("energie/world/grid_meter_power_w/set", "1"),
        ] {
            obs.publish(topic, QoS::AtLeastOnce, false, payload).await.unwrap();
        }
        crate::ha::ws::tests::until(|| calls.lock().unwrap().len() >= 2).await;
        assert_eq!(calls.lock().unwrap()[0], ("setting_batterij_reservering_w".into(), Value::Number(750.0)));
        tx.send(Arc::new(snap(entities(750.0)))).unwrap();
        until(&seen, |s| s.get("energie/setting/batterij_reservering_w").is_some_and(|v| v == "750.0")).await;
        // HA restarts: everything again (the observer sees the summary rewritten).
        seen.lock().unwrap().remove("energie/summary");
        obs.publish("homeassistant/status", QoS::AtLeastOnce, false, "offline").await.unwrap();
        obs.publish("homeassistant/status", QoS::AtLeastOnce, false, "online").await.unwrap();
        until(&seen, |s| s.contains_key("energie/summary")).await;
        // The link drops: the broker sends the will; the bridge reconnects.
        proxy.drop_all();
        until(&seen, |s| s.get("energie/status").is_some_and(|v| v == "offline")).await;
        until(&seen, |s| s.get("energie/status").is_some_and(|v| v == "online")).await;
        stop.send(true).unwrap();
        h.await.unwrap();
        until(&seen, |s| s.get("energie/status").is_some_and(|v| v == "offline")).await;
        assert_eq!(calls.lock().unwrap().len(), 2, "only the parseable set on a settable entity reached the setter");
    }

    /// Unknown and not-settable answers of the setter are refused like the parser's; a stop
    /// while the broker is away ends the task; a closed snapshot stream ends it too.
    #[tokio::test]
    async fn refusals_and_stops() {
        let p = Publisher::new(cfg());
        let s = snap(entities(700.0));
        let t = "energie/setting/batterij_reservering_w/set";
        let unknown: Setter = Arc::new(|_, _| Err(SetError::Unknown));
        assert!(apply(&p, &s, &unknown, t, "100").unwrap_err().contains("unknown"));
        let busy: Setter = Arc::new(|_, _| Err(SetError::NotSettable));
        assert!(apply(&p, &s, &busy, t, "100").unwrap_err().contains("not settable"));
        let ok: Setter = Arc::new(|_, _| Ok(()));
        let (_tx, rx) = watch::channel(Arc::new(s));
        let (stop, stop_rx) = watch::channel(false);
        let nowhere = link("127.0.0.1:1".parse().unwrap());
        let h = tokio::spawn(run(
            Link { backoff_min: Duration::from_secs(30), ..nowhere },
            Publisher::new(cfg()),
            rx,
            ok.clone(),
            stop_rx,
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), h).await.unwrap().unwrap();
        // With a password, against a broker without auth (accepted); the snapshot sender goes.
        let broker = Broker::start(None).await;
        let (tx, rx) = watch::channel(Arc::new(snap(vec![])));
        let (_stop, stop_rx) = watch::channel(false);
        let l = Link { credentials: Some(("u".into(), "p".into())), ..link(broker.addr) };
        let (_, seen) = observer(broker.addr, "energie/status").await;
        let h = tokio::spawn(run(l, Publisher::new(cfg()), rx, ok, stop_rx));
        until(&seen, |s| s.contains_key("energie/status")).await;
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), h).await.unwrap().unwrap();
    }
}
