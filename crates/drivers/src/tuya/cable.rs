//! The Kolanky EV charging cable (`tuya_local`, device type `kolanky_evcharger`).
//!
//! A background worker keeps one 3.4 session open (the cable allows a single local client),
//! merges DP queries and pushes into the current state, sends heartbeats, carries out commands
//! and reconnects after the link drops. The [`CableHandle`] is the [`Source`] and [`Actuator`].
//!
//! DP map (`devices/kolanky_evcharger.yaml`):
//!
//! | DP | Entity | Encoding |
//! | --- | --- | --- |
//! | 101 | `sensor.ev_charging_cable_status` | `no_connet`→available, `connect`→plugged_in, `charing`→charging, `wait_rfid`→waiting_for_authorization, `finish`→charged, `wait_charing`→waiting, `error`→fault |
//! | 102 | `sensor.…_spanning` | V |
//! | 105 | `sensor.…_stroom` | A ×10 |
//! | 109 | `sensor.…_vermogen` | kW ×10 |
//! | 113 | (max current constraint) | `Max16A` / `Max32A` |
//! | 114 / 115 | `number.…_charge_current` | A, 6..16 (or DP 115, 8..32, when 113 = `Max32A`) |
//! | 124 | `select.…_charging_operation` | `OpenCharging`=Start charging, `CloseCharging`=Stop charging, `WaitOperation`=No operation |

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use energie_core::command::Command;
use energie_core::world::Cable;
use serde_json::{Map, Value, json};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep_until, timeout};

use super::protocol::{self, Key, cmd};
use super::session::{Reader, Writer, negotiate};
use crate::driver::{Actuator, Applied, Outcome, Reading, Source, Write};
use crate::entity::Entity;
use crate::error::{DriverError, Result};

/// How to reach the cable and how the worker paces itself.
#[derive(Debug, Clone)]
pub struct CableConfig {
    /// `192.168.1.60:6668`.
    pub addr: SocketAddr,
    /// Tuya device id (config entry `device_id`).
    pub device_id: String,
    /// 16-character local key (config entry `local_key`; never commit it).
    pub local_key: String,
    /// Heartbeat period; the link counts as dead after three periods without any frame.
    pub heartbeat: Duration,
    /// Delay between `Start charging` and setting the current (`script.laadkabel_start_laden`).
    pub start_delay: Duration,
    /// Wait before reconnecting after the link drops.
    pub reconnect_delay: Duration,
    /// Connect, handshake and first-query timeout.
    pub timeout: Duration,
}

impl CableConfig {
    pub fn new(addr: SocketAddr, device_id: impl Into<String>, local_key: impl Into<String>) -> Self {
        CableConfig {
            addr,
            device_id: device_id.into(),
            local_key: local_key.into(),
            heartbeat: Duration::from_secs(10),
            start_delay: Duration::from_secs(3),
            reconnect_delay: Duration::from_secs(5),
            timeout: Duration::from_secs(5),
        }
    }
}

/// `sensor.ev_charging_cable_status` for a DP 101 value. Unknown values pass through.
pub fn status_name(dp: &str) -> String {
    match dp {
        "no_connet" => "available",
        "connect" => "plugged_in",
        "charing" => "charging",
        "wait_rfid" => "waiting_for_authorization",
        "finish" => "charged",
        "wait_charing" => "waiting",
        "error" => "fault",
        other => other,
    }
    .to_string()
}

fn max32(dps: &Map<String, Value>) -> bool {
    dps.get("113").and_then(Value::as_str) == Some("Max32A")
}

/// The HA entities of the cable from its DPs.
pub fn cable_from_dps(dps: &Map<String, Value>) -> Cable {
    let num = |k: &str| dps.get(k).and_then(Value::as_f64);
    let wide = max32(dps);
    Cable {
        status: dps.get("101").and_then(Value::as_str).map(status_name),
        current_a: num("105").map(|v| v / 10.0),
        voltage_v: num("102"),
        power_kw: num("109").map(|v| v / 10.0),
        set_current_a: num(if wide { "115" } else { "114" }),
        set_current_max_a: Some(if wide { 32.0 } else { 16.0 }),
    }
}

/// The DP and range `number.ev_charging_cable_charge_current` writes to.
fn current_dp(dps: &Map<String, Value>) -> (&'static str, u8, u8) {
    if max32(dps) { ("115", 8, 32) } else { ("114", 6, 16) }
}

/// The cable commands, after [`Actuator::apply`] filtered out everything else.
enum Op {
    Start(u8),
    Stop,
    Current(u8),
    Reconnect,
}

