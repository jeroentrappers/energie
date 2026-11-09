//! An embedded MQTT broker (`rumqttd`) and a SmartEVSE v3 that lives on it.
//!
//! The simulated charger publishes the topics observed on the home broker (retained), its
//! availability with an `offline` last will, and the HA discovery config of the
//! charge-current override. It obeys `Set/CurrentOverride` (0, or 6.0 A up to `MaxCurrent`, in
//! tenths) and `Set/Mode` (one of the select's options); anything else is recorded and ignored.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use rumqttd::{Config, ConnectionSettings, RouterConfig, ServerSettings};
use tokio::task::JoinHandle;

/// A broker running on its own threads for the rest of the process.
pub struct Broker {
    pub addr: SocketAddr,
}

impl Broker {
    /// Start a broker on a free local port; `credentials` turns on user/password auth.
    ///
    /// The port is picked by binding port 0 and letting it go, so another test (or process) can
    /// take it before the broker binds it. The broker's `start` only returns when its listener
    /// failed, so a broker that has not returned shortly after the port answers is ours; one
    /// that did is retried on a new port.
    pub async fn start(credentials: Option<(&str, &str)>) -> Broker {
        let (mut broker, mut tries) = (None, 0);
        while broker.is_none() && tries < 20 {
            tries += 1;
            let port = std::net::TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port();
            broker = Broker::start_on(SocketAddr::from(([127, 0, 0, 1], port)), credentials).await;
        }
        broker.expect("an MQTT broker came up")
    }

