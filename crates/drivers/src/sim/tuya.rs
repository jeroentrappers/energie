//! The Tuya chargers as devices: session-key handshake, DP query, DP pushes, sets and
//! heartbeats, in protocol 3.4 or 3.5. Like the real devices they serve one local connection at
//! a time; a second one is closed straight away.
//!
//! Kolanky cable ([`TuyaSim::start`], 3.4): `124 = OpenCharging` while plugged in starts
//! charging at the set current (`101 = charing`, `105` = A×10, `109` = kW×10), `CloseCharging`
//! stops it, a new `114` while charging changes the current.
//!
//! `qccdz` charger ([`TuyaSim::start_evcharger`], 3.5): `18 = true` with a car connected starts
//! charging at DP 4 (`3 = charger_charging`, `9` and `5` = W), `18 = false` pauses it
//! (`charger_pause`), a new `4` while charging changes the current. No current or voltage DP.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::tuya::protocol::{Frame, Key, cmd};
use crate::tuya::wire::{self, Version};

/// The DPs of the cable at rest, plugged in (`kolanky_evcharger.yaml` ids).
pub fn default_dps() -> Map<String, Value> {
    let v = json!({"101": "connect", "102": 230, "105": 0, "109": 0, "110": 215, "112": 1234,
        "113": "Max16A", "114": 8, "118": 0, "119": 0, "124": "WaitOperation", "10": 0});
    v.as_object().expect("object").clone()
}

/// The `qccdz` charger at rest with a car connected (its DP ids; values as read from the device).
pub fn evcharger_dps() -> Map<String, Value> {
    let v = json!({"1": 1, "3": "charger_insert", "4": 16, "5": 0, "9": 0, "10": 0, "14": "charge_now",
        "18": false, "23": "2.0.0", "24": 18, "25": 0, "27": "online"});
    v.as_object().expect("object").clone()
}

#[derive(Clone, Copy, PartialEq)]
enum Model {
    Kolanky,
    EvCharger,
}

struct Dev {
    model: Model,
    version: Version,
    key: Key,
    dps: Map<String, Value>,
    sets: Vec<(Instant, Map<String, Value>)>,
    handshakes: usize,
    active: bool,
    answer_heartbeats: bool,
    corrupt_next: bool,
    query_override: Option<Value>,
}

impl Dev {
    fn query_reply(&self) -> Value {
        self.query_override.clone().unwrap_or_else(|| json!({"dps": self.dps}))
    }

    fn amps(&self) -> i64 {
        self.dps["114"].as_i64().unwrap_or(0)
    }

    fn charge(&mut self, amps: i64) {
        let volts = self.dps["102"].as_i64().unwrap_or(230);
        self.dps.insert("105".into(), json!(amps * 10));
        self.dps.insert("109".into(), json!(volts * amps / 100));
    }

