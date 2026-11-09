//! Control mode, carried out safely: the gate between the engine's commands and the devices.
//!
//! The engine decides as before; this only changes how its commands reach the devices
//! (`docs/CONTROL.md`):
//!
//! - **Kill switch** (`control_enabled`) and **per-device enable** (`control_<device>`), from the
//!   dashboard, `POST /api/control` or the entities; persisted in `<state_dir>/control.json`, so
//!   a stop survives a restart. A blocked command is shown on the dashboard, never sent; a job
//!   already queued for a device is dropped by the device task when it gets to it.
//! - **Start-up**: no command for a device before its first good reading (the app also waits
//!   for those before the first tick), and a per-device rate limit (`max_commands_per_min`, a
//!   token bucket) as a backstop well above what the automations send.
//! - **Dry run** (`dry_run = true`): every command is worked out by its driver against the
//!   device's real state (register, DP or topic, value, what it replaces) and logged, nothing
//!   is written.
//! - **Audit**: every real command with the driver's wire writes, the World value before, the
//!   value it must read back ([`verify`]) and what was read; a write that does not show within
//!   `verify_s` raises a notification. JSONL, rotated, in `<state_dir>/control-audit.jsonl`
//!   (dry run: `control-dryrun.jsonl`), and the latest 50 on the dashboard.

pub mod verify;

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use energie_core::command::Command;
use energie_core::time::{Ts, secs};
use energie_core::world::World;
use energie_drivers::entity::{Control as EntityControl, Entity, Value};
use energie_drivers::{Applied, Outcome, Write};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::plug::{Device, Router};
use crate::shadow::log::MismatchLog;
use verify::Expect;

const KEEP_RECENT: usize = 50;

fn yes() -> bool {
    true
}

/// `[control]`: how control mode carries out commands. Ignored outside control mode.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ControlConfig {
    /// Actuation on at the first start (later the persisted switch wins).
    pub enabled: bool,
    /// Work out and log every write, send none.
    pub dry_run: bool,
    /// Per-device enable at the first start (default: all on).
    pub devices: BTreeMap<Device, bool>,
    /// A command's read-back must show within this many seconds.
    pub verify_s: u64,
    /// Per device: at most this many commands per minute (burst and rate).
    pub max_commands_per_min: u32,
    /// Wait at most this long at start-up for the first reading of every controlled device.
    pub startup_wait_s: u64,
    /// At a clean shutdown: `solis` selects RC mode `None` (the inverter returns to self-use at
    /// once instead of after its RC timeout), `evse` publishes override 0 (the charger's own
    /// current), `cable` stops the cable.
    pub release_on_shutdown: Vec<Device>,
    /// The audit log; default `<state_dir>/control-audit.jsonl`.
    pub audit_file: Option<PathBuf>,
    /// The dry-run log; default `<state_dir>/control-dryrun.jsonl`.
    pub dry_run_file: Option<PathBuf>,
    /// Rotate a log (one `.1` generation) past this size.
    pub log_max_bytes: u64,
    /// At most one alarm per field (or device) in this many seconds.
    pub alarm_interval_s: u64,
    /// Raise a notification when a write does not read back.
    #[serde(default = "yes")]
    pub alarms: bool,
}

impl Default for ControlConfig {
    fn default() -> Self {
        ControlConfig {
            enabled: true,
            dry_run: false,
            devices: BTreeMap::new(),
            verify_s: 60,
            max_commands_per_min: 30,
            startup_wait_s: 60,
            release_on_shutdown: vec![Device::Solis],
            audit_file: None,
            dry_run_file: None,
            log_max_bytes: 4 * 1024 * 1024,
            alarm_interval_s: 1800,
            alarms: true,
        }
    }
}

impl ControlConfig {
    pub fn check(&self) -> Result<()> {
        if self.verify_s == 0 || self.max_commands_per_min == 0 {
            return Err(Error::new("control: verify_s and max_commands_per_min must be > 0"));
        }
        Ok(())
    }

