//! Kolanky EV cable as a Tuya 3.4 device: session-key handshake, DP query, DP pushes, sets and
//! heartbeats. Like the real cable it serves one local connection at a time; a second one is
//! closed straight away.
//!
//! Model: `124 = OpenCharging` while plugged in starts charging at the set current
//! (`101 = charing`, `105` = A×10, `109` = kW×10), `CloseCharging` stops it, a new `114` while
//! charging changes the current.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::tuya::protocol::{self, Frame, Key, cmd};

/// The DPs of the cable at rest, plugged in (`kolanky_evcharger.yaml` ids).
pub fn default_dps() -> Map<String, Value> {
    let v = json!({"101": "connect", "102": 230, "105": 0, "109": 0, "110": 215, "112": 1234,
        "113": "Max16A", "114": 8, "118": 0, "119": 0, "124": "WaitOperation", "10": 0});
    v.as_object().expect("object").clone()
}

struct Dev {
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
        self.dps.iter().filter(|(k, v)| before.get(*k) != Some(v)).map(|(k, v)| (k.clone(), v.clone())).collect()
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

async fn next_frame(rd: &mut (impl AsyncReadExt + Unpin), key: &Key, buf: &mut Vec<u8>) -> Option<Frame> {
    loop {
        match protocol::unpack(key, buf) {
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
async fn handshake(stream: &mut TcpStream, local: &Key, buf: &mut Vec<u8>) -> Option<Key> {
    let start = next_frame(stream, local, buf).await.filter(|f| f.cmd == cmd::SESS_KEY_NEG_START)?;
    let client_nonce: Key = protocol::decrypt(local, &start.payload).ok()?.try_into().ok()?;
    let device_nonce: Key = rand::random();
    let body = [&device_nonce[..], &protocol::hmac(local, &client_nonce)[..]].concat();
    let resp = Frame {
        seq: start.seq,
        cmd: cmd::SESS_KEY_NEG_RESP,
        retcode: Some(0),
        payload: protocol::encrypt(local, &body),
    };
    stream.write_all(&protocol::pack(local, &resp)).await.ok()?;
    let finish = next_frame(stream, local, buf).await.filter(|f| f.cmd == cmd::SESS_KEY_NEG_FINISH)?;
    let proof = protocol::decrypt(local, &finish.payload).ok()?;
    (proof == protocol::hmac(local, &device_nonce)).then(|| protocol::session_key(local, &client_nonce, &device_nonce))
}

async fn send(
    wr: &mut (impl AsyncWriteExt + Unpin),
    dev: &Shared,
    key: &Key,
    seq: u32,
    command: u32,
    plain: Option<&[u8]>,
) -> bool {
    let payload = plain.map(|p| protocol::seal(key, command, p)).unwrap_or_default();
    let mut wire = protocol::pack(key, &Frame { seq, cmd: command, retcode: Some(0), payload });
    if std::mem::take(&mut dev.lock().unwrap().corrupt_next) {
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
    let local = dev.lock().unwrap().key;
    // Shared with the handshake: the first query can arrive in the same read as its last frame.
    let mut buf = Vec::new();
    let key = handshake(&mut stream, &local, &mut buf).await?;
    dev.lock().unwrap().handshakes += 1;
    let (mut rd, mut wr) = stream.into_split();
    let mut seq = 100;
    loop {
        seq += 1;
        tokio::select! {
            f = next_frame(&mut rd, &key, &mut buf) => {
                let f = f?;
                let plain = protocol::open(&key, &f.payload).unwrap_or_default();
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
    /// Start a cable that uses `local_key`, with [`default_dps`].
    pub async fn start(local_key: &Key) -> TuyaSim {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let dev = Arc::new(Mutex::new(Dev {
            key: *local_key,
            dps: default_dps(),
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

    /// Corrupt the HMAC of the next frame the device sends.
    pub fn corrupt_next_frame(&self) {
        self.dev.lock().unwrap().corrupt_next = true;
    }
}
