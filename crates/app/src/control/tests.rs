use std::sync::Mutex;

use chrono::Duration;
use energie_core::world::{EvseMode, RcMode};

use super::*;
use crate::plug::tests::t0;
use crate::plug::{Actuator, DriverHealth, LogActuator};

/// An actuator that records what it got and reports a fixed outcome (or none).
struct Fake {
    report: Mutex<Option<Report>>,
    health: Option<DriverHealth>,
    got: Mutex<Vec<(Command, bool)>>,
}

impl Fake {
    fn new(report: Option<Report>) -> Arc<Fake> {
        Arc::new(Fake { report: Mutex::new(report), health: None, got: Mutex::default() })
    }
    fn got(&self) -> Vec<(Command, bool)> {
        self.got.lock().unwrap().clone()
    }
}

impl Actuator for Fake {
    fn name(&self) -> &str {
        "fake"
    }
    fn submit(&self, _: Ts, _: &Command) -> crate::error::Result<()> {
        Ok(())
    }
    fn submit_tracked(&self, _: Ts, cmd: &Command, dry: bool, t: Ticket) -> crate::error::Result<()> {
        self.got.lock().unwrap().push((cmd.clone(), dry));
        if let Some(r) = self.report.lock().unwrap().clone() {
            t.finish(r);
        }
        Ok(())
    }
    fn health(&self) -> Option<DriverHealth> {
        self.health.clone()
    }
}

fn done(writes: Vec<Write>) -> Option<Report> {
    Some(Ok(Outcome { applied: Applied::Done, writes }))
}

fn router(a: Arc<dyn Actuator>) -> Router {
    Router { per_device: Device::ALL.iter().map(|d| (*d, a.clone())).collect() }
}

fn control(cfg: ControlConfig) -> (Control, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (Control::new(cfg, dir.path()), dir)
}

fn lines(dir: &tempfile::TempDir, file: &str) -> Vec<serde_json::Value> {
    std::fs::read_to_string(dir.path().join(file))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn at(s: i64) -> Ts {
    t0() + Duration::seconds(s)
}

/// The defaults: on, live, 60 s read-back, 30 commands a minute per device, release the Solis
/// RC at shutdown; zeros are refused; each mode writes its own log.
#[test]
fn defaults_and_checks() {
    let c = ControlConfig::default();
    assert!(c.enabled && !c.dry_run && c.alarms);
    assert_eq!((c.verify_s, c.max_commands_per_min, c.startup_wait_s), (60, 30, 60));
    assert_eq!(c.release_on_shutdown, vec![Device::Solis]);
    c.check().unwrap();
    assert!(ControlConfig { verify_s: 0, ..c.clone() }.check().is_err());
    assert!(ControlConfig { max_commands_per_min: 0, ..c.clone() }.check().is_err());
    let d = std::path::Path::new("/var/lib/energie");
    assert_eq!(c.log_path(d), d.join("control-audit.jsonl"));
    assert_eq!(ControlConfig { dry_run: true, ..c.clone() }.log_path(d), d.join("control-dryrun.jsonl"));
    let own = ControlConfig { audit_file: Some("/a".into()), dry_run_file: Some("/b".into()), ..c.clone() };
    assert_eq!(own.log_path(d), PathBuf::from("/a"));
    assert_eq!(ControlConfig { dry_run: true, ..own }.log_path(d), PathBuf::from("/b"));
    let t: ControlConfig =
        toml::from_str("dry_run = true\ndevices = { solis = false }\nrelease_on_shutdown = []").unwrap();
    assert!(t.dry_run && t.release_on_shutdown.is_empty() && !t.devices[&Device::Solis]);
    assert!(toml::from_str::<ControlConfig>("dryrun = true").is_err());
}

/// The kill switch and the per-device switches block a command before it reaches the
/// actuator; they are entities, persisted, and the persisted state wins at the next start.
#[test]
fn switches_block_and_persist() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new(done(vec![]));
    let r = router(fake.clone());
    let w = World::default();
    let mut c = Control::new(
        ControlConfig { devices: BTreeMap::from([(Device::Airco, false)]), ..Default::default() },
        dir.path(),
    );
    let cmd = Command::SolisRcTimeoutMin(10);
    c.dispatch(t0(), &cmd, &w, &r).unwrap();
    let airco = Command::Airco { unit: energie_core::command::AircoUnit::Zolder, on: true };
    assert_eq!(c.dispatch(t0(), &airco, &w, &r).unwrap_err(), "blocked: control of airco is switched off");
    c.set(None, false);
    assert_eq!(c.dispatch(t0(), &cmd, &w, &r).unwrap_err(), "blocked: control is switched off");
    assert_eq!(fake.got().len(), 1);
    assert_eq!(c.stats.blocked, 2);
    assert!(!c.live[&Device::Solis].load(Ordering::SeqCst));
    let es = c.entities();
    let ids: Vec<_> = es.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "control_enabled",
            "control_dry_run",
            "control_evse",
            "control_cable",
            "control_solis",
            "control_airco",
            "control_prices"
        ]
    );
    assert_eq!(es[0].value, Value::Bool(false));
    assert!(es[0].control.is_some() && es[1].control.is_none());
    assert_eq!(Control::switch_of("enabled"), Some(None));
    assert_eq!(Control::switch_of("solis"), Some(Some(Device::Solis)));
    assert_eq!(Control::switch_of("x"), None);
    // A new start reads the switches back, whatever the config says.
    let again = Control::new(ControlConfig::default(), dir.path());
    assert!(!again.flags().enabled);
    assert!(!again.flags().devices[&Device::Airco]);
    c.set(Some(Device::Airco), true);
    c.set(None, true);
    assert!(Control::new(ControlConfig::default(), dir.path()).flags().devices[&Device::Airco]);
    // An unwritable state dir only logs.
    c.flags_file = dir.path().join("missing/control.json");
    c.set(None, true);
}

