//! The native device instances: every `[[device]]` whose kind is an `energie-drivers` factory
//! (see [`crate::plan`] and [`crate::registry`]), behind the [`DriverFactory`] trait.
//!
//! Every instance gets one tokio task that owns the driver: it polls [`Source::read`] every
//! `poll_ms` and, for a writable instance, carries out the queued commands with
//! [`DriverActuator::apply`] in between (the drivers take `&mut self`, so reads and writes of
//! one device never overlap). The tick only ever touches the task's cache:
//!
//! - [`WorldSource::read`] clones the cached World section, never blocks on the device.
//! - [`Actuator::submit`] pushes into a bounded queue (16); a full queue is an error, not a wait.
//! - [`DeviceView::set`] (an entity the driver marks settable, such as the EV3's AC charging
//!   limit) goes through the same queue to the driver's `set`.
//! - [`DeviceView::set_credentials`] (a driver that signs in with a password, [`Source::auth`])
//!   has a queue of its own, in every mode: new credentials are not a command to the house.
//!   After every poll and every credentials change the task stores the driver's sign-in state
//!   in its health, and the device gets an `auth` entity with it.
//!
//! A failed read keeps the last good reading through `grace_polls` consecutive failures, then
//! writes the driver's HA-like `unavailable` reading (every field `None`) until a read succeeds.
//! After every good read the task also stores the driver's extra entities
//! ([`energie_drivers::Source::entities`]) and forecasts ([`energie_drivers::Source::forecasts`]).
//!
//! Safety: an actuator exists only in `control`/`simulate` mode, for an instance with a
//! `control` binding and `read_only = false` (the default is `true`); entities can be set under
//! the same conditions minus the binding. In shadow mode no command channel is even created.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use energie_core::command::Command;
use energie_core::time::{TZ, Ts};
use energie_core::world::World;
use energie_drivers::entity::{Entity, Value};
use energie_drivers::plugin::{BuildCx, Driver, Slot};
use energie_drivers::{Applied, AuthStatus, DriverError, Outcome, Reading, Source};
use serde_json::{Map, Value as Json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::{Config, Mode};
use crate::control::Ticket;
use crate::error::{Error, Result};
use crate::plan::Instance;
use crate::plug::{Actuator, DeviceView, DriverFactory, DriverHealth, WorldSource};
use crate::registry::Registry;

/// Commands queued per device before `submit` refuses.
const QUEUE: usize = 16;

/// What the device task carries out between polls.
#[derive(Debug, Clone, PartialEq)]
enum Job {
    Command(Command),
    /// A control-mode command: carried out (or with `dry`, only planned) and reported on its
    /// ticket. A real one is dropped when its device was switched off since it was queued.
    Tracked(Command, bool, Ticket),
    /// Set one of the driver's own entities.
    Set(String, Value),
}

/// New credentials for a driver, and where its answer goes.
type CredentialsJob = (Map<String, Json>, oneshot::Sender<std::result::Result<AuthStatus, String>>);

/// The answer to a credentials change.
pub type CredentialsReply = oneshot::Receiver<std::result::Result<AuthStatus, String>>;

/// Construct the driver of `inst`; errors name the instance.
fn build(inst: &Instance, state_dir: &std::path::Path) -> Result<Box<dyn Driver>> {
    let f = Registry::builtin()
        .factory(&inst.kind)
        .ok_or_else(|| Error::new(format!("{}: kind {} has no driver", inst.label, inst.kind)))?;
    f.build(&inst.table, &BuildCx { id: &inst.id, slots: &inst.reads, state_dir: Some(state_dir) })
        .map_err(|e| Error::new(format!("{}: {e}", inst.label)))
}

fn wall_now() -> Ts {
    chrono::Utc::now().with_timezone(&TZ)
}

/// What a device task shares with its source and actuator handles.
struct State {
    world: World,
    health: DriverHealth,
    grace: u32,
    /// The driver's extra entities from the last good read (`null` while unavailable).
    extra: Vec<Entity>,
    /// The driver's forecasts from the last good read (none while unavailable).
    forecasts: Vec<(String, Vec<(i64, f64)>)>,
}

impl State {
    fn new(grace: u32, writable: bool) -> Self {
        State {
            world: World::default(),
            health: DriverHealth { writable, ..Default::default() },
            grace,
            extra: Vec::new(),
            forecasts: Vec::new(),
        }
    }

    /// Record one poll: a reading replaces the cache; an error keeps the last reading through
    /// `grace` consecutive failures and then writes `unavailable`.
    fn on_read(&mut self, name: &str, r: std::result::Result<Reading, DriverError>, unavailable: Reading, at: Ts) {
        let h = &mut self.health;
        match r {
            Ok(reading) => {
                if h.consecutive_errors > 0 {
                    tracing::info!(driver = name, after = h.consecutive_errors, "driver reads again");
                }
                reading.apply_to(&mut self.world);
                h.available = true;
                h.consecutive_errors = 0;
                h.reads_ok += 1;
                h.last_ok = Some(at.to_rfc3339());
            }
            Err(e) => {
                h.read_errors += 1;
                h.consecutive_errors += 1;
                h.last_error = Some(e.to_string());
                if h.consecutive_errors > self.grace {
                    if h.consecutive_errors == self.grace + 1 {
                        tracing::warn!(driver = name, error = %e, "driver unavailable");
                    }
                    unavailable.apply_to(&mut self.world);
                    h.available = false;
                } else {
                    tracing::warn!(driver = name, error = %e, failures = h.consecutive_errors, "read failed; keeping the last reading");
                }
            }
        }
    }

    /// Record one command's outcome.
    fn on_apply(&mut self, name: &str, cmd: &Job, r: std::result::Result<Applied, DriverError>) {
        let h = &mut self.health;
        let err = match r {
            Ok(Applied::NotMine) => "not a command for this device".to_string(),
            Ok(applied) => {
                tracing::info!(driver = name, ?cmd, ?applied, "command applied");
                h.commands_applied += 1;
                return;
            }
            Err(e) => e.to_string(),
        };
        tracing::warn!(driver = name, ?cmd, error = %err, "command failed");
        h.command_errors += 1;
        h.last_command_error = Some(err);
    }
}

type Shared = Arc<Mutex<State>>;

/// The next queued item; never resolves without a queue.
async fn next<T>(rx: &mut Option<mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// The device task: poll every `poll`, apply queued commands and credentials in between.
async fn run(
    mut drv: Box<dyn Driver>,
    poll: Duration,
    state: Shared,
    mut rx: Option<mpsc::Receiver<Job>>,
    mut creds: Option<mpsc::Receiver<CredentialsJob>>,
) {
    let mut iv = tokio::time::interval(poll);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = iv.tick() => {
                let r = drv.read().await;
                let ok = r.is_ok();
                let unavailable = drv.unavailable();
                let mut st = state.lock().expect("driver state");
                st.on_read(drv.name(), r, unavailable, wall_now());
                st.health.auth = drv.auth();
                if ok {
                    st.extra = drv.entities();
                    st.forecasts = drv.forecasts();
                } else if !st.health.available {
                    st.extra.iter_mut().for_each(|e| e.value = Value::Null);
                    st.forecasts.clear();
                }
            }
            // A closed queue yields `None`: the branch is skipped and polling goes on.
            Some(job) = next(&mut rx) => {
                let r = match &job {
                    Job::Command(cmd) => drv.apply(cmd).await,
                    Job::Tracked(cmd, true, ticket) => {
                        ticket.finish(drv.plan(cmd).await.map_err(|e| e.to_string()));
                        continue;
                    }
                    Job::Tracked(_, false, ticket) if !ticket.allowed() => {
                        ticket.finish(Err("blocked: control was switched off before it was sent".into()));
                        continue;
                    }
                    Job::Tracked(cmd, false, ticket) => {
                        let r = drv.execute(cmd).await;
                        ticket.finish(r.as_ref().map(Outcome::clone).map_err(|e| e.to_string()));
                        r.map(|o| o.applied)
                    }
                    Job::Set(key, value) => drv.set(key, value).await,
                };
                state.lock().expect("driver state").on_apply(Source::name(drv.as_ref()), &job, r);
            }
            Some((fields, reply)) = next(&mut creds) => {
                // The fields hold a password: they are never logged.
                let r = drv.set_credentials(&fields).await;
                match &r {
                    Ok(a) => tracing::info!(driver = drv.name(), state = a.state, "new credentials"),
                    Err(e) => tracing::warn!(driver = drv.name(), error = %e, "credentials refused"),
                }
                state.lock().expect("driver state").health.auth = drv.auth();
                let _ = reply.send(r.map_err(|e| e.to_string()));
            }
        }
    }
}

