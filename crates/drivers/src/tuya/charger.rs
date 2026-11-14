//! The session worker shared by the Tuya chargers, generic over a device [`Profile`].
//!
//! A background worker keeps one session open (these devices allow a single local client),
//! merges DP queries and pushes into the current state, sends heartbeats, carries out commands
//! and reconnects after the link drops. The [`TuyaCharger`] handle is the [`Source`] and
//! [`Actuator`]. What differs per device (its DPs, how they map to the `charger:ev3` reading
//! and which DPs a command sets) is the [`Profile`].

use std::marker::PhantomData;
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
use super::wire::Version;
use crate::driver::{Actuator, Applied, Outcome, Reading, Source, Write};
use crate::entity::Entity;
use crate::error::{DriverError, Result};

/// DP id → value, as the device reports and takes them.
pub type Dps = Map<String, Value>;

/// One kind of Tuya charger: its DPs as the `charger:ev3` slot sees them.
pub trait Profile: Send + Sync + 'static {
    /// The driver name ([`Source::name`]).
    const NAME: &'static str;
    /// The note on a start's deferred current write in plans.
    const DEFERRED_NOTE: &'static str = "after start_delay";
    /// DPs the device only reports when asked (`UPDATEDPS`), such as a meter: requested every
    /// [`ChargerConfig::refresh`] while connected.
    const REFRESH: &'static [u32] = &[];
    /// The largest drop in set current per write (`None`: any). A larger drop is written in
    /// steps of this size every [`ChargerConfig::step_interval`], so the car can follow; a
    /// charger that sees the car draw well over a new, much lower setting trips on overcurrent.
    const STEP_DOWN: Option<u8> = None;

    /// The `charger:ev3` reading from the DPs.
    fn cable(dps: &Dps) -> Cable;
    /// The device's other entities, from the DPs already reported.
    fn entities(dps: &Dps) -> Vec<Entity>;
    /// The DPs a `CableStart` sets at once, and the current to set `start_delay` later (`None`:
    /// nothing follows). The deferred current goes through [`Profile::current`] when it is due
    /// and is dropped if refused, as HA's script stops at a refused `number.set_value`.
    fn start(amps: u8, dps: &Dps) -> (Dps, Option<u8>);
    /// The DPs a `CableStop` sets.
    fn stop(dps: &Dps) -> Dps;
    /// The DPs a `CableSetCurrent` sets; [`DriverError::OutOfRange`] when the device refuses it.
    fn current(amps: u8, dps: &Dps) -> Result<Dps>;
}

/// How to reach the device and how the worker paces itself.
#[derive(Debug, Clone)]
pub struct ChargerConfig {
    /// `192.168.1.94:6668`.
    pub addr: SocketAddr,
    /// Tuya device id (config entry `device_id`).
    pub device_id: String,
    /// 16-character local key (config entry `local_key`; never commit it).
    pub local_key: String,
    /// The device's local protocol version.
    pub version: Version,
    /// Heartbeat period; the link counts as dead after three periods without any frame.
    pub heartbeat: Duration,
    /// How often [`Profile::REFRESH`] DPs are requested; zero: never.
    pub refresh: Duration,
    /// Time between the steps of a lowered current ([`Profile::STEP_DOWN`]).
    pub step_interval: Duration,
    /// Delay between the start DPs and a deferred current ([`Profile::start`]).
    pub start_delay: Duration,
    /// Wait before reconnecting after the link drops.
    pub reconnect_delay: Duration,
    /// Connect, handshake and first-query timeout.
    pub timeout: Duration,
}

impl ChargerConfig {
    /// Protocol 3.4 with the production timings; set `version` for another protocol.
    pub fn new(addr: SocketAddr, device_id: impl Into<String>, local_key: impl Into<String>) -> Self {
        ChargerConfig {
            addr,
            device_id: device_id.into(),
            local_key: local_key.into(),
            version: Version::V34,
            heartbeat: Duration::from_secs(10),
            refresh: Duration::from_secs(5),
            step_interval: Duration::from_secs(5),
            start_delay: Duration::from_secs(3),
            reconnect_delay: Duration::from_secs(5),
            timeout: Duration::from_secs(5),
        }
    }
}

