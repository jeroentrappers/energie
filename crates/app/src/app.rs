//! Wiring: build the runtime from the config, start the HA client, MQTT listener, tick loop
//! and web server, and stop them again.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use energie_core::engine::Engine;
use energie_core::time::{TZ, Ts};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::config::{Config, Mode, NotifyKind, read_secret};
use crate::control::{self, Control};
use crate::error::{Error, Result};
use crate::ha::entity::{HaStore, parse_time};
use crate::ha::mapping::{memory_from, seed_meters, settings_from};
use crate::ha::ws::{self, ClientConfig, ServiceCall, SharedStore};
use crate::ha::{HaActuator, HaSource};
use crate::history::{self, History, SharedHistory};
use crate::mqtt::{self, SharedInbox, Subscriber};
use crate::notify::{LogNotifier, Notifier, NtfyNotifier};
use crate::persist::Persister;
use crate::plan::Provider;
use crate::plug::{
    Actuator, Device, DriverFactory, LogActuator, PvSum, Router, Section, Sources, Unavailable, WorldSource,
};
use crate::runtime::{Parts, Runtime};
use crate::shadow::Shadow;
use crate::shadow::commands::act_entities;
use crate::shadow::log::MismatchLog;
use crate::sim::{House, SimHandle};
use crate::web::{self, AppState};

/// Wall-clock time, or a virtual clock that advances one tick per tick (simulate mode).
pub struct TickClock {
    virt: Option<Mutex<Ts>>,
    step: chrono::Duration,
}

impl TickClock {
    pub fn wall() -> Self {
        TickClock { virt: None, step: chrono::Duration::zero() }
    }

    pub fn virtual_from(start: Ts, step: chrono::Duration) -> Self {
        TickClock { virt: Some(Mutex::new(start)), step }
    }

    /// The time of the next tick.
    pub fn next(&self) -> Ts {
        match &self.virt {
            Some(t) => {
                let mut t = t.lock().expect("clock lock");
                *t += self.step;
                *t
            }
            None => chrono::Utc::now().with_timezone(&TZ),
        }
    }

    /// The time now, without advancing.
    pub fn current(&self) -> Ts {
        match &self.virt {
            Some(t) => *t.lock().expect("clock lock"),
            None => chrono::Utc::now().with_timezone(&TZ),
        }
    }
}

pub struct Running {
    pub addr: SocketAddr,
    pub rt: Arc<Mutex<Runtime>>,
    pub ha: Option<SharedStore>,
    pub history: Option<SharedHistory>,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    clock: Arc<TickClock>,
}

impl Running {
    /// Stop every task and write the state one last time.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        for t in self.tasks {
            let _ = t.await;
        }
        // Control mode: the release commands go out while the drivers still run.
        self.rt.lock().expect("runtime lock").release(self.clock.current());
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while self.rt.lock().expect("runtime lock").control_unreported() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.rt.lock().expect("runtime lock").close_control(self.clock.current());
        self.rt.lock().expect("runtime lock").shutdown(self.clock.current());
        if let Some(h) = &self.history {
            h.lock().expect("history lock").flush();
        }
        tracing::info!("stopped");
    }
}