/// No command for a device before its first good reading; a token bucket of
/// `max_commands_per_min` per device refills over the minute.
#[test]
fn readiness_and_rate_limit() {
    let fresh = Arc::new(Fake { health: Some(DriverHealth::default()), ..Arc::into_inner(Fake::new(None)).unwrap() });
    let (mut c, _d) = control(ControlConfig { max_commands_per_min: 3, ..Default::default() });
    let w = World::default();
    let cmd = Command::EvseMode(EvseMode::Normal);
    assert_eq!(c.dispatch(t0(), &cmd, &w, &router(fresh)).unwrap_err(), "blocked: no reading of evse yet");
    let read = Arc::new(Fake {
        health: Some(DriverHealth { reads_ok: 1, ..Default::default() }),
        ..Arc::into_inner(Fake::new(done(vec![]))).unwrap()
    });
    let r = router(read.clone());
    for _ in 0..3 {
        c.dispatch(t0(), &cmd, &w, &r).unwrap();
    }
    assert!(c.dispatch(t0(), &cmd, &w, &r).unwrap_err().contains("more than 3 commands a minute for evse"));
    c.dispatch(t0(), &Command::CableStop, &w, &r).unwrap();
    assert!(c.dispatch(at(10), &cmd, &w, &r).is_err(), "half a token after 10 s");
    c.dispatch(at(20), &cmd, &w, &r).unwrap();
    assert_eq!(read.got().len(), 5);
}

/// A real command is audited with its wire writes, the value before, the value it must show
/// and what was read back; the line is written once it is verified (or no field shows it).
#[test]
fn audit_with_read_back() {
    let (mut c, dir) = control(ControlConfig::default());
    let write = Write::new("holding 43136", 150).before(Some(0));
    let fake = Fake::new(done(vec![write.clone()]));
    let r = router(fake);
    let mut w = World::default();
    w.solis.rc_charge_w = Some(0.0);
    c.dispatch(t0(), &Command::SolisRcChargeW(1500), &w, &r).unwrap();
    c.dispatch(t0(), &Command::CableStop, &w, &r).unwrap();
    assert_eq!((c.pending(), c.unreported()), (2, 0));
    assert!(c.poll(at(1), &w).is_empty());
    assert_eq!(c.pending(), 1, "the cable stop is logged as sent");
    w.solis.rc_charge_w = Some(1500.0);
    assert!(c.poll(at(14), &w).is_empty());
    let log = lines(&dir, "control-audit.jsonl");
    assert_eq!(log.len(), 2);
    assert_eq!(log[0]["result"], "sent");
    assert_eq!(log[0]["field"], serde_json::Value::Null);
    let e = &log[1];
    assert_eq!(
        (e["result"].as_str(), e["device"].as_str(), e["actuator"].as_str()),
        (Some("verified"), Some("solis"), Some("fake"))
    );
    assert_eq!(e["command"], serde_json::json!({"SolisRcChargeW": 1500}));
    assert_eq!(e["writes"], serde_json::json!([{"target": "holding 43136", "value": 150, "before": 0}]));
    assert_eq!(
        (e["field"].as_str(), e["before"].as_f64(), e["want"].as_f64(), e["after"].as_f64()),
        (Some("battery_inverter_rc_charge_w"), Some(0.0), Some(1500.0), Some(1500.0))
    );
    assert_eq!(e["applied"], "Done");
    let v = c.view();
    assert_eq!((v.stats.sent, v.stats.verified, v.pending, v.recent.len()), (2, 1, 0, 2));
    assert_eq!(v.recent[0].result, Verdict::Verified, "newest first");
    assert!(v.log_file.ends_with("control-audit.jsonl") && v.enabled && !v.dry_run);
}