fn op_of(cmd: &Command) -> Option<Op> {
    match *cmd {
        Command::CableStart { amps } => Some(Op::Start(amps)),
        Command::CableStop => Some(Op::Stop),
        Command::CableSetCurrent(amps) => Some(Op::Current(amps)),
        Command::CableReconnect => Some(Op::Reconnect),
        _ => None,
    }
}

#[derive(Default)]
struct State {
    connected: bool,
    dps: Map<String, Value>,
}

type Shared = Arc<Mutex<State>>;
type Reply = oneshot::Sender<Result<Applied>>;

/// Handle to the cable worker. Dropping it ends the worker and closes the session.
pub struct CableHandle {
    tx: mpsc::Sender<(Op, Reply)>,
    state: Shared,
}

impl CableHandle {
    /// Start the worker. Fails only on a malformed local key; connection problems are retried.
    pub fn spawn(cfg: CableConfig) -> Result<CableHandle> {
        let key = protocol::parse_key(&cfg.local_key)?;
        let state: Shared = Arc::default();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(Worker { cfg, key, state: state.clone(), rx, pending: None }.run());
        Ok(CableHandle { tx, state })
    }

    /// Whether a session is open.
    pub fn connected(&self) -> bool {
        self.state.lock().unwrap().connected
    }

    /// The raw DPs as last reported.
    pub fn dps(&self) -> Map<String, Value> {
        self.state.lock().unwrap().dps.clone()
    }
}

#[async_trait]
impl Source for CableHandle {
    fn name(&self) -> &'static str {
        "tuya_cable"
    }

    /// The current state; `NotConnected` while there is no session (HA: `unavailable`).
    async fn read(&mut self) -> Result<Reading> {
        let s = self.state.lock().unwrap();
        if !s.connected {
            return Err(DriverError::NotConnected);
        }
        Ok(Reading::Cable(cable_from_dps(&s.dps)))
    }

    fn unavailable(&self) -> Reading {
        Reading::Cable(Cable::default())
    }

    /// The cable's other `tuya_local` entities, from the DPs already reported (no extra traffic).
    fn entities(&self) -> Vec<Entity> {
        extra_entities(&self.state.lock().unwrap().dps)
            .into_iter()
            .zip(HA_ENTITIES)
            .map(|(e, (_, ha))| e.with_ha(ha))
            .collect()
    }
}

/// The key of each extra cable entity and the HA entity it replaces.
pub const HA_ENTITIES: [(&str, &str); 8] = [
    ("temperature_c", "sensor.ev_charging_cable_temperatuur"),
    ("energy_kwh", "sensor.ev_charging_cable_energie"),
    ("charging_time_h", "sensor.ev_charging_cable_charging_time"),
    ("operation", "select.ev_charging_cable_charging_operation"),
    ("status_raw", "sensor.ev_charging_cable_status"),
    ("fault", "binary_sensor.ev_charging_cable_probleem"),
    ("delay_h", "number.ev_charging_cable_delay"),
    ("charging_duration_h", "number.ev_charging_cable_charging_duration"),
];

/// `select.ev_charging_cable_charging_operation` for a DP 124 value; unknown values pass through.
pub fn operation_name(dp: &str) -> &str {
    match dp {
        "OpenCharging" => "Start charging",
        "CloseCharging" => "Stop charging",
        "WaitOperation" => "No operation",
        other => other,
    }
}

/// `kolanky_evcharger.yaml`: DP 110 °C ×10, DP 112 kWh ×10 (the session energy), DP 120 h ×10,
/// DP 124 the operation select, DP 101 raw, DP 10 the fault bitfield (any bit = problem),
/// DP 118 / 119 the delay and duration numbers (h).
pub fn extra_entities(dps: &Map<String, Value>) -> Vec<Entity> {
    use crate::entity::Value as V;
    let num = |k: &str, scale: f64| -> V { dps.get(k).and_then(Value::as_f64).map(|v| v / scale).into() };
    let text = |k: &str, f: fn(&str) -> &str| -> V { dps.get(k).and_then(Value::as_str).map(f).into() };
    vec![
        Entity::sensor("temperature_c", "Temperatuur", num("110", 10.0)).with_unit("temperature", "°C"),
        Entity::sensor("energy_kwh", "Energie (sessie)", num("112", 10.0)).with_unit("energy", "kWh"),
        Entity::sensor("charging_time_h", "Laadtijd", num("120", 10.0)).with_unit("duration", "h"),
        Entity::sensor("operation", "Laadbediening", text("124", operation_name)),
        Entity::sensor("status_raw", "Status (DP 101)", text("101", |s| s)),
        Entity::sensor("fault", "Probleem", dps.get("10").and_then(Value::as_f64).map(|v| v != 0.0).into()),
        Entity::sensor("delay_h", "Uitstel", num("118", 1.0)).with_unit("duration", "h"),
        Entity::sensor("charging_duration_h", "Laadduur", num("119", 1.0)).with_unit("duration", "h"),
    ]
}