/// The cached readings of one instance; also its [`DeviceView`].
pub struct DriverSource {
    name: String,
    id: String,
    kind: String,
    slots: Vec<Slot>,
    state: Shared,
    /// The job queue, when the instance may be written to.
    tx: Option<mpsc::Sender<Job>>,
    /// The credentials queue, for a driver that signs in ([`Source::auth`]).
    creds: Option<mpsc::Sender<CredentialsJob>>,
}

/// Queue `job`; a full or closed queue counts as a command error.
fn queue(name: &str, tx: &mpsc::Sender<Job>, state: &Shared, job: Job) -> Result<()> {
    tx.try_send(job).map_err(|e| {
        let why = match e {
            mpsc::error::TrySendError::Full(_) => "command queue full",
            mpsc::error::TrySendError::Closed(_) => "driver task stopped",
        };
        let mut st = state.lock().expect("driver state");
        st.health.command_errors += 1;
        st.health.last_command_error = Some(why.to_string());
        Error::new(format!("{name}: {why}"))
    })
}

impl WorldSource for DriverSource {
    fn name(&self) -> &str {
        &self.name
    }
    fn read(&self, _now: Ts) -> World {
        self.state.lock().expect("driver state").world.clone()
    }
    fn health(&self) -> Option<DriverHealth> {
        Some(self.state.lock().expect("driver state").health.clone())
    }
}