/// A write the device does not show within `verify_s` raises one alarm per field per
/// `alarm_interval_s`; a driver that needed no write and reads back counts as unchanged.
#[test]
fn failed_read_back_alarms_once() {
    let (mut c, dir) = control(ControlConfig { verify_s: 30, alarm_interval_s: 600, ..Default::default() });
    let r = router(Fake::new(done(vec![])));
    let mut w = World::default();
    w.evse.override_a = Some(6.0);
    c.dispatch(t0(), &Command::EvseCurrentDeciA(145), &w, &r).unwrap();
    assert!(c.poll(at(29), &w).is_empty());
    let alarms = c.poll(at(30), &w);
    assert_eq!(alarms.len(), 1);
    assert_eq!(alarms[0].0, "Sturing: schrijven niet gelukt");
    assert!(alarms[0].1.contains("charger_buzz_override_a toont 6.0 in plaats van 14.5 na 30 s"), "{}", alarms[0].1);
    c.dispatch(at(40), &Command::EvseCurrentDeciA(145), &w, &r).unwrap();
    assert!(c.poll(at(80), &w).is_empty(), "the same field alarms again only after the interval");
    c.dispatch(at(700), &Command::EvseCurrentDeciA(145), &w, &r).unwrap();
    assert_eq!(c.poll(at(740), &w).len(), 1);
    assert_eq!(c.stats.failed, 3);
    let unchanged = router(Fake::new(Some(Ok(Outcome { applied: Applied::Unchanged, writes: vec![] }))));
    w.evse.override_a = Some(14.5);
    c.dispatch(at(800), &Command::EvseCurrentDeciA(145), &w, &unchanged).unwrap();
    c.poll(at(801), &w);
    let log = lines(&dir, "control-audit.jsonl");
    assert_eq!(log.last().unwrap()["result"], "unchanged");
    assert_eq!(log[0]["after"], 6.0);
    // Alarms can be switched off.
    let (mut quiet, _d) = control(ControlConfig { verify_s: 1, alarms: false, ..Default::default() });
    w.evse.override_a = Some(6.0);
    quiet.dispatch(t0(), &Command::EvseCurrentDeciA(145), &w, &r).unwrap();
    assert!(quiet.poll(at(5), &w).is_empty());
    assert_eq!(quiet.stats.failed, 1);
}

/// A newer command for the same field with another value supersedes a pending read-back; the
/// same value again keeps the first deadline, so a write that never takes still alarms even
/// though it is re-sent every 30 s.
#[test]
fn superseding_and_repeated_commands() {
    let (mut c, dir) = control(ControlConfig { verify_s: 60, ..Default::default() });
    let r = router(Fake::new(done(vec![])));
    let mut w = World::default();
    w.solis.rc_mode = Some(RcMode::None);
    c.dispatch(t0(), &Command::SolisRcMode(RcMode::ForceCharge), &w, &r).unwrap();
    c.dispatch(at(1), &Command::SolisRcMode(RcMode::ForceDischarge), &w, &r).unwrap();
    c.poll(at(2), &w);
    assert_eq!(lines(&dir, "control-audit.jsonl")[0]["result"], "superseded");
    c.dispatch(at(31), &Command::SolisRcMode(RcMode::ForceDischarge), &w, &r).unwrap();
    assert_eq!(c.poll(at(61), &w).len(), 1, "both fail at the first one's deadline");
    assert_eq!(c.pending(), 0);
}