/// The DP writes of `op` against the DPs as last reported (`before` from them). A start's
/// current follows `start_delay` after DP 124, and only when it is within the DP's range (the
/// HA script's `number.set_value` fails otherwise and the start stays).
fn writes_of(op: &Op, dps: &Map<String, Value>) -> Result<Vec<Write>> {
    let dp = |id: &str, v: Value| Write::new(format!("dp {id}"), v).before(dps.get(id).cloned());
    let (cur, lo, hi) = current_dp(dps);
    Ok(match *op {
        Op::Start(amps) => {
            let mut w = vec![dp("124", json!("OpenCharging"))];
            if (lo..=hi).contains(&amps) {
                w.push(dp(cur, json!(amps)).note("after start_delay (script.laadkabel_start_laden: delay 3 s)"));
            }
            w
        }
        Op::Stop => vec![dp("124", json!("CloseCharging"))],
        Op::Current(amps) if (lo..=hi).contains(&amps) => vec![dp(cur, json!(amps))],
        Op::Current(amps) => return Err(DriverError::OutOfRange(format!("{amps} A, cable accepts {lo}..{hi}"))),
        Op::Reconnect => vec![Write::new("tuya session", "reconnect")],
    })
}

impl CableHandle {
    /// The writes of `cmd` now; `NotConnected` without a session, as the command itself.
    fn planned(&self, op: &Op) -> Result<Vec<Write>> {
        let s = self.state.lock().unwrap();
        if !s.connected && !matches!(op, Op::Reconnect) {
            return Err(DriverError::NotConnected);
        }
        writes_of(op, &s.dps)
    }
}

#[async_trait]
impl Actuator for CableHandle {
    /// `CableStart` returns `Scheduled` once `Start charging` is sent; the current follows after
    /// `start_delay`. While the link is down every command but `CableReconnect` fails with
    /// `NotConnected`.
    async fn apply(&mut self, cmd: &Command) -> Result<Applied> {
        let Some(op) = op_of(cmd) else { return Ok(Applied::NotMine) };
        let (reply, rx) = oneshot::channel();
        self.tx.send((op, reply)).await.map_err(|_| DriverError::NotConnected)?;
        rx.await.map_err(|_| DriverError::NotConnected)?
    }

    async fn execute(&mut self, cmd: &Command) -> Result<Outcome> {
        let writes = match op_of(cmd) {
            Some(op) => self.planned(&op).unwrap_or_default(),
            None => Vec::new(),
        };
        let applied = self.apply(cmd).await?;
        Ok(Outcome { applied, writes })
    }

    /// The DP sets `execute` would send; nothing is sent.
    async fn plan(&mut self, cmd: &Command) -> Result<Outcome> {
        let Some(op) = op_of(cmd) else { return Ok(Outcome { applied: Applied::NotMine, writes: Vec::new() }) };
        let applied = if matches!(op, Op::Start(_)) { Applied::Scheduled } else { Applied::Done };
        Ok(Outcome { applied, writes: self.planned(&op)? })
    }
}

/// Why a session ended.
enum Exit {
    Lost,
    Reconnect,
    Closed,
}

struct Worker {
    cfg: CableConfig,
    key: Key,
    state: Shared,
    rx: mpsc::Receiver<(Op, Reply)>,
    /// The pending second half of a `CableStart`: when, and which current.
    pending: Option<(Instant, u8)>,
}

type Session = (Reader<TcpStream>, Writer<TcpStream>);

impl Worker {
    async fn connect(&self) -> Result<Session> {
        let hs = async {
            let stream = TcpStream::connect(self.cfg.addr).await?;
            let (mut rd, mut wr) = negotiate(stream, &self.key, rand::random()).await?;
            wr.send(cmd::DP_QUERY_NEW, b"{}").await?;
            let frame = rd.recv().await?;
            let dps = dps_of(&wr.open(&frame)?).ok_or(DriverError::Tuya("no DPs in query response".into()))?;
            let mut s = self.state.lock().unwrap();
            s.dps = dps;
            s.connected = true;
            Ok((rd, wr))
        };
        timeout(self.cfg.timeout, hs).await?
    }