/// Wait until the HA client has a snapshot, or `limit` passes.
async fn wait_for_snapshot(store: &SharedStore, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        if store.lock().expect("store lock").snapshots > 0 {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!("no home assistant snapshot yet; starting without it");
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Ha {
    store: SharedStore,
    calls: mpsc::UnboundedSender<ServiceCall>,
    seeded: bool,
}

async fn start_ha(
    cfg: &Config,
    need_seed: bool,
    stop: &watch::Receiver<bool>,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<Option<Ha>> {
    if !cfg.needs_ha() {
        return Ok(None);
    }
    let ha = cfg.ha.as_ref().expect("validated: needs_ha implies [ha]");
    let token = read_secret(&ha.token_file)?;
    let store: SharedStore = Arc::new(Mutex::new(HaStore::watching(act_entities())));
    let (calls, rx) = mpsc::unbounded_channel();
    tasks.push(tokio::spawn(ws::run(ClientConfig::new(ha.ws_url()?, token), store.clone(), rx, stop.clone())));
    let wait = (ha.seed && need_seed) || cfg.mode == Mode::Shadow;
    let got = wait && wait_for_snapshot(&store, Duration::from_secs(ha.start_wait_s)).await;
    Ok(Some(Ha { store, calls, seeded: got && ha.seed }))
}

fn start_mqtt(
    cfg: &Config,
    stop: &watch::Receiver<bool>,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<Option<SharedInbox>> {
    let Some(m) = &cfg.mqtt else { return Ok(None) };
    let password = m.password_file.as_deref().map(read_secret).transpose()?;
    let sub = Subscriber {
        addr: format!("{}:{}", m.host, m.port),
        client_id: m.client_id.clone(),
        username: m.username.clone(),
        password,
        topic: m.topic.clone(),
        keepalive: Duration::from_secs(30),
        backoff_min: Duration::from_secs(1),
        backoff_max: Duration::from_secs(60),
    };
    let inbox: SharedInbox = Arc::default();
    tasks.push(tokio::spawn(mqtt::run(sub, inbox.clone(), stop.clone())));
    Ok(Some(inbox))
}

/// The MQTT bridge, when `[mqtt_publish] enabled = true` (never otherwise): the snapshot
/// stream out, `/set` messages in through [`Runtime::set_entity`].
fn start_bridge(
    cfg: &Config,
    plan: &crate::plan::Plan,
    rt: &Arc<Mutex<Runtime>>,
    mut ready: watch::Receiver<bool>,
    stop: &watch::Receiver<bool>,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<()> {
    let m = &cfg.mqtt_publish;
    if !m.enabled {
        return Ok(());
    }
    let password = m.password_file.as_deref().map(read_secret).transpose()?;
    let link = crate::bridge::client::Link {
        host: m.broker.clone().expect("validated: enabled needs a broker"),
        port: m.port,
        client_id: m.client_id.clone(),
        credentials: m.username.clone().map(|u| (u, password.unwrap_or_default())),
        keep_alive: Duration::from_secs(30),
        backoff_min: Duration::from_secs(1),
        backoff_max: Duration::from_secs(60),
    };
    let publisher = crate::bridge::Publisher::new(crate::bridge::BridgeConfig::from_config(cfg, &plan.devices));
    let snap = rt.lock().expect("runtime lock").snapshot_rx();
    let r = rt.clone();
    let set: crate::bridge::client::Setter = Arc::new(move |id, v| {
        let now = chrono::Utc::now().with_timezone(&TZ);
        r.lock().expect("runtime lock").set_entity(now, id, v)
    });
    tracing::info!(broker = %link.host, prefix = %m.prefix, discovery = cfg.discovery(), "mqtt bridge");
    let stop = stop.clone();
    tasks.push(tokio::spawn(async move {
        // Nothing goes to HA before the first decision: the snapshot is still empty until then.
        if ready.wait_for(|r| *r).await.is_ok() {
            crate::bridge::client::run(link, publisher, snap, set, stop).await;
        }
    }));
    Ok(())
}

fn notifier(cfg: &Config) -> Result<Arc<dyn Notifier>> {
    if cfg.mode == Mode::Shadow || cfg.notify.kind == NotifyKind::Log {
        return Ok(Arc::new(LogNotifier));
    }
    let token = cfg.notify.token_file.as_deref().map(read_secret).transpose()?;
    let url = cfg.notify.url.as_deref().expect("validated: ntfy needs a url");
    Ok(Arc::new(Arc::new(NtfyNotifier::new(url, token)?)))
}

/// Build everything and start serving. `drivers` provides the native device instances.
/// The web password stands alone, with no account lock-out behind it.
const MIN_WEB_PASSWORD_CHARS: usize = 12;

pub async fn start(cfg: Config, drivers: &dyn DriverFactory) -> Result<Running> {
    let plan = cfg.plan()?;
    for w in cfg.warnings() {
        tracing::warn!("{w}");
    }
    match cfg.site {
        Some(s) => energie_core::sun::set_site(s.latitude, s.longitude),
        None if cfg.mode == Mode::Control => {
            tracing::warn!("no [site] in the config: the sun position is computed for central Brussels")
        }
        None => {}
    }
    let web_password = match &cfg.web_password_file {
        Some(p) => {
            let read = std::fs::read_to_string(p).map_err(|e| Error::new(format!("{}: {e}", p.display())))?;
            let password = read.trim().to_string();
            if password.chars().count() < MIN_WEB_PASSWORD_CHARS {
                return Err(Error::new(format!(
                    "{}: the password must be at least {MIN_WEB_PASSWORD_CHARS} characters",
                    p.display()
                )));
            }
            Some(password)
        }
        None => None,
    };
    std::fs::create_dir_all(&cfg.state_dir).map_err(|e| Error::new(format!("{}: {e}", cfg.state_dir.display())))?;
    let mut persister = Persister::new(cfg.state_dir.clone(), cfg.persist_interval_s);
    let (file_settings, file_memory) = persister.load()?;
    let (stop, stop_rx) = watch::channel(false);
    let mut tasks = Vec::new();

    let empty = file_settings.is_none() || file_memory.is_none();
    let ha = start_ha(&cfg, empty, &stop_rx, &mut tasks).await?;
    let seed = ha.as_ref().filter(|h| h.seeded).map(|h| h.store.clone());
    let now = chrono::Utc::now().with_timezone(&energie_core::time::TZ);
    let imported = match &cfg.ha {
        Some(h) if h.import_only && h.seed && empty => import_once(h, now).await,
        _ => None,
    };
    let (imported_settings, imported_memory) = imported.unzip();
    let settings = file_settings.or(imported_settings).unwrap_or_else(|| match &seed {
        Some(s) => settings_from(&*s.lock().expect("store lock"), Default::default()),
        None => Default::default(),
    });
    let memory = file_memory.or(imported_memory).unwrap_or_else(|| match &seed {
        Some(s) => {
            let store = s.lock().expect("store lock");
            let mut m = memory_from(&*store, Default::default());
            m.meters = seed_meters(&*store, now);
            m
        }
        None => Default::default(),
    });

    let tick = chrono::Duration::milliseconds(cfg.tick_ms as i64);
    let clock = Arc::new(if cfg.mode == Mode::Simulate {
        let start = cfg.sim.start.as_deref().and_then(parse_time).unwrap_or_else(|| TickClock::wall().current());
        TickClock::virtual_from(start, tick)
    } else {
        TickClock::wall()
    });
    let history = open_history(&cfg, clock.current(), &stop_rx, &mut tasks)?;
    let sim = cfg.needs_sim().then(|| SimHandle::new(House::new(cfg.sim.clone(), clock.current())));

    let mut per_section: BTreeMap<Section, Arc<dyn WorldSource>> = BTreeMap::new();
    for s in Section::ALL {
        let mut srcs = Vec::new();
        for p in &plan.readers[&s.slot()] {
            srcs.push(match p {
                Provider::Ha => {
                    Arc::new(HaSource(ha.as_ref().expect("needs_ha").store.clone())) as Arc<dyn WorldSource>
                }
                Provider::Sim => Arc::new(sim.clone().expect("needs_sim")),
                Provider::Unavailable => Arc::new(Unavailable),
                Provider::Device(id) => drivers
                    .source(id)
                    .ok_or_else(|| Error::new(format!("no driver available for source {s:?} (device {id})")))?,
            });
        }
        let src = if srcs.len() == 1 { srcs.remove(0) } else { Arc::new(PvSum::new(srcs)) };
        per_section.insert(s, src);
    }
    let mut per_device: BTreeMap<Device, Arc<dyn Actuator>> = BTreeMap::new();
    for d in Device::ALL {
        let p = plan.controllers.get(&d).filter(|_| cfg.mode != Mode::Shadow);
        let act: Arc<dyn Actuator> = match p {
            None | Some(Provider::Unavailable) => Arc::new(LogActuator),
            Some(Provider::Ha) => Arc::new(HaActuator(ha.as_ref().expect("needs_ha").calls.clone())),
            Some(Provider::Sim) => Arc::new(sim.clone().expect("needs_sim")),
            Some(Provider::Device(id)) => drivers
                .actuator(id)
                .ok_or_else(|| Error::new(format!("no driver available for actuator {d:?} (device {id})")))?,
        };
        per_device.insert(d, act);
    }

    // Read the controlled devices before the first decision, in the background: the dashboard and
    // the API do not wait for a device.
    let gate = (cfg.mode == Mode::Control).then(|| Router { per_device: per_device.clone() });
    let startup_wait = Duration::from_secs(cfg.control.startup_wait_s);
    let (ready_tx, ready_rx) = watch::channel(false);
    let mqtt = start_mqtt(&cfg, &stop_rx, &mut tasks)?;
    let shadow = (cfg.mode == Mode::Shadow)
        .then(|| Shadow::new(&cfg.shadow, MismatchLog::new(Some(cfg.shadow_log()), cfg.shadow.log_max_bytes)));
    let (rt, snap) = Runtime::new(Parts {
        mode: cfg.mode,
        engine: Engine::new(settings, memory),
        sources: Sources { per_section },
        router: Router { per_device },
        notifier: notifier(&cfg)?,
        ha: ha.as_ref().map(|h| h.store.clone()),
        mqtt,
        lockstep: cfg.shadow.lockstep,
        settings_source: cfg.settings_source(),
        shadow,
        persister: Some(persister),
        devices: drivers.devices(),
        publish: cfg.publish.clone(),
        history: history.clone(),
        control: (cfg.mode == Mode::Control).then(|| Control::new(cfg.control.clone(), &cfg.state_dir)),
    });
    let rt = Arc::new(Mutex::new(rt));
    start_bridge(&cfg, &plan, &rt, ready_rx, &stop_rx, &mut tasks)?;

    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    let addr = listener.local_addr()?;
    let period =
        Duration::from_millis(cfg.tick_ms).div_f64(if cfg.mode == Mode::Simulate { cfg.sim.speed } else { 1.0 });
    let state = AppState {
        rt: rt.clone(),
        snap,
        stale_after: (period * 10).max(Duration::from_secs(10)),
        stop: stop_rx.clone(),
        history: history.clone(),
    };
    let mut web_stop = stop_rx.clone();
    let oidc = match &cfg.oidc {
        Some(o) => {
            let secret = o.client_secret_file.as_deref().map(read_secret).transpose()?;
            let oidc = Arc::new(web::oidc::Oidc::new(o.clone(), secret)?);
            // Only for the log: the controller never waits for the provider.
            let probe = oidc.clone();
            tokio::spawn(async move {
                match probe.check().await {
                    Ok(()) => tracing::info!("oidc: provider reached"),
                    Err(e) => tracing::warn!(error = %e, "oidc: provider not reachable; sign-in waits for it"),
                }
            });
            Some(oidc)
        }
        None => None,
    };
    // The command port stays optional: without it the service still runs, only `energie
    // credentials` cannot reach it.
    let cli_addr = cfg.cli_listen();
    match tokio::net::TcpListener::bind(&cli_addr).await {
        Ok(cli) => {
            let (app, mut stop) = (web::cli_router(state.clone()), web_stop.clone());
            tasks.push(tokio::spawn(async move {
                let app = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
                let shutdown = async move {
                    let _ = stop.wait_for(|s| *s).await;
                };
                axum::serve(cli, app).with_graceful_shutdown(shutdown).await.expect("axum::serve does not fail");
            }));
        }
        Err(e) => {
            tracing::warn!(addr = %cli_addr, error = %e, "command port unavailable; energie credentials will not work")
        }
    }
    tasks.push(tokio::spawn(async move {
        let app = web::router(state);
        let app = web::protect(app, web_password, oidc);
        let shutdown = async move {
            let _ = web_stop.wait_for(|s| *s).await;
        };
        // axum retries accept errors itself; serve only ends at shutdown.
        axum::serve(listener, app).with_graceful_shutdown(shutdown).await.expect("axum::serve does not fail");
    }));
    tracing::info!(mode = cfg.mode.label(), "listening on http://{addr}");
    let (boot_rt, boot_clock, mut boot_stop) = (rt.clone(), clock.clone(), stop_rx.clone());
    tasks.push(tokio::spawn(async move {
        if let Some(gate) = gate {
            tokio::select! {
                waiting = control::wait_ready(&gate, startup_wait) => {
                    if !waiting.is_empty() {
                        tracing::warn!(?waiting, "no first reading yet; their commands wait for one");
                    }
                }
                _ = boot_stop.wait_for(|s| *s) => return,
            }
        }
        let _ = ready_tx.send(true);
        tick_loop(boot_rt, boot_clock, period, stop_rx).await;
    }));
    Ok(Running { addr, rt, ha: ha.map(|h| h.store), history, stop, tasks, clock })
}

/// The first-start import of an `[ha] import_only` setup: HA's helpers, read once; `None`
/// (logged) when HA cannot be read, so the binary starts on the defaults.
async fn import_once(
    ha: &crate::config::HaConfig,
    now: Ts,
) -> Option<(energie_core::settings::Settings, energie_core::memory::Memory)> {
    match crate::import::fetch(ha, now, Duration::from_secs(ha.start_wait_s)).await {
        Ok(sm) => {
            tracing::info!(url = %ha.url, "settings and memory imported from home assistant (first start)");
            Some(sm)
        }
        Err(e) => {
            tracing::warn!(error = %e, "importing the settings from home assistant failed; starting on the defaults");
            None
        }
    }
}

/// Open the history store and, on a first start with `seed_from_ha`, seed it from HA's recorder
/// in the background.
fn open_history(
    cfg: &Config,
    now: Ts,
    stop: &watch::Receiver<bool>,
    tasks: &mut Vec<JoinHandle<()>>,
) -> Result<Option<SharedHistory>> {
    if !cfg.history.enabled {
        return Ok(None);
    }
    let ret = cfg.history.retention(&cfg.state_dir);
    let h = History::open_with(history::series::meta(), &cfg.history.extra_series, &ret, now.timestamp())?;
    let seed = cfg.history.seed_from_ha && h.is_empty();
    let h: SharedHistory = Arc::new(Mutex::new(h));
    if seed {
        let ha = cfg.ha.as_ref().expect("validated: seed_from_ha needs [ha]");
        let client = ClientConfig::new(ha.ws_url()?, read_secret(&ha.token_file)?);
        tasks.push(tokio::spawn(history::seed::run(client, h.clone(), now, Duration::from_secs(300), stop.clone())));
    }
    Ok(Some(h))
}

async fn tick_loop(rt: Arc<Mutex<Runtime>>, clock: Arc<TickClock>, period: Duration, mut stop: watch::Receiver<bool>) {
    let mut iv = tokio::time::interval(period);
    iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = iv.tick() => {
                let now = clock.next();
                rt.lock().expect("runtime lock").step(now);
            }
            _ = stop.wait_for(|s| *s) => break,
        }
    }
}

/// Resolves when the process should stop.
pub type Shutdown = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

/// Start, run until `shutdown` resolves, stop.
pub async fn run(cfg: Config, drivers: &dyn DriverFactory, shutdown: Shutdown) -> Result<()> {
    let running = start(cfg, drivers).await?;
    shutdown.await;
    tracing::info!("shutting down");
    running.stop().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ha::ws::tests::{fake_ha, push, recv, until};
    use crate::plug::NoDrivers;
    use serde_json::json;
    use std::path::Path;

    fn write(dir: &Path, name: &str, text: &str) -> String {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        crate::toml_path(&p).to_string()
    }

    async fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.unwrap();
        let code = buf[9..12].parse().unwrap();
        (code, buf)
    }

    /// The virtual clock advances one tick per tick; the wall clock is now.
    #[test]
    fn clocks() {
        let t0 = crate::plug::tests::t0();
        let c = TickClock::virtual_from(t0, chrono::Duration::seconds(1));
        assert_eq!(c.next(), t0 + chrono::Duration::seconds(1));
        assert_eq!(c.current(), t0 + chrono::Duration::seconds(1));
        let w = TickClock::wall();
        assert!((w.next() - w.current()).num_seconds().abs() < 2);
    }

    /// Simulate mode runs closed-loop on a virtual clock, serves the dashboard and persists
    /// its state at shutdown.
    #[tokio::test]
    async fn simulate_mode_runs_and_persists() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config::parse(&format!(
            "mode = \"simulate\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 1000\n[sim]\nspeed = 100.0\nstart = \"2026-06-21T12:00:00+02:00\"\n",
            crate::toml_path(d.path().join("state"))
        ))
        .unwrap();
        let r = start(cfg, &NoDrivers).await.unwrap();
        until(|| r.rt.lock().unwrap().tick >= 5).await;
        let (code, body) = http_get(r.addr, "/healthz").await;
        assert_eq!(code, 200, "{body}");
        let (code, body) = http_get(r.addr, "/api/state").await;
        assert_eq!(code, 200);
        assert!(body.contains("\"mode\":\"simulate\""));
        assert!(body.contains("2026-06-21T12:00:0"));
        r.stop().await;
        assert!(d.path().join("state/settings.json").exists());
        assert!(d.path().join("state/memory.json").exists());
        let minute = std::fs::read_to_string(d.path().join("state/history/minute/2026-06-21.jsonl")).unwrap();
        assert!(minute.starts_with("{\"series\":[\"p1_w\""), "the open minute is flushed at stop: {minute}");
    }

    /// History can be off; `seed_from_ha` seeds an empty store in the background (here against
    /// an HA that is not there, which is logged and does not stop the start).
    #[tokio::test]
    async fn history_off_or_seeded() {
        let d = tempfile::tempdir().unwrap();
        let base =
            format!("mode = \"simulate\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n", crate::toml_path(d.path()));
        let r =
            start(Config::parse(&format!("{base}[history]\nenabled = false\n")).unwrap(), &NoDrivers).await.unwrap();
        assert!(r.history.is_none());
        r.stop().await;
        let token = write(d.path(), "token", "x");
        let seeded = format!(
            "{base}[ha]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"{token}\"\n[history]\nseed_from_ha = true\n"
        );
        let r = start(Config::parse(&seeded).unwrap(), &NoDrivers).await.unwrap();
        assert!(r.history.is_some());
        r.stop().await;
        let bad_token = format!(
            "{base}[ha]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"/nonexistent/t\"\n[history]\nseed_from_ha = true\n"
        );
        assert!(
            start(Config::parse(&bad_token).unwrap(), &NoDrivers).await.err().unwrap().0.contains("/nonexistent/t")
        );
        std::fs::write(d.path().join("blocker"), "").unwrap();
        let other = tempfile::tempdir().unwrap();
        let blocked = format!(
            "mode = \"simulate\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n[history]\ndir = \"{}\"\n",
            crate::toml_path(other.path()),
            crate::toml_path(d.path().join("blocker"))
        );
        assert!(start(Config::parse(&blocked).unwrap(), &NoDrivers).await.is_err());
    }

    /// Shadow mode against a fake HA: seeds Settings and Memory from HA on first start,
    /// mirrors states, compares, and never sends a service call.
    #[tokio::test]
    async fn shadow_mode_seeds_from_ha_and_never_calls_services() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/ha-states-2026-11-07.json")).unwrap();
        let (done_tx, mut done) = mpsc::unbounded_channel::<()>();
        let (url, _) = fake_ha(fixture, move |_, mut ws| {
            let done_tx = done_tx.clone();
            async move {
                push(
                    &mut ws,
                    json!({"id": 1, "type": "event", "event": {"data": {
                "entity_id": "select.smartevse_1234_mode",
                "new_state": {"entity_id": "select.smartevse_1234_mode", "state": "Off"}}}}),
                )
                .await;
                while recv(&mut ws).await.is_some() {}
                done_tx.send(()).unwrap();
            }
        })
        .await;
        let d = tempfile::tempdir().unwrap();
        let token = write(d.path(), "token", "good-token\n");
        let cfg = Config::parse(&format!(
            "mode = \"shadow\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 50\n[ha]\nurl = \"{url}\"\ntoken_file = \"{token}\"\nstart_wait_s = 5\n[mqtt]\nhost = \"127.0.0.1\"\nport = 1\n",
            crate::toml_path(d.path())
        ))
        .unwrap();
        let r = start(cfg, &NoDrivers).await.unwrap();
        {
            let rt = r.rt.lock().unwrap();
            assert_eq!(rt.engine.memory.peak_guard_interventies, 2880, "seeded from HA");
            assert_eq!(rt.engine.settings.batterij_reservering_w, 700.0);
            let mt = &rt.engine.memory.meters;
            assert_eq!(mt.batterij_ontladen_energie.total, Some(750.288), "meters seeded from HA");
            assert_eq!(mt.netimport_dag.value, Some(20.523));
        }
        until(|| r.rt.lock().unwrap().shadow.as_ref().unwrap().stats().ticks >= 3).await;
        let (code, body) = http_get(r.addr, "/api/shadow").await;
        assert_eq!(code, 200);
        assert!(body.contains("batterij_setpoint_w"));
        assert!(r.ha.as_ref().unwrap().lock().unwrap().connected);
        // Shadow mode has no path to HA services: every device is on the log actuator.
        assert!(r.rt.lock().unwrap().router.describe().values().all(|a| a == "log"));
        r.stop().await;
        done.recv().await.unwrap();
    }

    /// Control mode with HA actuators sends commands as service calls; the HA snapshot is not
    /// awaited when the state dir already has state; ntfy notifications are configured.
    #[tokio::test]
    async fn control_mode_wires_ha_actuators_and_ntfy() {
        let (url, conns) = fake_ha(json!([]), |_, mut ws| async move { while recv(&mut ws).await.is_some() {} }).await;
        let d = tempfile::tempdir().unwrap();
        let token = write(d.path(), "token", "good-token");
        let ntfy_token = write(d.path(), "ntfy", "tk");
        write(d.path(), "settings.json", "{}");
        write(d.path(), "memory.json", "{}");
        let cfg = Config::parse(&format!(
            "mode = \"control\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n[ha]\nurl = \"{url}\"\ntoken_file = \"{token}\"\n[actuators]\ndefault = \"ha\"\n[notify]\nkind = \"ntfy\"\nurl = \"http://127.0.0.1:1/t\"\ntoken_file = \"{ntfy_token}\"\n",
            crate::toml_path(d.path())
        ))
        .unwrap();
        let r = start(cfg, &NoDrivers).await.unwrap();
        assert_eq!(r.rt.lock().unwrap().notifier.name(), "ntfy");
        assert_eq!(r.rt.lock().unwrap().router.describe()[&Device::Evse], "ha");
        until(|| conns.load(std::sync::atomic::Ordering::SeqCst) >= 1).await;
        r.stop().await;
    }

    /// `driver` selections fail clearly when the factory has no driver; a missing token file or a
    /// busy port fails start-up.
    #[tokio::test]
    async fn startup_errors() {
        let d = tempfile::tempdir().unwrap();
        let base = format!("listen = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n", crate::toml_path(d.path()));
        let e = |text: String| async move { start(Config::parse(&text).unwrap(), &NoDrivers).await.err().unwrap().0 };
        let hw = "[devices.homewizard]\naddress = \"127.0.0.1\"\nserial = \"x\"\ntoken_file = \"t\"\n";
        assert!(e(format!("mode = \"simulate\"\n{base}[sources]\np1 = \"driver\"\n{hw}")).await.contains("source P1"));
        let evse = "[devices.smartevse]\nhost = \"127.0.0.1\"\nread_only = false\n";
        assert!(
            e(format!("mode = \"simulate\"\n{base}[actuators]\nevse = \"driver\"\n{evse}"))
                .await
                .contains("actuator Evse")
        );
        let no_token =
            format!("mode = \"shadow\"\n{base}[ha]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"/nonexistent/t\"\n");
        assert!(e(no_token).await.contains("/nonexistent/t"));
        let busy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let taken = format!(
            "mode = \"simulate\"\nlisten = \"{}\"\nstate_dir = \"{}\"\n",
            busy.local_addr().unwrap(),
            crate::toml_path(d.path())
        );
        assert!(!e(taken).await.is_empty());
        let mqtt_pw = format!("mode = \"simulate\"\n{base}[mqtt]\nhost = \"h\"\npassword_file = \"/nonexistent/pw\"\n");
        assert!(e(mqtt_pw).await.contains("/nonexistent/pw"));
        let bad_ntfy = format!(
            "mode = \"control\"\n{base}[sources]\ndefault = \"sim\"\n[notify]\nkind = \"ntfy\"\nurl = \"geen-url\"\n"
        );
        assert!(e(bad_ntfy).await.contains("topic"));
        let file = d.path().join("file");
        std::fs::write(&file, "").unwrap();
        let bad_dir = format!("mode = \"simulate\"\nstate_dir = \"{}\"\n", crate::toml_path(file.join("sub")));
        assert!(e(bad_dir).await.contains("sub"));
        std::fs::write(d.path().join("settings.json"), "kapot").unwrap();
        assert!(e(format!("mode = \"simulate\"\n{base}")).await.contains("settings.json"));
    }

    /// Standalone: control mode with no `[ha]` and no devices starts, every section is
    /// unavailable and nothing talks to HA.
    #[tokio::test]
    async fn standalone_starts_without_ha() {
        let d = tempfile::tempdir().unwrap();
        let cfg = Config::parse(&format!(
            "mode = \"control\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 20\n",
            crate::toml_path(d.path())
        ))
        .unwrap();
        let r = start(cfg, &NoDrivers).await.unwrap();
        until(|| r.rt.lock().unwrap().tick >= 2).await;
        assert!(r.ha.is_none());
        let snap = r.rt.lock().unwrap().snapshot_rx().borrow().clone();
        assert!(snap.sources.values().all(|s| s == "unavailable"), "{:?}", snap.sources);
        assert_eq!(snap.world.p1.power_w, None);
        assert!(snap.settings_editable);
        assert_eq!(snap.settings_source, "local");
        r.stop().await;
    }

    /// `[ha] import_only`: an empty state dir is filled from one read-only `get_states` at the
    /// first start; an HA that cannot be read leaves the defaults.
    #[tokio::test]
    async fn first_start_imports_from_ha_once() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/ha-states-2026-11-07.json")).unwrap();
        let (url, conns) = fake_ha(fixture, |_, mut ws| async move { while recv(&mut ws).await.is_some() {} }).await;
        let d = tempfile::tempdir().unwrap();
        let token = write(d.path(), "token", "good-token");
        let cfg = |url: &str, dir: &Path| {
            Config::parse(&format!(
                "mode = \"control\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n[ha]\nurl = \"{url}\"\ntoken_file = \"{token}\"\nimport_only = true\nstart_wait_s = 2\n",
                crate::toml_path(dir)
            ))
            .unwrap()
        };
        let r = start(cfg(&url, &d.path().join("a")), &NoDrivers).await.unwrap();
        assert_eq!(r.rt.lock().unwrap().engine.memory.peak_guard_interventies, 2880, "imported");
        assert!(r.ha.is_none(), "no live HA connection");
        r.stop().await;
        // The state dir now has the state: a restart does not read HA again.
        let r = start(cfg(&url, &d.path().join("a")), &NoDrivers).await.unwrap();
        assert_eq!(conns.load(std::sync::atomic::Ordering::SeqCst), 1);
        r.stop().await;
        let r = start(cfg("http://127.0.0.1:1", &d.path().join("b")), &NoDrivers).await.unwrap();
        assert_eq!(r.rt.lock().unwrap().engine.memory.peak_guard_interventies, 0, "defaults");
        r.stop().await;
    }

    /// `[mqtt_publish]`: the running binary publishes its entities and the summary on a
    /// broker, takes `/set` through the runtime, and says `offline` when it stops; a missing
    /// password file fails the start.
    #[tokio::test]
    async fn the_mqtt_bridge_runs_with_the_binary() {
        use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
        let broker = energie_drivers::sim::mqtt::Broker::start(None).await;
        let (obs, mut ev) = AsyncClient::new(MqttOptions::new("obs-app", "127.0.0.1", broker.addr.port()), 100);
        obs.subscribe("energie/#", QoS::AtLeastOnce).await.unwrap();
        let seen: Arc<Mutex<BTreeMap<String, String>>> = Arc::default();
        let s2 = seen.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(Event::Incoming(Packet::Publish(m))) = ev.poll().await {
                    s2.lock().unwrap().insert(m.topic.clone(), String::from_utf8_lossy(&m.payload).into_owned());
                }
            }
        });
        let d = tempfile::tempdir().unwrap();
        let pw = write(d.path(), "pw", "geheim");
        let base = format!(
            "mode = \"simulate\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\ntick_ms = 50\n[mqtt_publish]\nallow_set = true\nenabled = true\nbroker = \"127.0.0.1\"\nport = {}\nusername = \"energie\"\n",
            crate::toml_path(d.path()),
            broker.addr.port()
        );
        let r = start(Config::parse(&format!("{base}password_file = \"{pw}\"\n")).unwrap(), &NoDrivers).await.unwrap();
        let has = |t: &str, v: &str| seen.lock().unwrap().get(t).is_some_and(|x| x == v);
        until(|| has("energie/status", "online") && seen.lock().unwrap().contains_key("energie/summary")).await;
        until(|| has("energie/setting/peak_guard_actief", "ON")).await;
        obs.publish("energie/setting/peak_guard_actief/set", QoS::AtLeastOnce, false, "OFF").await.unwrap();
        until(|| !r.rt.lock().unwrap().engine.settings.peak_guard_actief).await;
        until(|| has("energie/setting/peak_guard_actief", "OFF")).await;
        r.stop().await;
        until(|| has("energie/status", "offline")).await;
        let missing = format!("{base}password_file = \"/nonexistent/pw\"\n");
        assert!(start(Config::parse(&missing).unwrap(), &NoDrivers).await.err().unwrap().0.contains("/nonexistent/pw"));
    }

    /// Without an HA snapshot within the wait, the binary starts on defaults.
    #[tokio::test]
    async fn starts_on_defaults_when_ha_is_unreachable() {
        let d = tempfile::tempdir().unwrap();
        let token = write(d.path(), "token", "x");
        let cfg = Config::parse(&format!(
            "mode = \"shadow\"\nlisten = \"127.0.0.1:0\"\nstate_dir = \"{}\"\n[ha]\nurl = \"http://127.0.0.1:1\"\ntoken_file = \"{token}\"\nstart_wait_s = 0\n",
            crate::toml_path(d.path())
        ))
        .unwrap();
        let r = run(cfg, &NoDrivers, Box::pin(async {})).await;
        assert!(r.is_ok());
        let s: energie_core::settings::Settings =
            serde_json::from_slice(&std::fs::read(d.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(s, Default::default());
    }
}