/// A driver error or a refused queue is an error entry and an alarm per device; a command
/// whose outcome never comes is closed at its deadline.
#[test]
fn errors_and_missing_outcomes() {
    let (mut c, dir) = control(ControlConfig { verify_s: 10, ..Default::default() });
    let w = World::default();
    let failing = router(Fake::new(Some(Err("not connected".into()))));
    c.dispatch(t0(), &Command::CableSetCurrent(10), &w, &failing).unwrap();
    let alarms = c.poll(at(1), &w);
    assert_eq!(alarms[0].0, "Sturing: commando mislukt");
    assert!(alarms[0].1.contains("naar cable: not connected"));
    let silent = router(Fake::new(None));
    c.dispatch(t0(), &Command::CableStop, &w, &silent).unwrap();
    assert_eq!(c.unreported(), 1);
    c.poll(at(5), &w);
    assert_eq!(c.pending(), 1);
    c.poll(at(10), &w);
    let log = lines(&dir, "control-audit.jsonl");
    assert_eq!((log[0]["result"].as_str(), log[0]["detail"].as_str()), (Some("error"), Some("not connected")));
    assert_eq!(log[1]["detail"], "no outcome from the device task");
    let none = Router { per_device: BTreeMap::new() };
    assert_eq!(c.dispatch(t0(), &Command::CableStop, &w, &none).unwrap_err(), "no actuator for Cable");
    assert!(c.dispatch(t0(), &Command::notify("a", "b"), &w, &none).is_err());
    assert_eq!(c.stats.errors, 3);
}

/// Dry run: the actuator is asked for a plan, nothing is verified, every command is a
/// `dry_run` line in the dry-run log with the writes it would make.
#[test]
fn dry_run_logs_the_plan() {
    let (mut c, dir) = control(ControlConfig { dry_run: true, ..Default::default() });
    assert!(c.dry_run() && c.config().dry_run);
    let fake = Fake::new(done(vec![Write::new("holding 43135", 1).before(Some(0))]));
    let r = router(fake.clone());
    c.dispatch(t0(), &Command::SolisRcMode(RcMode::ForceCharge), &World::default(), &r).unwrap();
    c.poll(at(1), &World::default());
    assert_eq!(fake.got(), vec![(Command::SolisRcMode(RcMode::ForceCharge), true)]);
    let log = lines(&dir, "control-dryrun.jsonl");
    assert_eq!((log[0]["result"].as_str(), log[0]["dry_run"].as_bool()), (Some("dry_run"), Some(true)));
    assert_eq!(log[0]["writes"][0]["target"], "holding 43135");
    assert_eq!(log[0]["field"], serde_json::Value::Null);
    assert!(!dir.path().join("control-audit.jsonl").exists());
    assert_eq!((c.stats.dry_run, c.stats.sent), (1, 0));
    assert!(c.entities()[1].value == Value::Bool(true));
}

/// At shutdown the configured releases go out past the rate limit (the switches still count);
/// the airco and the prices have none.
#[test]
fn release_at_shutdown() {
    let cfg = ControlConfig {
        max_commands_per_min: 1,
        release_on_shutdown: vec![Device::Solis, Device::Evse, Device::Cable, Device::Airco, Device::Prices],
        ..Default::default()
    };
    let (mut c, _d) = control(cfg);
    let fake = Fake::new(done(vec![]));
    let r = router(fake.clone());
    c.dispatch(t0(), &Command::SolisRcTimeoutMin(10), &World::default(), &r).unwrap();
    c.release(t0(), &World::default(), &r);
    let got: Vec<_> = fake.got().into_iter().map(|(c, _)| c).collect();
    assert_eq!(
        got,
        [
            Command::SolisRcTimeoutMin(10),
            Command::SolisRcMode(RcMode::None),
            Command::EvseCurrentDeciA(0),
            Command::CableStop
        ]
    );
    c.set(None, false);
    c.release(t0(), &World::default(), &r);
    assert_eq!(fake.got().len(), 4, "a switched-off control releases nothing");
}