    /// The JSONL file this mode writes to.
    pub fn log_path(&self, state_dir: &std::path::Path) -> PathBuf {
        match self.dry_run {
            true => self.dry_run_file.clone().unwrap_or_else(|| state_dir.join("control-dryrun.jsonl")),
            false => self.audit_file.clone().unwrap_or_else(|| state_dir.join("control-audit.jsonl")),
        }
    }

    /// The release command of `d` at shutdown.
    fn release(d: Device) -> Option<Command> {
        match d {
            Device::Solis => Some(Command::SolisRcMode(energie_core::world::RcMode::None)),
            Device::Evse => Some(Command::EvseCurrentDeciA(0)),
            Device::Cable => Some(Command::CableStop),
            Device::Airco | Device::Prices => None,
        }
    }
}

/// The switches, as persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Flags {
    pub enabled: bool,
    pub devices: BTreeMap<Device, bool>,
}

/// What a device task reports back for one command.
type Report = std::result::Result<Outcome, String>;

/// The handle a tracked command carries to its device task: whether it may still be sent, and
/// where the outcome goes.
#[derive(Clone)]
pub struct Ticket {
    allowed: Arc<AtomicBool>,
    report: Arc<Mutex<Option<Report>>>,
}

impl std::fmt::Debug for Ticket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ticket").field("allowed", &self.allowed()).field("done", &self.done()).finish()
    }
}

impl PartialEq for Ticket {
    fn eq(&self, o: &Self) -> bool {
        Arc::ptr_eq(&self.report, &o.report)
    }
}

impl Ticket {
    pub fn new(allowed: Arc<AtomicBool>) -> Self {
        Ticket { allowed, report: Arc::default() }
    }

    /// Actuation of its device is still on (checked by the device task right before it writes).
    pub fn allowed(&self) -> bool {
        self.allowed.load(Ordering::SeqCst)
    }

    /// Report the outcome (the first report counts).
    pub fn finish(&self, r: Report) {
        self.report.lock().expect("ticket lock").get_or_insert(r);
    }

    pub fn done(&self) -> bool {
        self.report.lock().expect("ticket lock").is_some()
    }

    fn take(&self) -> Option<Report> {
        self.report.lock().expect("ticket lock").clone()
    }
}

/// How an audited command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The device reads back the commanded value.
    Verified,
    /// Sent; nothing in the World shows it (cable stop, reconnects, price refresh).
    Sent,
    /// The driver needed no write (the device already had the value) and it reads back.
    Unchanged,
    /// A later command for the same field with another value came first.
    Superseded,
    /// Sent, but the read-back did not show it within `verify_s`.
    Failed,
    /// The driver or the queue refused it.
    Error,
    /// Dry run: what would have been written.
    DryRun,
}

/// One line of the audit (or dry-run) log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditEntry {
    pub at: String,
    pub device: Device,
    pub actuator: String,
    pub dry_run: bool,
    pub command: Command,
    /// The wire writes, as the driver reports them.
    pub writes: Vec<Write>,
    pub applied: Option<Applied>,
    /// The World field that must show it, its value before, wanted and read back.
    pub field: Option<String>,
    pub before: Option<Value>,
    pub want: Option<Value>,
    pub after: Option<Value>,
    pub result: Verdict,
    pub detail: Option<String>,
    pub done_at: String,
}

struct Pending {
    at: Ts,
    device: Device,
    actuator: String,
    cmd: Command,
    ticket: Ticket,
    expect: Option<Expect>,
    before: Option<Value>,
    deadline: Ts,
    outcome: Option<Outcome>,
    superseded: bool,
}

/// Counters for the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ControlStats {
    pub sent: u64,
    pub blocked: u64,
    pub verified: u64,
    pub failed: u64,
    pub errors: u64,
    pub dry_run: u64,
    pub log_errors: u64,
}