/// The charger commands, after [`Actuator::apply`] filtered out everything else.
pub(crate) enum Op {
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
    dps: Dps,
    /// energie switched the charger off and it has not charged since. A charger that ends its
    /// session on a switch-off (the `qccdz` goes to `charger_end`) then reads `plugged_in`,
    /// not `charged`: the car is not full, so the logic must not hold back its next start.
    stopped: bool,
}

impl State {
    /// The reading, with a session energie ended reported as `plugged_in`.
    fn cable<P: Profile>(&self) -> Cable {
        let mut c = P::cable(&self.dps);
        if self.stopped && c.status.as_deref() == Some("charged") {
            c.status = Some("plugged_in".into());
        }
        c
    }
}

type Shared = Arc<Mutex<State>>;
type Reply = oneshot::Sender<Result<Applied>>;

/// Handle to a charger worker. Dropping it ends the worker and closes the session.
pub struct TuyaCharger<P> {
    tx: mpsc::Sender<(Op, Reply)>,
    state: Shared,
    profile: PhantomData<fn() -> P>,
}

impl<P: Profile> TuyaCharger<P> {
    /// Start the worker. Fails only on a malformed local key; connection problems are retried.
    pub fn spawn(cfg: ChargerConfig) -> Result<Self> {
        let key = protocol::parse_key(&cfg.local_key)?;
        let state: Shared = Arc::default();
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(
            Worker::<P> {
                cfg,
                key,
                state: state.clone(),
                rx,
                pending: None,
                ramp: None,
                written: None,
                profile: PhantomData,
            }
            .run(),
        );
        Ok(TuyaCharger { tx, state, profile: PhantomData })
    }

    /// Whether a session is open.
    pub fn connected(&self) -> bool {
        self.state.lock().unwrap().connected
    }

    /// The raw DPs as last reported.
    pub fn dps(&self) -> Dps {
        self.state.lock().unwrap().dps.clone()
    }

    /// The writes of `cmd` now; `NotConnected` without a session, as the command itself.
    fn planned(&self, op: &Op) -> Result<Vec<Write>> {
        let s = self.state.lock().unwrap();
        if !s.connected && !matches!(op, Op::Reconnect) {
            return Err(DriverError::NotConnected);
        }
        writes_of::<P>(op, &s.dps)
    }
}

#[async_trait]
impl<P: Profile> Source for TuyaCharger<P> {
    fn name(&self) -> &'static str {
        P::NAME
    }

    /// The current state; `NotConnected` while there is no session (HA: `unavailable`).
    async fn read(&mut self) -> Result<Reading> {
        let s = self.state.lock().unwrap();
        if !s.connected {
            return Err(DriverError::NotConnected);
        }
        Ok(Reading::Cable(s.cable::<P>()))
    }

    fn unavailable(&self) -> Reading {
        Reading::Cable(Cable::default())
    }

    /// The device's other entities, from the DPs already reported (no extra traffic).
    fn entities(&self) -> Vec<Entity> {
        P::entities(&self.state.lock().unwrap().dps)
    }
}

/// The DP writes of `op` against the DPs as last reported (`before` from them).
fn writes_of<P: Profile>(op: &Op, dps: &Dps) -> Result<Vec<Write>> {
    let set = |m: Dps| -> Vec<Write> {
        m.into_iter().map(|(id, v)| Write::new(format!("dp {id}"), v).before(dps.get(&id).cloned())).collect()
    };
    Ok(match *op {
        Op::Start(amps) => {
            let (now, later) = P::start(amps, dps);
            let mut w = set(now);
            if let Some(Ok(m)) = later.map(|a| P::current(a, dps)) {
                w.extend(set(m).into_iter().map(|w| w.note(P::DEFERRED_NOTE)));
            }
            w
        }
        Op::Stop => set(P::stop(dps)),
        Op::Current(amps) => {
            let w = set(P::current(amps, dps)?);
            match (P::STEP_DOWN, set_current::<P>(dps)) {
                (Some(step), Some(from)) if from > amps.saturating_add(step) => {
                    w.into_iter().map(|w| w.note(format!("in steps of {step} A"))).collect()
                }
                _ => w,
            }
        }
        Op::Reconnect => vec![Write::new("tuya session", "reconnect")],
    })
}