/// Tickets: the first report counts; a ticket compares by identity.
#[test]
fn tickets() {
    let flag = Arc::new(AtomicBool::new(true));
    let t = Ticket::new(flag.clone());
    assert!(t.allowed() && !t.done());
    t.finish(Err("first".into()));
    t.finish(Ok(Outcome { applied: Applied::Done, writes: vec![] }));
    assert_eq!(t.take(), Some(Err("first".into())));
    assert_eq!(t, t.clone());
    assert_ne!(t, Ticket::new(flag.clone()));
    flag.store(false, Ordering::SeqCst);
    assert!(!t.allowed());
    assert_eq!(format!("{t:?}"), "Ticket { allowed: false, done: true }");
}

/// The default `submit_tracked` (bridges, the log actuator): a dry run names the command, a
/// switched-off ticket is refused, otherwise it is submitted.
#[test]
fn default_tracked_submit() {
    let on = Ticket::new(Arc::new(AtomicBool::new(true)));
    LogActuator.submit_tracked(t0(), &Command::CableStop, false, on.clone()).unwrap();
    assert_eq!(on.take(), Some(Ok(Outcome { applied: Applied::Done, writes: vec![] })));
    let dry = Ticket::new(Arc::new(AtomicBool::new(true)));
    LogActuator.submit_tracked(t0(), &Command::CableStop, true, dry.clone()).unwrap();
    assert_eq!(dry.take().unwrap().unwrap().writes, vec![Write::command(&Command::CableStop)]);
    let off = Ticket::new(Arc::new(AtomicBool::new(false)));
    LogActuator.submit_tracked(t0(), &Command::CableStop, false, off.clone()).unwrap();
    assert!(off.take().unwrap().is_err());
}

/// Start-up waits for the first reading of every native controller, up to a limit.
#[tokio::test]
async fn waiting_for_first_readings() {
    let fresh = Arc::new(Fake { health: Some(DriverHealth::default()), ..Arc::into_inner(Fake::new(None)).unwrap() });
    let mut per_device: BTreeMap<Device, Arc<dyn Actuator>> = BTreeMap::new();
    per_device.insert(Device::Solis, fresh);
    per_device.insert(Device::Cable, Arc::new(LogActuator));
    let r = Router { per_device };
    assert_eq!(wait_ready(&r, std::time::Duration::from_millis(60)).await, vec![Device::Solis]);
    assert!(wait_ready(&router(Arc::new(LogActuator)), std::time::Duration::from_secs(5)).await.is_empty());
}

/// The aircos' cloud does not hold up the start; their commands stay refused until a reading.
#[tokio::test]
async fn the_start_does_not_wait_for_the_aircos() {
    let silent = Arc::new(Fake { health: Some(DriverHealth::default()), ..Arc::into_inner(Fake::new(None)).unwrap() });
    let mut per_device: BTreeMap<Device, Arc<dyn Actuator>> = BTreeMap::new();
    per_device.insert(Device::Airco, silent);
    let r = Router { per_device };
    let start = std::time::Instant::now();
    assert!(wait_ready(&r, std::time::Duration::from_secs(30)).await.is_empty());
    assert!(start.elapsed() < std::time::Duration::from_secs(1));
}

/// A log that cannot be written is counted, not fatal.
#[test]
fn unwritable_log_is_counted() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = ControlConfig { audit_file: Some(dir.path().join("missing/a.jsonl")), ..Default::default() };
    let mut c = Control::new(cfg, dir.path());
    c.dispatch(t0(), &Command::CableStop, &World::default(), &router(Fake::new(done(vec![])))).unwrap();
    c.poll(at(1), &World::default());
    assert_eq!(c.stats.log_errors, 1);
}

/// At shutdown what is still pending is written as it stands: sent but not read back, or
/// without an outcome from its device task.
#[test]
fn close_writes_what_is_pending() {
    let (mut c, dir) = control(ControlConfig::default());
    let w = World::default();
    c.dispatch(t0(), &Command::SolisRcTimeoutMin(10), &w, &router(Fake::new(done(vec![])))).unwrap();
    c.dispatch(t0(), &Command::SolisRcChargeW(100), &w, &router(Fake::new(None))).unwrap();
    assert!(c.close(at(1), &w).is_empty());
    assert_eq!(c.pending(), 0);
    let log = lines(&dir, "control-audit.jsonl");
    assert_eq!(
        (log[0]["result"].as_str(), log[0]["detail"].as_str()),
        (Some("sent"), Some("stopped before the read-back"))
    );
    assert_eq!(
        (log[1]["result"].as_str(), log[1]["detail"].as_str()),
        (Some("error"), Some("stopped before the device task reported"))
    );
}