/// What the dashboard and `/api/control` show.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ControlView {
    pub enabled: bool,
    pub dry_run: bool,
    pub devices: BTreeMap<Device, bool>,
    pub pending: usize,
    pub stats: ControlStats,
    pub log_file: String,
    /// Newest first.
    pub recent: Vec<AuditEntry>,
}

/// An alarm to raise: title and message.
pub type Alarm = (String, String);

/// The gate, the audit and the read-back check.
pub struct Control {
    cfg: ControlConfig,
    flags: Flags,
    flags_file: PathBuf,
    live: BTreeMap<Device, Arc<AtomicBool>>,
    /// Tokens left and when they were counted, per device.
    buckets: BTreeMap<Device, (f64, Ts)>,
    pending: Vec<Pending>,
    recent: VecDeque<AuditEntry>,
    log: MismatchLog,
    log_path: PathBuf,
    alarmed: BTreeMap<String, Ts>,
    pub stats: ControlStats,
}

impl Control {
    /// The gate for `cfg`, with the switches from `<state_dir>/control.json` when present.
    pub fn new(cfg: ControlConfig, state_dir: &std::path::Path) -> Control {
        let flags_file = state_dir.join("control.json");
        let initial = Flags {
            enabled: cfg.enabled,
            devices: Device::ALL.iter().map(|d| (*d, cfg.devices.get(d).copied().unwrap_or(true))).collect(),
        };
        let flags = std::fs::read_to_string(&flags_file)
            .ok()
            .and_then(|t| serde_json::from_str::<Flags>(&t).ok())
            .map(|f| Flags { devices: initial.devices.clone().into_iter().chain(f.devices).collect(), ..f })
            .unwrap_or(initial);
        let log_path = cfg.log_path(state_dir);
        let c = Control {
            log: MismatchLog::new(Some(log_path.clone()), cfg.log_max_bytes),
            log_path,
            flags,
            flags_file,
            live: Device::ALL.iter().map(|d| (*d, Arc::new(AtomicBool::new(false)))).collect(),
            buckets: BTreeMap::new(),
            pending: Vec::new(),
            recent: VecDeque::new(),
            alarmed: BTreeMap::new(),
            stats: ControlStats::default(),
            cfg,
        };
        c.sync_live();
        if c.flags.enabled != c.cfg.enabled {
            tracing::warn!(enabled = c.flags.enabled, "control switch taken from control.json, not the config");
        }
        c
    }

    /// Start switched off, whatever was configured or saved (`require_state`): the state this
    /// controller works from is gone, so someone has to look before it writes to anything.
    pub fn off_until_enabled(&mut self) {
        self.flags.enabled = false;
        self.sync_live();
        tracing::warn!("no saved state and require_state is set: control is off until switched on");
    }

    pub fn config(&self) -> &ControlConfig {
        &self.cfg
    }

    pub fn dry_run(&self) -> bool {
        self.cfg.dry_run
    }

    fn sync_live(&self) {
        for (d, a) in &self.live {
            a.store(self.flags.enabled && self.flags.devices[d], Ordering::SeqCst);
        }
    }

    pub fn flags(&self) -> &Flags {
        &self.flags
    }

    /// Switch actuation on or off, for everything (`None`) or one device; persisted at once.
    pub fn set(&mut self, device: Option<Device>, on: bool) {
        match device {
            None => self.flags.enabled = on,
            Some(d) => {
                self.flags.devices.insert(d, on);
            }
        }
        self.sync_live();
        tracing::warn!(?device, on, "control switched");
        let body = serde_json::to_string_pretty(&self.flags).expect("flags serialise");
        if let Err(e) = std::fs::write(&self.flags_file, body) {
            tracing::error!(path = %self.flags_file.display(), error = %e, "persisting the control switch failed");
        }
    }