    /// Apply a set and return every DP that changed.
    fn apply(&mut self, set: &Map<String, Value>) -> Map<String, Value> {
        let before = self.dps.clone();
        for (k, v) in set {
            self.dps.insert(k.clone(), v.clone());
        }
        match self.model {
            Model::Kolanky => self.apply_kolanky(set),
            Model::EvCharger => self.apply_evcharger(set),
        }
        self.dps.iter().filter(|(k, v)| before.get(*k) != Some(v)).map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    fn apply_evcharger(&mut self, set: &Map<String, Value>) {
        let state = self.dps["3"].as_str().unwrap_or_default().to_string();
        let draw = |d: &mut Self, on: bool| {
            let w = if on { d.dps["4"].as_i64().unwrap_or(0) * 230 } else { 0 };
            d.dps.insert("9".into(), json!(w));
            d.dps.insert("5".into(), json!(w));
        };
        match set.get("18").and_then(Value::as_bool) {
            Some(true) if state != "charger_free" && !state.contains("fault") => {
                self.dps.insert("3".into(), json!("charger_charging"));
                draw(self, true);
            }
            Some(false) if state == "charger_charging" => {
                self.dps.insert("3".into(), json!("charger_pause"));
                draw(self, false);
            }
            _ if set.contains_key("4") && state == "charger_charging" => draw(self, true),
            _ => {}
        }
    }

    fn apply_kolanky(&mut self, set: &Map<String, Value>) {
        let status = self.dps["101"].as_str().unwrap_or_default().to_string();
        match set.get("124").and_then(Value::as_str) {
            Some("OpenCharging") if status != "no_connet" => {
                self.dps.insert("101".into(), json!("charing"));
                self.charge(self.amps());
            }
            Some("CloseCharging") if status == "charing" => {
                self.dps.insert("101".into(), json!("connect"));
                self.charge(0);
            }
            _ if set.contains_key("114") && status == "charing" => self.charge(self.amps()),
            _ => {}
        }
    }
}

#[derive(Clone)]
enum Ev {
    Push(Map<String, Value>),
    Drop,
}

type Shared = Arc<Mutex<Dev>>;

/// A running simulated cable.
pub struct TuyaSim {
    pub addr: SocketAddr,
    dev: Shared,
    events: broadcast::Sender<Ev>,
    task: JoinHandle<()>,
}

impl Drop for TuyaSim {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn next_frame(rd: &mut (impl AsyncReadExt + Unpin), v: Version, key: &Key, buf: &mut Vec<u8>) -> Option<Frame> {
    loop {
        match wire::decode(v, key, buf, false) {
            Ok(Some((f, used))) => {
                buf.drain(..used);
                return Some(f);
            }
            Ok(None) => {}
            Err(_) => return None,
        }
        if rd.read_buf(buf).await.ok()? == 0 {
            return None;
        }
    }
}

/// Server side of the handshake; the session key on success.
async fn handshake(stream: &mut TcpStream, v: Version, local: &Key, buf: &mut Vec<u8>) -> Option<Key> {
    use crate::tuya::protocol::hmac;
    let start = next_frame(stream, v, local, buf).await.filter(|f| f.cmd == cmd::SESS_KEY_NEG_START)?;
    let client_nonce: Key = wire::open_handshake(v, local, &start).ok()?.try_into().ok()?;
    let device_nonce: Key = rand::random();
    let body = [&device_nonce[..], &hmac(local, &client_nonce)[..]].concat();
    stream.write_all(&wire::encode(v, local, start.seq, cmd::SESS_KEY_NEG_RESP, Some(0), Some(&body))).await.ok()?;
    let finish = next_frame(stream, v, local, buf).await.filter(|f| f.cmd == cmd::SESS_KEY_NEG_FINISH)?;
    let proof = wire::open_handshake(v, local, &finish).ok()?;
    (proof == hmac(local, &device_nonce)).then(|| wire::session_key(v, local, &client_nonce, &device_nonce))
}

async fn send(
    wr: &mut (impl AsyncWriteExt + Unpin),
    dev: &Shared,
    key: &Key,
    seq: u32,
    command: u32,
    plain: Option<&[u8]>,
) -> bool {
    let (v, corrupt) = {
        let mut d = dev.lock().unwrap();
        (d.version, std::mem::take(&mut d.corrupt_next))
    };
    let mut wire = wire::encode(v, key, seq, command, Some(0), plain);
    if corrupt {
        let n = wire.len();
        wire[n - 5] ^= 0xff;
    }
    wr.write_all(&wire).await.is_ok()
}

fn status_json(dps: &Map<String, Value>) -> Vec<u8> {
    json!({"protocol": 4, "t": 1_794_056_000, "data": {"dps": dps}}).to_string().into_bytes()
}

/// One client session; `None` ends it (closing the socket).
async fn session(mut stream: TcpStream, dev: Shared, mut events: broadcast::Receiver<Ev>) -> Option<()> {
    let (local, v) = {
        let d = dev.lock().unwrap();
        (d.key, d.version)
    };
    // Shared with the handshake: the first query can arrive in the same read as its last frame.
    let mut buf = Vec::new();
    let key = handshake(&mut stream, v, &local, &mut buf).await?;
    dev.lock().unwrap().handshakes += 1;
    let (mut rd, mut wr) = stream.into_split();
    let mut seq = 100;
    loop {
        seq += 1;
        tokio::select! {
            f = next_frame(&mut rd, v, &key, &mut buf) => {
                let f = f?;
                let plain = wire::open(v, &key, &f).unwrap_or_default();
                let ok = match f.cmd {
                    cmd::DP_QUERY_NEW => {
                        let reply = dev.lock().unwrap().query_reply();
                        send(&mut wr, &dev, &key, f.seq, cmd::DP_QUERY_NEW, Some(reply.to_string().as_bytes())).await
                    }
                    cmd::HEART_BEAT if dev.lock().unwrap().answer_heartbeats => send(&mut wr, &dev, &key, f.seq, cmd::HEART_BEAT, None).await,
                    cmd::CONTROL_NEW => {
                        let v: Value = serde_json::from_slice(&plain).unwrap_or_default();
                        let set = v["data"]["dps"].as_object().cloned().unwrap_or_default();
                        let changed = {
                            let mut d = dev.lock().unwrap();
                            d.sets.push((Instant::now(), set.clone()));
                            d.apply(&set)
                        };
                        send(&mut wr, &dev, &key, f.seq, cmd::CONTROL_NEW, None).await
                            && send(&mut wr, &dev, &key, seq, cmd::STATUS, Some(&status_json(&changed))).await
                    }
                    _ => true,
                };
                ok.then_some(())?;
            }
            ev = events.recv() => {
                let Ok(Ev::Push(dps)) = ev else { return None };
                send(&mut wr, &dev, &key, seq, cmd::STATUS, Some(&status_json(&dps))).await.then_some(())?;
            }
        }
    }
}

impl TuyaSim {
    /// Start a Kolanky cable (protocol 3.4) that uses `local_key`, with [`default_dps`].
    pub async fn start(local_key: &Key) -> TuyaSim {
        Self::start_model(Model::Kolanky, Version::V34, local_key, default_dps()).await
    }