/// The set current as last reported (`charge_cur_set`, DP 4 on both chargers).
fn set_current<P: Profile>(dps: &Dps) -> Option<u8> {
    P::cable(dps).set_current_a.and_then(|a| u8::try_from(a.round() as i64).ok())
}

#[async_trait]
impl<P: Profile> Actuator for TuyaCharger<P> {
    /// `CableStart` returns `Scheduled` when a current follows after `start_delay`, else `Done`.
    /// While the link is down every command but `CableReconnect` fails with `NotConnected`.
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
        let writes = self.planned(&op)?;
        let applied = match op {
            Op::Start(amps) if P::start(amps, &self.dps()).1.is_some() => Applied::Scheduled,
            _ => Applied::Done,
        };
        Ok(Outcome { applied, writes })
    }
}

/// Why a session ended.
enum Exit {
    Lost,
    Reconnect,
    Closed,
}

struct Worker<P> {
    cfg: ChargerConfig,
    key: Key,
    state: Shared,
    rx: mpsc::Receiver<(Op, Reply)>,
    /// The pending second half of a `CableStart`: when, and which current.
    pending: Option<(Instant, u8)>,
    /// A current being lowered in steps: when the next step is due, and the target.
    ramp: Option<Ramp>,
    /// The set current last written in this session (the device reports it a moment later).
    written: Option<u8>,
    profile: PhantomData<fn() -> P>,
}

#[derive(Clone, Copy)]
struct Ramp {
    at: Instant,
    target: u8,
}

type Session = (Reader<TcpStream>, Writer<TcpStream>);