    /// Take a token for `d` at `now`; false when its budget is used up.
    fn token(&mut self, d: Device, now: Ts) -> bool {
        let cap = f64::from(self.cfg.max_commands_per_min);
        let (left, then) = self.buckets.get(&d).copied().unwrap_or((cap, now));
        let left = (left + secs(then, now).max(0.0) * cap / 60.0).min(cap);
        let ok = left >= 1.0;
        self.buckets.insert(d, (if ok { left - 1.0 } else { left }, now));
        ok
    }

    /// Why `cmd` may not go out now, if it may not.
    fn refusal(&mut self, d: Device, now: Ts, router: &Router, limit: bool) -> Option<String> {
        if !self.flags.enabled {
            return Some("blocked: control is switched off".into());
        }
        if !self.flags.devices[&d] {
            return Some(format!("blocked: control of {} is switched off", d.name()));
        }
        if router.device_health(d).is_some_and(|h| h.reads_ok == 0) {
            return Some(format!("blocked: no reading of {} yet", d.name()));
        }
        if limit && !self.token(d, now) {
            return Some(format!(
                "blocked: more than {} commands a minute for {}",
                self.cfg.max_commands_per_min,
                d.name()
            ));
        }
        None
    }

    /// Send `cmd` (a device command) through the gate. `world` is the latest World, for the
    /// value before. An `Err` is why it was not sent.
    pub fn dispatch(
        &mut self,
        now: Ts,
        cmd: &Command,
        world: &World,
        router: &Router,
    ) -> std::result::Result<(), String> {
        self.send(now, cmd, world, router, true)
    }

    fn send(
        &mut self,
        now: Ts,
        cmd: &Command,
        world: &World,
        router: &Router,
        limit: bool,
    ) -> std::result::Result<(), String> {
        let d = Device::of(cmd).ok_or("notifications go to the notifier")?;
        if let Some(why) = self.refusal(d, now, router, limit) {
            self.stats.blocked += 1;
            tracing::debug!(?cmd, why, "command not sent");
            return Err(why);
        }
        let ticket = Ticket::new(self.live[&d].clone());
        router.route_tracked(now, cmd, self.cfg.dry_run, ticket.clone()).map_err(|e| {
            self.stats.errors += 1;
            e.0
        })?;
        let expect = verify::expect(cmd).filter(|_| !self.cfg.dry_run);
        let mut deadline = now + chrono::Duration::seconds(self.cfg.verify_s as i64);
        if let Some(x) = &expect {
            for p in
                self.pending.iter_mut().filter(|p| p.expect.as_ref().is_some_and(|e| std::ptr::eq(e.field, x.field)))
            {
                if p.expect.as_ref() == Some(x) {
                    // The same value again: the first deadline still counts.
                    deadline = deadline.min(p.deadline);
                } else {
                    p.superseded = true;
                }
            }
        }
        self.pending.push(Pending {
            at: now,
            device: d,
            actuator: router.name(d),
            cmd: cmd.clone(),
            ticket,
            before: expect.as_ref().map(|x| x.read(world)),
            expect,
            deadline,
            outcome: None,
            superseded: false,
        });
        Ok(())
    }

    /// The release commands of `release_on_shutdown`, sent past the rate limit (the switches
    /// still apply).
    pub fn release(&mut self, now: Ts, world: &World, router: &Router) {
        for d in self.cfg.release_on_shutdown.clone() {
            if let Some(cmd) = ControlConfig::release(d) {
                match self.send(now, &cmd, world, router, false) {
                    Ok(()) => tracing::info!(?cmd, "released at shutdown"),
                    Err(e) => tracing::warn!(?cmd, error = %e, "release at shutdown not sent"),
                }
            }
        }
    }

    /// Commands still waiting for their outcome or read-back.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Commands whose device task has not reported yet.
    pub fn unreported(&self) -> usize {
        self.pending.iter().filter(|p| p.outcome.is_none() && !p.ticket.done()).count()
    }