impl DeviceView for DriverSource {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &str {
        &self.kind
    }
    fn slots(&self) -> &[Slot] {
        &self.slots
    }
    fn world(&self) -> World {
        self.state.lock().expect("driver state").world.clone()
    }
    /// The driver's entities, plus `auth` (its sign-in state) for one that signs in.
    fn extra(&self) -> Vec<Entity> {
        let st = self.state.lock().expect("driver state");
        let mut out = st.extra.clone();
        if let Some(a) = &st.health.auth {
            out.push(Entity::sensor("auth", "Aanmelding", Value::Text(a.state.clone())));
        }
        out
    }
    fn health(&self) -> DriverHealth {
        self.state.lock().expect("driver state").health.clone()
    }
    fn forecasts(&self) -> Vec<(String, Vec<(i64, f64)>)> {
        self.state.lock().expect("driver state").forecasts.clone()
    }
    fn settable(&self) -> bool {
        self.tx.is_some()
    }
    fn set(&self, key: &str, value: &Value) -> Result<()> {
        let tx = self.tx.as_ref().ok_or_else(|| Error::new(format!("{}: read only", self.name)))?;
        queue(&self.name, tx, &self.state, Job::Set(key.to_string(), value.clone()))
    }
    fn set_credentials(&self, fields: Map<String, Json>) -> Result<CredentialsReply> {
        let tx = self.creds.as_ref().ok_or_else(|| Error::new(format!("{}: takes no credentials", self.id)))?;
        let (reply, rx) = oneshot::channel();
        tx.try_send((fields, reply)).map_err(|_| Error::new(format!("{}: busy, try again", self.id)))?;
        Ok(rx)
    }
}