    /// Start a `qccdz` charger (protocol 3.5) that uses `local_key`, with [`evcharger_dps`].
    pub async fn start_evcharger(local_key: &Key) -> TuyaSim {
        Self::start_model(Model::EvCharger, Version::V35, local_key, evcharger_dps()).await
    }

    async fn start_model(model: Model, version: Version, local_key: &Key, dps: Map<String, Value>) -> TuyaSim {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let dev = Arc::new(Mutex::new(Dev {
            model,
            version,
            key: *local_key,
            dps,
            sets: Vec::new(),
            handshakes: 0,
            active: false,
            answer_heartbeats: true,
            corrupt_next: false,
            query_override: None,
        }));
        let (events, _) = broadcast::channel(64);
        let (d, ev) = (dev.clone(), events.clone());
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.expect("accept");
                if std::mem::replace(&mut d.lock().unwrap().active, true) {
                    continue; // one local connection at a time: drop the newcomer
                }
                let (d, rx) = (d.clone(), ev.subscribe());
                tokio::spawn(async move {
                    let _ = session(stream, d.clone(), rx).await;
                    d.lock().unwrap().active = false;
                });
            }
        });
        TuyaSim { addr, dev, events, task }
    }

    /// Change a DP on the device and push it to the connected client (like a status report).
    pub fn set_dp(&self, id: &str, value: Value) {
        self.dev.lock().unwrap().dps.insert(id.into(), value.clone());
        let _ = self.events.send(Ev::Push([(id.to_string(), value)].into_iter().collect()));
    }

    /// The device's current DPs.
    pub fn dps(&self) -> Map<String, Value> {
        self.dev.lock().unwrap().dps.clone()
    }

    /// Every set received, with its arrival time.
    pub fn sets(&self) -> Vec<(Instant, Map<String, Value>)> {
        self.dev.lock().unwrap().sets.clone()
    }

    /// Completed handshakes so far.
    pub fn handshakes(&self) -> usize {
        self.dev.lock().unwrap().handshakes
    }

    /// Close the current connection (the device rebooting or dropping the client).
    pub fn drop_connection(&self) {
        let _ = self.events.send(Ev::Drop);
    }

    /// Stop (or resume) answering heartbeats.
    pub fn answer_heartbeats(&self, on: bool) {
        self.dev.lock().unwrap().answer_heartbeats = on;
    }

    /// Answer DP queries with this JSON instead of `{"dps": ..}`.
    pub fn set_query_reply(&self, reply: Value) {
        self.dev.lock().unwrap().query_override = Some(reply);
    }

    /// Corrupt the HMAC (3.4) or GCM tag (3.5) of the next frame the device sends.
    pub fn corrupt_next_frame(&self) {
        self.dev.lock().unwrap().corrupt_next = true;
    }
}