impl<P: Profile> Worker<P> {
    async fn connect(&self) -> Result<Session> {
        let hs = async {
            let stream = TcpStream::connect(self.cfg.addr).await?;
            let (mut rd, mut wr) = negotiate(stream, &self.key, rand::random(), self.cfg.version).await?;
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
                Err(e) => {
                    tracing::debug!(driver = P::NAME, error = %e, "tuya: connect failed");
                    Exit::Lost
                }
            };
            {
                let mut s = self.state.lock().unwrap();
                s.connected = false;
                s.dps.clear();
            }
            self.pending = None;
            self.ramp = None;
            self.written = None;
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

    async fn set(&mut self, wr: &mut Writer<TcpStream>, dps: Dps) -> Result<()> {
        let t = chrono::Utc::now().timestamp();
        let amps = P::cable(&dps).set_current_a.and_then(|a| u8::try_from(a.round() as i64).ok());
        wr.send(cmd::CONTROL_NEW, json!({"protocol": 5, "t": t, "data": {"dps": dps}}).to_string().as_bytes()).await?;
        if amps.is_some() {
            self.written = amps;
        }
        Ok(())
    }

    async fn set_current(&mut self, wr: &mut Writer<TcpStream>, amps: u8) -> Result<()> {
        let m = P::current(amps, &self.state.lock().unwrap().dps)?;
        self.set(wr, m).await
    }

    /// Lower or raise the current towards `target`: at once, or (lowering by more than
    /// [`Profile::STEP_DOWN`]) one step now and the rest from [`Worker::step`].
    async fn current(&mut self, wr: &mut Writer<TcpStream>, target: u8) -> Result<()> {
        P::current(target, &self.state.lock().unwrap().dps)?;
        // The higher of what was written and what the device reports: the car may still draw
        // either.
        let from = self.written.max(set_current::<P>(&self.state.lock().unwrap().dps));
        self.ramp = None;
        let first = match (P::STEP_DOWN, from) {
            (Some(step), Some(from)) if from > target.saturating_add(step) => from - step,
            _ => target,
        };
        self.set_current(wr, first).await?;
        if first != target {
            self.ramp = Some(Ramp { at: Instant::now() + self.cfg.step_interval, target });
        }
        Ok(())
    }

    /// The next step of a lowered current.
    async fn step(&mut self, wr: &mut Writer<TcpStream>) {
        let Some(Ramp { target, .. }) = self.ramp.take() else { return };
        let level = self.written.unwrap_or(target);
        let next = target.max(level.saturating_sub(P::STEP_DOWN.unwrap_or(u8::MAX)));
        if let Err(e) = self.set_current(wr, next).await {
            tracing::warn!(driver = P::NAME, error = %e, amps = next, "tuya: current step failed");
        }
        if next != target {
            self.ramp = Some(Ramp { at: Instant::now() + self.cfg.step_interval, target });
        }
    }

    async fn command(&mut self, wr: &mut Writer<TcpStream>, op: Op) -> Result<Applied> {
        match op {
            Op::Start(amps) => {
                let (now, later) = P::start(amps, &self.state.lock().unwrap().dps);
                self.ramp = None;
                self.set(wr, now).await?;
                self.state.lock().unwrap().stopped = false;
                // `mode: restart`: a new start replaces a pending one.
                self.pending = later.map(|a| (Instant::now() + self.cfg.start_delay, a));
                Ok(if later.is_some() { Applied::Scheduled } else { Applied::Done })
            }
            Op::Stop => {
                self.ramp = None;
                let m = P::stop(&self.state.lock().unwrap().dps);
                self.set(wr, m).await?;
                self.state.lock().unwrap().stopped = true;
                Ok(Applied::Done)
            }
            Op::Current(amps) => self.current(wr, amps).await.map(|()| Applied::Done),
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
        // `interval` refuses a zero period; a zero refresh is never polled (see the branch below).
        let mut refresh = tokio::time::interval(self.cfg.refresh.max(Duration::from_millis(1)));
        let refreshing = !P::REFRESH.is_empty() && !self.cfg.refresh.is_zero();
        let update = json!({ "dpId": P::REFRESH }).to_string();
        let started = Instant::now();
        loop {
            let deadline = self.pending.map(|(at, _)| at);
            let step_at = self.ramp.map(|r| r.at);
            tokio::select! {
                frame = rd.recv() => {
                    let frame = match frame {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::warn!(driver = P::NAME, error = %e, after_s = started.elapsed().as_secs(), "tuya: session lost");
                            return Exit::Lost;
                        }
                    };
                    last_rx = Instant::now();
                    if let Some(dps) = wr.open(&frame).ok().as_deref().and_then(dps_of) {
                        let mut s = self.state.lock().unwrap();
                        // The device reports charging (again), whoever started it: a later end is
                        // the car's own. Only this frame's DPs count: a meter refresh between the
                        // stop and the device's new status would otherwise clear the flag.
                        if P::cable(&dps).status.as_deref() == Some("charging") {
                            s.stopped = false;
                        }
                        s.dps.extend(dps);
                    }
                }
                _ = beat.tick() => {
                    if last_rx.elapsed() > self.cfg.heartbeat * 3 {
                        tracing::warn!(driver = P::NAME, after_s = started.elapsed().as_secs(), "tuya: session lost: device silent");
                        return Exit::Lost;
                    }
                    let _ = wr.send(cmd::HEART_BEAT, heartbeat.as_bytes()).await;
                }
                _ = refresh.tick(), if refreshing => {
                    let _ = wr.send(cmd::UPDATEDPS, update.as_bytes()).await;
                }
                _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                    let (_, amps) = self.pending.take().expect("pending");
                    // HA's script fails at `number.set_value` on a bad value; the start stays.
                    let _ = self.set_current(&mut wr, amps).await;
                }
                _ = sleep_until(step_at.unwrap_or_else(Instant::now)), if step_at.is_some() => {
                    self.step(&mut wr).await;
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
pub(crate) fn dps_of(plain: &[u8]) -> Option<Dps> {
    let v: Value = serde_json::from_slice(plain).ok()?;
    v.get("dps").or_else(|| v.get("data").and_then(|d| d.get("dps"))).and_then(Value::as_object).cloned()
}

/// A one-DP map, for profiles.
pub(crate) fn dp(id: &str, v: Value) -> Dps {
    [(id.to_string(), v)].into_iter().collect()
}