/// The command queue of one writable instance.
pub struct DriverActuator {
    name: String,
    tx: mpsc::Sender<Job>,
    state: Shared,
}

impl Actuator for DriverActuator {
    fn name(&self) -> &str {
        &self.name
    }
    fn submit(&self, _now: Ts, cmd: &Command) -> Result<()> {
        queue(&self.name, &self.tx, &self.state, Job::Command(cmd.clone()))
    }
    fn submit_tracked(&self, _now: Ts, cmd: &Command, dry: bool, ticket: Ticket) -> Result<()> {
        queue(&self.name, &self.tx, &self.state, Job::Tracked(cmd.clone(), dry, ticket))
    }
    fn health(&self) -> Option<DriverHealth> {
        Some(self.state.lock().expect("driver state").health.clone())
    }
}

/// The real [`DriverFactory`]: one task per native instance. Dropping it stops the tasks.
pub struct Drivers {
    sources: BTreeMap<String, Arc<DriverSource>>,
    actuators: BTreeMap<String, Arc<DriverActuator>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Drivers {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Drivers {
    /// Build and start the native instances of the config. Must run inside a tokio runtime
    /// when there are any. Fails on a bad binding, a missing secret file or a bad driver config.
    pub fn new(cfg: &Config) -> Result<Drivers> {
        let mut out = Drivers { sources: Default::default(), actuators: Default::default(), tasks: Vec::new() };
        for inst in cfg.plan()?.devices {
            let wants_write = cfg.mode != Mode::Shadow && !inst.controls.is_empty();
            let writable = wants_write && !inst.read_only;
            if wants_write && !writable {
                tracing::warn!(device = inst.id, "read_only: no actuator");
            }
            let settable = cfg.mode != Mode::Shadow && !inst.read_only;
            let drv = build(&inst, &cfg.state_dir)?;
            let mut state = State::new(inst.grace_polls, writable);
            state.health.auth = drv.auth();
            let (creds_tx, creds_rx) = match state.health.auth {
                Some(_) => {
                    let (tx, rx) = mpsc::channel(1);
                    (Some(tx), Some(rx))
                }
                None => (None, None),
            };
            let state: Shared = Arc::new(Mutex::new(state));
            let name = format!("driver:{}", inst.id);
            let (tx, rx) = if settable {
                let (tx, rx) = mpsc::channel(QUEUE);
                (Some(tx), Some(rx))
            } else {
                (None, None)
            };
            if let Some(tx) = tx.clone().filter(|_| writable) {
                let act = DriverActuator { name: name.clone(), tx, state: state.clone() };
                out.actuators.insert(inst.id.clone(), Arc::new(act));
            }
            let src = DriverSource {
                name,
                id: inst.id.clone(),
                kind: inst.kind.clone(),
                slots: inst.reads.clone(),
                state: state.clone(),
                tx,
                creds: creds_tx,
            };
            out.sources.insert(inst.id.clone(), Arc::new(src));
            out.tasks.push(tokio::spawn(run(drv, Duration::from_millis(inst.poll_ms), state, rx, creds_rx)));
        }
        Ok(out)
    }

    /// How many actuators were built (0 in shadow mode).
    pub fn actuator_count(&self) -> usize {
        self.actuators.len()
    }
}

impl DriverFactory for Drivers {
    /// Only for an instance that reads something (a control-only one still polls, for health).
    fn source(&self, id: &str) -> Option<Arc<dyn WorldSource>> {
        self.sources.get(id).filter(|s| !s.slots.is_empty()).map(|s| s.clone() as Arc<dyn WorldSource>)
    }
    fn actuator(&self, id: &str) -> Option<Arc<dyn Actuator>> {
        self.actuators.get(id).map(|a| a.clone() as Arc<dyn Actuator>)
    }
    fn devices(&self) -> Vec<Arc<dyn DeviceView>> {
        self.sources.values().map(|s| s.clone() as Arc<dyn DeviceView>).collect()
    }
}

#[cfg(test)]
mod tests;