    fn alarm(&mut self, key: String, now: Ts, title: &str, message: String) -> Option<Alarm> {
        if !self.cfg.alarms {
            return None;
        }
        let quiet = self.alarmed.get(&key).is_some_and(|t| secs(*t, now) < self.cfg.alarm_interval_s as f64);
        if quiet {
            return None;
        }
        self.alarmed.insert(key, now);
        Some((title.to_string(), message))
    }

    /// Collect outcomes, check read-backs against `world`, write the finished entries and
    /// return the alarms to raise.
    pub fn poll(&mut self, now: Ts, world: &World) -> Vec<Alarm> {
        let mut alarms = Vec::new();
        let mut keep = Vec::new();
        for mut p in std::mem::take(&mut self.pending) {
            if p.outcome.is_none() {
                match p.ticket.take() {
                    Some(Ok(o)) => p.outcome = Some(o),
                    Some(Err(e)) => {
                        let d = p.device;
                        let entry = self.entry(&p, now, Verdict::Error, None, Some(e.clone()));
                        self.finish(entry);
                        let msg = format!("{:?} naar {}: {e}", p.cmd, d.name());
                        alarms.extend(self.alarm(format!("error:{}", d.name()), now, "Sturing: commando mislukt", msg));
                        continue;
                    }
                    None if secs(p.at, now) < self.cfg.verify_s as f64 => {
                        keep.push(p);
                        continue;
                    }
                    None => {
                        let entry =
                            self.entry(&p, now, Verdict::Error, None, Some("no outcome from the device task".into()));
                        self.finish(entry);
                        continue;
                    }
                }
            }
            let applied = p.outcome.as_ref().map(|o| o.applied);
            let verdict = match &p.expect {
                _ if self.cfg.dry_run => Some(Verdict::DryRun),
                None => Some(Verdict::Sent),
                Some(x) if x.holds(world) => {
                    Some(if applied == Some(Applied::Unchanged) { Verdict::Unchanged } else { Verdict::Verified })
                }
                Some(_) if p.superseded => Some(Verdict::Superseded),
                Some(_) if now >= p.deadline => Some(Verdict::Failed),
                Some(_) => None,
            };
            let Some(verdict) = verdict else {
                keep.push(p);
                continue;
            };
            let after = p.expect.as_ref().map(|x| x.read(world));
            if verdict == Verdict::Failed {
                let x = p.expect.as_ref().expect("failed needs an expectation");
                let msg = format!(
                    "{:?} naar {}: {} toont {} in plaats van {} na {} s",
                    p.cmd,
                    p.device.name(),
                    x.key(),
                    serde_json::to_string(&after).expect("values serialise"),
                    serde_json::to_string(&x.want).expect("values serialise"),
                    self.cfg.verify_s
                );
                alarms.extend(self.alarm(format!("verify:{}", x.key()), now, "Sturing: schrijven niet gelukt", msg));
            }
            let entry = self.entry(&p, now, verdict, after, None);
            self.finish(entry);
        }
        self.pending = keep;
        alarms
    }

    /// Shutdown: what can still be decided is, the rest is written as it stands (sent and not
    /// read back, or without an outcome).
    pub fn close(&mut self, now: Ts, world: &World) -> Vec<Alarm> {
        let alarms = self.poll(now, world);
        for mut p in std::mem::take(&mut self.pending) {
            if p.outcome.is_none() {
                p.outcome = p.ticket.take().and_then(|r| r.ok());
            }
            let (verdict, detail) = match p.outcome {
                Some(_) => (Verdict::Sent, "stopped before the read-back"),
                None => (Verdict::Error, "stopped before the device task reported"),
            };
            let after = p.expect.as_ref().map(|x| x.read(world));
            let entry = self.entry(&p, now, verdict, after, Some(detail.into()));
            self.finish(entry);
        }
        alarms
    }