    async fn run(mut self) {
        loop {
            let exit = match self.connect().await {
                Ok(session) => self.serve(session).await,
                Err(_) => Exit::Lost,
            };
            {
                let mut s = self.state.lock().unwrap();
                s.connected = false;
                s.dps.clear();
            }
            self.pending = None;
            match exit {
                Exit::Closed => return,
                Exit::Reconnect => continue,
                Exit::Lost => {}
            }
            // Wait before reconnecting; commands meanwhile fail fast, a reconnect skips the wait.
            let until = Instant::now() + self.cfg.reconnect_delay;
            loop {
                tokio::select! {
                    _ = sleep_until(until) => break,
                    m = self.rx.recv() => match m {
                        None => return,
                        Some((Op::Reconnect, reply)) => {
                            let _ = reply.send(Ok(Applied::Done));
                            break;
                        }
                        Some((_, reply)) => {
                            let _ = reply.send(Err(DriverError::NotConnected));
                        }
                    },
                }
            }
        }
    }

    async fn set(&self, wr: &mut Writer<TcpStream>, dps: Value) -> Result<()> {
        let t = chrono::Utc::now().timestamp();
        wr.send(cmd::CONTROL_NEW, json!({"protocol": 5, "t": t, "data": {"dps": dps}}).to_string().as_bytes()).await
    }

    async fn set_current(&self, wr: &mut Writer<TcpStream>, amps: u8) -> Result<()> {
        let (dp, lo, hi) = current_dp(&self.state.lock().unwrap().dps);
        if !(lo..=hi).contains(&amps) {
            return Err(DriverError::OutOfRange(format!("{amps} A, cable accepts {lo}..{hi}")));
        }
        self.set(wr, json!({ dp: amps })).await
    }

    async fn command(&mut self, wr: &mut Writer<TcpStream>, op: Op) -> Result<Applied> {
        match op {
            Op::Start(amps) => {
                self.set(wr, json!({"124": "OpenCharging"})).await?;
                // `mode: restart`: a new start replaces a pending one.
                self.pending = Some((Instant::now() + self.cfg.start_delay, amps));
                Ok(Applied::Scheduled)
            }
            Op::Stop => self.set(wr, json!({"124": "CloseCharging"})).await.map(|()| Applied::Done),
            Op::Current(amps) => self.set_current(wr, amps).await.map(|()| Applied::Done),
            Op::Reconnect => Ok(Applied::Done),
        }
    }

    /// Run one session until the link is lost, a reconnect is asked for, or the handle is gone.
    /// Write errors are not acted on here: a dead link shows up as a read error or as silence
    /// for three heartbeat periods.
    async fn serve(&mut self, (mut rd, mut wr): Session) -> Exit {
        let mut last_rx = Instant::now();
        let mut beat = tokio::time::interval_at(Instant::now() + self.cfg.heartbeat, self.cfg.heartbeat);
        let heartbeat = json!({"gwId": self.cfg.device_id, "devId": self.cfg.device_id}).to_string();
        loop {
            let deadline = self.pending.map(|(at, _)| at);
            tokio::select! {
                frame = rd.recv() => {
                    let Ok(frame) = frame else { return Exit::Lost };
                    last_rx = Instant::now();
                    if let Some(dps) = wr.open(&frame).ok().as_deref().and_then(dps_of) {
                        self.state.lock().unwrap().dps.extend(dps);
                    }
                }
                _ = beat.tick() => {
                    if last_rx.elapsed() > self.cfg.heartbeat * 3 {
                        return Exit::Lost;
                    }
                    let _ = wr.send(cmd::HEART_BEAT, heartbeat.as_bytes()).await;
                }
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    let (_, amps) = self.pending.take().expect("pending");
                    // HA's script fails at `number.set_value` on a bad value; the start stays.
                    let _ = self.set_current(&mut wr, amps).await;
                }
                m = self.rx.recv() => {
                    let Some((op, reply)) = m else { return Exit::Closed };
                    let reconnect = matches!(op, Op::Reconnect);
                    let _ = reply.send(self.command(&mut wr, op).await);
                    if reconnect {
                        return Exit::Reconnect;
                    }
                }
            }
        }
    }
}

/// The `dps` object of a query response (`{"dps": {..}}`) or a status push
/// (`{"data": {"dps": {..}}}`).
fn dps_of(plain: &[u8]) -> Option<Map<String, Value>> {
    let v: Value = serde_json::from_slice(plain).ok()?;
    v.get("dps").or_else(|| v.get("data").and_then(|d| d.get("dps"))).and_then(Value::as_object).cloned()
}

#[cfg(test)]
mod tests;