    /// A broker on `addr`, or `None` when it could not listen there.
    async fn start_on(addr: SocketAddr, credentials: Option<(&str, &str)>) -> Option<Broker> {
        let (alive, running) = std::sync::mpsc::channel::<()>();
        let config = broker_config(addr, credentials);
        std::thread::spawn(move || {
            let _alive = alive;
            drop(rumqttd::Broker::new(config).start());
        });
        let ended = || !matches!(running.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        for _ in 0..1000 {
            if ended() || tokio::net::TcpStream::connect(addr).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        (!ended() && tokio::net::TcpStream::connect(addr).await.is_ok()).then_some(Broker { addr })
    }
}

fn broker_config(addr: SocketAddr, credentials: Option<(&str, &str)>) -> Config {
    let connections = ConnectionSettings {
        connection_timeout_ms: 5000,
        max_payload_size: 20480,
        max_inflight_count: 100,
        auth: credentials.map(|(u, p)| HashMap::from([(u.to_string(), p.to_string())])),
        external_auth: None,
        dynamic_filters: true,
    };
    let server =
        ServerSettings { name: "v4".into(), listen: addr, tls: None, next_connection_delay_ms: 1, connections };
    Config {
        router: RouterConfig {
            max_connections: 100,
            max_outgoing_packet_count: 200,
            max_segment_size: 1 << 20,
            max_segment_count: 10,
            ..RouterConfig::default()
        },
        v4: Some(HashMap::from([("v4".to_string(), server)])),
        ..Config::default()
    }
}

struct Dev {
    prefix: String,
    topics: BTreeMap<String, String>,
    received: Vec<(String, String)>,
}

/// The topics of the live charger on 2026-11-07, plugged in and charging at 15.1 A.
fn live_topics() -> BTreeMap<String, String> {
    [
        ("EVPlugState", "Connected"),
        ("State", "Charging"),
        ("StateID", "C"),
        ("Mode", "Normal"),
        ("ChargeCurrent", "151"),
        ("ChargeCurrentOverride", "151"),
        ("MaxCurrent", "290"),
        ("Access", "Allow"),
        ("Error", "None"),
        ("NrOfPhases", "3"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

/// A running simulated SmartEVSE.
pub struct SmartEvseSim {
    dev: Arc<Mutex<Dev>>,
    client: AsyncClient,
    task: JoinHandle<()>,
}

impl Drop for SmartEvseSim {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn publish_all(client: &AsyncClient, dev: &Dev) {
    let p = &dev.prefix;
    for (k, v) in &dev.topics {
        let _ = client.try_publish(format!("{p}/{k}"), QoS::AtMostOnce, true, v.clone());
    }
    let discovery = serde_json::json!({"name": "Charge Current Override", "state_topic": format!("{p}/ChargeCurrentOverride"),
        "command_topic": format!("{p}/Set/CurrentOverride"), "min": "0", "max": "29", "mode": "slider",
        "value_template": "{{ value | int / 10 if value | is_number else none }}", "command_template": "{{ value | int * 10 }}"});
    let _ = client.try_publish(
        format!("homeassistant/number/{p}-ChargeCurrentOverride/config"),
        QoS::AtMostOnce,
        true,
        discovery.to_string(),
    );
    let _ = client.try_publish(format!("{p}/connected"), QoS::AtMostOnce, true, "online");
}

/// Apply a `Set/...` command like the firmware; returns the topics that changed.
fn obey(dev: &mut Dev, what: &str, payload: &str) -> Vec<(String, String)> {
    let max = dev.topics["MaxCurrent"].parse::<u32>().unwrap_or(0);
    let accepted = match what {
        "CurrentOverride" => payload.parse::<u32>().ok().filter(|&d| d == 0 || (60..=max).contains(&d)).map(|d| {
            let mut v = vec![("ChargeCurrentOverride".to_string(), d.to_string())];
            if dev.topics["State"] == "Charging" && d > 0 {
                v.push(("ChargeCurrent".to_string(), d.to_string()));
            }
            v
        }),
        "Mode" => crate::smartevse::parse_mode(payload).map(|_| vec![("Mode".to_string(), payload.to_string())]),
        _ => None,
    };
    let changes = accepted.unwrap_or_default();
    for (k, v) in &changes {
        dev.topics.insert(k.clone(), v.clone());
    }
    changes
}

impl SmartEvseSim {
    /// Connect a charger with topic prefix `prefix` to the broker at `broker`.
    pub async fn start(broker: SocketAddr, prefix: &str, credentials: Option<(&str, &str)>) -> SmartEvseSim {
        let mut opts =
            MqttOptions::new(format!("sim-{}", prefix.replace('/', "-")), broker.ip().to_string(), broker.port());
        opts.set_keep_alive(Duration::from_secs(5));
        opts.set_last_will(LastWill::new(format!("{prefix}/connected"), "offline", QoS::AtMostOnce, true));
        if let Some((u, p)) = credentials {
            opts.set_credentials(u, p);
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        let dev = Arc::new(Mutex::new(Dev { prefix: prefix.to_string(), topics: live_topics(), received: Vec::new() }));
        let (d, c, set_prefix) = (dev.clone(), client.clone(), format!("{prefix}/Set/"));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let mut ready = Some(ready_tx);
        let task = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        publish_all(&c, &d.lock().unwrap());
                        let _ = c.try_subscribe(format!("{set_prefix}#"), QoS::AtMostOnce);
                    }
                    Ok(Event::Incoming(Packet::SubAck(_))) => {
                        let _ = ready.take().map(|r| r.send(()));
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let payload = String::from_utf8_lossy(&p.payload).to_string();
                        let mut dev = d.lock().unwrap();
                        dev.received.push((p.topic.clone(), payload.clone()));
                        let what = p.topic.strip_prefix(&set_prefix).unwrap_or_default().to_string();
                        for (k, v) in obey(&mut dev, &what, &payload) {
                            let _ = c.try_publish(format!("{}/{k}", dev.prefix), QoS::AtMostOnce, true, v);
                        }
                    }
                    _ => {}
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(10), ready_rx)
            .await
            .expect("the simulated charger subscribed within 10 s")
            .expect("the simulator task runs");
        SmartEvseSim { dev, client, task }
    }

    /// Publish a state topic (retained), e.g. `("EVPlugState", "Disconnected")`.
    pub fn set(&self, suffix: &str, payload: &str) {
        let mut d = self.dev.lock().unwrap();
        d.topics.insert(suffix.to_string(), payload.to_string());
        let _ = self.client.try_publish(format!("{}/{suffix}", d.prefix), QoS::AtMostOnce, true, payload.to_string());
    }

    /// The charger's current value of a state topic.
    pub fn get(&self, suffix: &str) -> Option<String> {
        self.dev.lock().unwrap().topics.get(suffix).cloned()
    }

    /// Every `Set/...` message received: (topic, payload).
    pub fn received(&self) -> Vec<(String, String)> {
        self.dev.lock().unwrap().received.clone()
    }

    /// Drop off the network without saying goodbye: the broker publishes the `offline` will.
    pub fn kill(&self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Set/...` the firmware does not know is recorded and changes nothing.
    #[tokio::test]
    async fn a_taken_port_is_not_mistaken_for_the_broker() {
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(Broker::start_on(taken.local_addr().unwrap(), None).await.is_none());
    }

    #[tokio::test]
    async fn unknown_set_topic_is_ignored() {
        let broker = Broker::start(None).await;
        let sim = SmartEvseSim::start(broker.addr, "SmartEVSE/1", None).await;
        let (client, mut ev) = AsyncClient::new(MqttOptions::new("t", "127.0.0.1", broker.addr.port()), 8);
        client.publish("SmartEVSE/1/Set/CableLock", QoS::AtMostOnce, false, "1").await.unwrap();
        tokio::spawn(async move { while ev.poll().await.is_ok() {} });
        for _ in 0..400 {
            if !sim.received().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(sim.received(), vec![("SmartEVSE/1/Set/CableLock".to_string(), "1".to_string())]);
        assert_eq!(sim.get("Mode").as_deref(), Some("Normal"));
    }
}