    fn entry(&self, p: &Pending, now: Ts, result: Verdict, after: Option<Value>, detail: Option<String>) -> AuditEntry {
        AuditEntry {
            at: p.at.to_rfc3339(),
            device: p.device,
            actuator: p.actuator.clone(),
            dry_run: self.cfg.dry_run,
            command: p.cmd.clone(),
            writes: p.outcome.as_ref().map(|o| o.writes.clone()).unwrap_or_default(),
            applied: p.outcome.as_ref().map(|o| o.applied),
            field: p.expect.as_ref().map(Expect::key),
            before: p.before.clone(),
            want: p.expect.as_ref().map(|x| x.want.clone()),
            after,
            result,
            detail,
            done_at: now.to_rfc3339(),
        }
    }

    fn finish(&mut self, e: AuditEntry) {
        let s = &mut self.stats;
        match e.result {
            Verdict::Verified | Verdict::Unchanged => s.verified += 1,
            Verdict::Failed => s.failed += 1,
            Verdict::Error => s.errors += 1,
            Verdict::DryRun => s.dry_run += 1,
            Verdict::Sent | Verdict::Superseded => {}
        }
        if !matches!(e.result, Verdict::Error | Verdict::DryRun) {
            s.sent += 1;
        }
        let errors = self.log.errors;
        self.log.write(&serde_json::to_value(&e).expect("audit entries serialise"));
        self.stats.log_errors += self.log.errors - errors;
        self.recent.push_back(e);
        while self.recent.len() > KEEP_RECENT {
            self.recent.pop_front();
        }
    }

    pub fn view(&self) -> ControlView {
        ControlView {
            enabled: self.flags.enabled,
            dry_run: self.cfg.dry_run,
            devices: self.flags.devices.clone(),
            pending: self.pending.len(),
            stats: self.stats.clone(),
            log_file: self.log_path.display().to_string(),
            recent: self.recent.iter().rev().cloned().collect(),
        }
    }

    /// The switches as entities (group `control`): `control_enabled`, `control_<device>`, and
    /// the read-only `control_dry_run`.
    pub fn entities(&self) -> Vec<Entity> {
        let switch = |key: &str, name: String, on: bool| {
            let mut e = Entity::sensor(key, name, Value::Bool(on));
            e.control = Some(EntityControl::Switch);
            e.in_group("control")
        };
        let mut out = vec![
            switch("enabled", "Sturing aan".into(), self.flags.enabled),
            Entity::sensor("dry_run", "Proefdraaien (dry run)", Value::Bool(self.cfg.dry_run)).in_group("control"),
        ];
        for (d, on) in &self.flags.devices {
            out.push(switch(d.name(), format!("Sturing {}", d.name()), *on));
        }
        out
    }

    /// The switch an entity key stands for: `enabled` (all) or a device name.
    pub fn switch_of(key: &str) -> Option<Option<Device>> {
        if key == "enabled" {
            return Some(None);
        }
        Device::ALL.into_iter().find(|d| d.name() == key).map(Some)
    }
}

/// Whether the first decision waits for this device's first reading. Not for the aircos: a slow
/// or failing cloud must not hold up the battery and the charger, and their commands are refused
/// on their own until they have a reading.
fn start_needs(d: Device) -> bool {
    d != Device::Airco
}

/// Wait until every device the start needs has had a good reading, or `limit` passes.
/// Returns the devices still without one.
pub async fn wait_ready(router: &Router, limit: std::time::Duration) -> Vec<Device> {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let waiting: Vec<Device> = Device::ALL
            .into_iter()
            .filter(|d| start_needs(*d) && router.device_health(*d).is_some_and(|h| h.reads_ok == 0))
            .collect();
        if waiting.is_empty() || tokio::time::Instant::now() >= deadline {
            return waiting;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod e2e;
#[cfg(test)]
mod live;
#[cfg(test)]
mod tests;
