//! The synchronous heart of the binary: one [`Runtime::step`] per tick.
//!
//! assemble the World from the sources → (lockstep: adopt HA's Settings and Memory) →
//! `Engine::tick` → route commands (shadow: log only) and notifications → shadow comparison →
//! throttled persistence → publish a [`Snapshot`] for the dashboard, with the entity registry
//! ([`crate::entities`]) rebuilt from the same tick.
//!
//! No I/O happens here except the persistence write and the mismatch log; everything that
//! talks to the network lives behind the source/actuator/notifier traits.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use energie_core::command::Command;
use energie_core::derived::Derived;
use energie_core::engine::Engine;
use energie_core::memory::Memory;
use energie_core::settings::Settings;
use energie_core::time::Ts;
use energie_core::world::World;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::watch;

use crate::config::{Mode, SettingsSource};
use crate::control::{Control, ControlView};
use crate::entities::{self, Inputs, PublishRule, SetError, Target};
use crate::error::{Error, Result};
use crate::ha::mapping::{memory_from, settings_from};
use crate::ha::ws::SharedStore;
use crate::history::{self, SharedHistory};
use crate::mqtt::SharedInbox;
use crate::notify::{Notification, Notifier};
use crate::persist::Persister;
use crate::plug::{Device, DeviceView, DriverHealth, Router, Section, Sources};
use crate::shadow::{Shadow, ShadowStats};
use energie_drivers::entity::{Entity, Value as EntityValue};

const KEEP_COMMANDS: usize = 50;
const KEEP_NOTIFICATIONS: usize = 30;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommandRecord {
    pub at: String,
    pub command: Command,
    /// Sent to an actuator (false in shadow mode).
    pub actuated: bool,
    pub error: Option<String>,
}

/// The hidden state worth showing.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MemorySummary {
    pub batterij_setpoint_bias: f64,
    pub batterij_net_laden: bool,
    pub soluna_absorptie_bezig: bool,
    pub ev_hoger_laadstil: bool,
    pub airco_dump_sessie_bezig: bool,
    pub smartevse_plug_laatst_bekend: String,
    pub peak_guard_interventies: u64,
    pub buzz_laadweigering: u64,
    pub nachtdekking: f64,
    pub nacht_batterij_kwh: f64,
    pub nacht_import_kwh: f64,
    pub nacht_duur_uren: f64,
    pub capaciteit_piekhistoriek: String,
    pub steering_volgende_update: Option<String>,
    pub automations_triggered: usize,
}

impl MemorySummary {
    pub fn of(m: &Memory) -> Self {
        MemorySummary {
            batterij_setpoint_bias: m.batterij_setpoint_bias,
            batterij_net_laden: m.batterij_net_laden,
            soluna_absorptie_bezig: m.soluna_absorptie_bezig,
            ev_hoger_laadstil: m.ev_hoger_laadstil,
            airco_dump_sessie_bezig: m.airco_dump_sessie_bezig,
            smartevse_plug_laatst_bekend: m.smartevse_plug_laatst_bekend.clone(),
            peak_guard_interventies: m.peak_guard_interventies,
            buzz_laadweigering: m.buzz_laadweigering,
            nachtdekking: m.nachtdekking,
            nacht_batterij_kwh: m.nacht_batterij_kwh,
            nacht_import_kwh: m.nacht_import_kwh,
            nacht_duur_uren: m.nacht_duur_uren,
            capaciteit_piekhistoriek: m.capaciteit_piekhistoriek.clone(),
            steering_volgende_update: m.steering_volgende_update.map(|t| t.to_rfc3339()),
            automations_triggered: m.last_triggered.len(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LinkStatus {
    pub connected: bool,
    pub entities: usize,
    pub snapshots: u64,
    pub events: u64,
}

/// What the dashboard and the API show.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub now: Option<String>,
    pub mode: &'static str,
    pub tick: u64,
    pub world: World,
    pub derived: Derived,
    pub settings: Settings,
    /// False while HA owns the settings (`settings_source = "ha"`).
    pub settings_editable: bool,
    /// `local` or `ha`.
    pub settings_source: &'static str,
    pub memory: MemorySummary,
    /// Newest first.
    pub commands: Vec<CommandRecord>,
    /// Newest first.
    pub notifications: Vec<Notification>,
    pub ha: Option<LinkStatus>,
    pub mqtt: Option<LinkStatus>,
    pub sources: BTreeMap<Section, String>,
    pub actuators: BTreeMap<Device, String>,
    /// Native driver health by driver name (`driver:solis`, ...).
    pub drivers: BTreeMap<String, DriverHealth>,
    pub shadow: Option<ShadowStats>,
    /// Control mode: the switches, the dry run and the latest audited commands.
    pub control: Option<ControlView>,
    pub persist_writes: u64,
    /// Every entity with its value (`/api/entities`; not part of `/api/state`).
    #[serde(skip)]
    pub entities: Arc<Vec<Entity>>,
    /// When this snapshot was made (wall clock, for `/healthz`).
    #[serde(skip)]
    pub made: Option<Instant>,
}

pub struct Runtime {
    /// No home battery in this installation ([`crate::config::Config::no_battery`]).
    pub no_battery: bool,
    pub mode: Mode,
    pub engine: Engine,
    pub sources: Sources,
    pub router: Router,
    pub notifier: Arc<dyn Notifier>,
    pub ha: Option<SharedStore>,
    pub mqtt: Option<SharedInbox>,
    /// Shadow lockstep: adopt HA's Memory (helpers, latches, counters, meters) every tick.
    pub lockstep: bool,
    /// Adopt HA's Settings (the `input_*` helpers) every tick: `settings_source = "ha"`.
    pub settings_from_ha: bool,
    pub settings_source: SettingsSource,
    pub shadow: Option<Shadow>,
    pub persister: Option<Persister>,
    /// The native device instances, for their entities and health.
    pub devices: Vec<Arc<dyn DeviceView>>,
    pub publish_rules: Vec<PublishRule>,
    pub history: Option<SharedHistory>,
    /// The gate of control mode (`None` in shadow and simulate mode).
    pub control: Option<Control>,
    pub tick: u64,
    last_now: Option<Ts>,
    last: Option<(World, Derived)>,
    commands: VecDeque<CommandRecord>,
    notifications: VecDeque<Notification>,
    tx: watch::Sender<Arc<Snapshot>>,
}

pub struct Parts {
    pub mode: Mode,
    pub engine: Engine,
    pub sources: Sources,
    pub router: Router,
    pub notifier: Arc<dyn Notifier>,
    pub ha: Option<SharedStore>,
    pub mqtt: Option<SharedInbox>,
    /// `[shadow] lockstep`.
    pub lockstep: bool,
    /// Who owns the settings and the memory: HA (adopted every tick) or this binary.
    pub settings_source: SettingsSource,
    pub shadow: Option<Shadow>,
    pub persister: Option<Persister>,
    pub devices: Vec<Arc<dyn DeviceView>>,
    /// `[[publish]]`.
    pub publish: Vec<PublishRule>,
    pub history: Option<SharedHistory>,
    /// The gate of control mode; only used in [`Mode::Control`].
    pub control: Option<Control>,
}

impl Runtime {
    pub fn new(p: Parts) -> (Self, watch::Receiver<Arc<Snapshot>>) {
        let (tx, rx) = watch::channel(Arc::new(Snapshot::default()));
        let rt = Runtime {
            no_battery: false,
            mode: p.mode,
            engine: p.engine,
            sources: p.sources,
            router: p.router,
            notifier: p.notifier,
            ha: p.ha,
            mqtt: p.mqtt,
            lockstep: p.lockstep && p.mode == Mode::Shadow && p.settings_source == SettingsSource::Ha,
            settings_from_ha: p.settings_source == SettingsSource::Ha && (p.mode != Mode::Shadow || p.lockstep),
            settings_source: p.settings_source,
            shadow: p.shadow,
            persister: p.persister,
            devices: p.devices,
            publish_rules: p.publish,
            history: p.history,
            control: p.control.filter(|_| p.mode == Mode::Control),
            tick: 0,
            last_now: None,
            last: None,
            commands: VecDeque::new(),
            notifications: VecDeque::new(),
            tx,
        };
        rt.publish();
        (rt, rx)
    }

    /// A new receiver of the published snapshots.
    pub fn snapshot_rx(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.tx.subscribe()
    }

    /// Whether HA is connected and has sent a snapshot.
    fn ha_ready(&self) -> bool {
        self.ha.as_ref().is_some_and(|s| {
            let s = s.lock().expect("store lock");
            s.connected && s.snapshots > 0
        })
    }

    /// One control step at `now`.
    pub fn step(&mut self, now: Ts) {
        let mut world = self.sources.assemble(now);
        if self.no_battery {
            world.without_battery();
        }
        if (self.settings_from_ha || self.lockstep) && self.ha_ready() {
            let store = self.ha.as_ref().expect("ha_ready").lock().expect("store lock");
            if self.settings_from_ha {
                self.engine.settings = settings_from(&*store, self.engine.settings.clone());
            }
            if self.lockstep {
                self.engine.memory = memory_from(&*store, self.engine.memory.clone());
                if let Some(sh) = &mut self.shadow {
                    sh.adopt_meters(&mut self.engine.memory.meters, &*store, now);
                }
            }
        }
        let before = self.engine.memory.clone();
        let out = self.engine.tick(now, &world);
        self.tick += 1;
        self.last_now = Some(now);
        for cmd in &out.commands {
            self.dispatch(now, cmd);
        }
        self.check_control(now, &world);
        if let Some(sh) = &mut self.shadow {
            let mqtt = self.mqtt.as_ref().map(|i| i.lock().expect("inbox lock").drain()).unwrap_or_default();
            if let Some(store) = &self.ha {
                let mut st = store.lock().expect("store lock");
                let changes = st.drain_changes();
                if st.connected {
                    sh.observe(now, &out.derived, &out.commands, &before, &self.engine.memory, &*st, &changes, &mqtt);
                }
            }
        }
        self.persist(now, false);
        self.last = Some((world, out.derived));
        self.publish();
        if let Some(h) = &self.history {
            let (world, derived) = self.last.as_ref().expect("set above");
            let entities = self.tx.borrow().entities.clone();
            history::series::record(h, now, world, derived, &self.engine.memory, &entities);
            history::series::forecasts(h, &self.devices);
        }
    }

    fn dispatch(&mut self, now: Ts, cmd: &Command) {
        let at = now.to_rfc3339();
        match cmd {
            Command::Notify { title, message, high_priority } => self.notification(Notification {
                at,
                title: title.clone(),
                message: message.clone(),
                high_priority: *high_priority,
                persistent: false,
                id: None,
            }),
            Command::Persistent { title, message, id } => self.notification(Notification {
                at,
                title: title.clone(),
                message: message.clone(),
                high_priority: false,
                persistent: true,
                id: id.clone(),
            }),
            _ => {
                let rec = if self.mode == Mode::Shadow {
                    tracing::debug!(?cmd, "shadow: not actuated");
                    CommandRecord { at, command: cmd.clone(), actuated: false, error: None }
                } else if let Some(c) = &mut self.control {
                    let before = self.last.as_ref().map(|(w, _)| w.clone()).unwrap_or_default();
                    let error = c.dispatch(now, cmd, &before, &self.router).err();
                    CommandRecord { at, command: cmd.clone(), actuated: error.is_none(), error }
                } else {
                    let error = self.router.route(now, cmd).err().map(|e| e.0);
                    if let Some(e) = &error {
                        tracing::warn!(?cmd, error = %e, "command failed");
                    }
                    CommandRecord { at, command: cmd.clone(), actuated: true, error }
                };
                push_bounded(&mut self.commands, rec, KEEP_COMMANDS);
            }
        }
    }

    /// Control mode: collect the outcomes and read-backs, raise the alarms.
    fn check_control(&mut self, now: Ts, world: &World) {
        let Some(c) = &mut self.control else { return };
        for (title, message) in c.poll(now, world) {
            self.notification(Notification {
                at: now.to_rfc3339(),
                title,
                message,
                high_priority: true,
                persistent: false,
                id: None,
            });
        }
    }

    /// Switch actuation on or off (`None`: everything) from the dashboard, the API or an
    /// entity. Refused outside control mode.
    pub fn set_control(&mut self, device: Option<Device>, on: bool) -> Result<()> {
        let c = self.control.as_mut().ok_or_else(|| Error::new("sturing bestaat alleen in control-modus"))?;
        c.set(device, on);
        self.publish();
        Ok(())
    }

    /// Shutdown: send the configured release commands (`[control] release_on_shutdown`).
    pub fn release(&mut self, now: Ts) {
        let world = self.last.as_ref().map(|(w, _)| w.clone()).unwrap_or_default();
        if let Some(c) = &mut self.control {
            c.release(now, &world, &self.router);
        }
    }

    /// Commands whose outcome has not come back yet (control mode).
    pub fn control_unreported(&self) -> usize {
        self.control.as_ref().map_or(0, Control::unreported)
    }

    /// Shutdown: write what is still pending to the audit log.
    pub fn close_control(&mut self, now: Ts) {
        let world = self.last.as_ref().map(|(w, _)| w.clone()).unwrap_or_default();
        if let Some(c) = &mut self.control {
            for (title, message) in c.close(now, &world) {
                tracing::warn!(title, message, "control alarm at shutdown");
            }
        }
    }

    fn notification(&mut self, n: Notification) {
        self.notifier.notify(&n);
        if n.id.is_some() {
            // HA's `notification_id`: the new note replaces the old one.
            self.notifications.retain(|old| old.id != n.id);
        }
        push_bounded(&mut self.notifications, n, KEEP_NOTIFICATIONS);
    }

    fn persist(&mut self, now: Ts, force: bool) {
        if let Some(p) = &mut self.persister
            && let Err(e) = p.save(now, &self.engine.settings, &self.engine.memory, force)
        {
            tracing::error!(error = %e, "persisting state failed");
        }
    }

    /// The settings may be changed from the UI, the API and MQTT (not while HA owns them).
    pub fn settings_editable(&self) -> bool {
        !self.settings_from_ha
    }

    /// Apply a partial settings change from the UI (`{"field": value, ...}` in the serde
    /// shape of `Settings`) and persist it at once.
    pub fn apply_settings(&mut self, now: Ts, patch: &Value) -> Result<Settings> {
        if !self.settings_editable() {
            return Err(Error::new("instellingen komen uit Home Assistant (settings_source = ha, shadow lockstep)"));
        }
        let patch = patch.as_object().ok_or_else(|| Error::new("verwacht een JSON-object"))?;
        let mut cur = serde_json::to_value(&self.engine.settings)?;
        let obj = cur.as_object_mut().expect("Settings serialise as an object");
        for (k, v) in patch {
            if !obj.contains_key(k) {
                return Err(Error::new(format!("onbekende instelling: {k}")));
            }
            obj.insert(k.clone(), v.clone());
        }
        let new: Settings = serde_json::from_value(cur)?;
        tracing::info!(?patch, "settings changed from the dashboard");
        self.engine.settings = new.clone();
        self.persist(self.last_now.unwrap_or(now), true);
        self.publish();
        Ok(new)
    }

    /// Set entity `id` from the dashboard or a bridge: a setting is applied as a settings
    /// change, a World field becomes its command and is routed like the engine's own (the
    /// control exists only where the device has a controller, never in shadow mode).
    pub fn set_entity(&mut self, now: Ts, id: &str, v: &EntityValue) -> std::result::Result<(), SetError> {
        let current = self.tx.borrow().entities.clone();
        match entities::target(&current, id, v)? {
            Target::Settings(patch) => {
                self.apply_settings(now, &patch).map_err(|e| SetError::Rejected(e.0))?;
            }
            Target::Command(cmd) => {
                tracing::info!(?cmd, entity = id, "command from an entity");
                self.dispatch(now, &cmd);
                self.publish();
            }
            Target::Control { key, on } => {
                let device = Control::switch_of(&key).ok_or(SetError::Unknown)?;
                self.set_control(device, on).map_err(|e| SetError::Rejected(e.0))?;
            }
            Target::Device { id: device, key, value } => {
                let d = self.devices.iter().find(|d| d.id() == device).ok_or(SetError::Unknown)?;
                tracing::info!(entity = id, ?value, "device entity set");
                d.set(&key, &value).map_err(|e| SetError::Rejected(e.0))?;
            }
        }
        Ok(())
    }

    /// Final write at shutdown.
    pub fn shutdown(&mut self, now: Ts) {
        self.persist(now, true);
    }

    fn link(&self) -> Option<LinkStatus> {
        self.ha.as_ref().map(|s| {
            let s = s.lock().expect("store lock");
            LinkStatus { connected: s.connected, entities: s.entities.len(), snapshots: s.snapshots, events: s.events }
        })
    }

    fn publish(&self) {
        let (world, derived) = self.last.clone().unwrap_or_default();
        let actuators = self.router.describe();
        let settable = |d: Device| self.mode != Mode::Shadow && actuators.get(&d).is_some_and(|a| a != "log");
        let mut entities = entities::collect(&Inputs {
            world: &world,
            derived: &derived,
            settings: &self.engine.settings,
            settings_editable: self.settings_editable(),
            memory: &self.engine.memory,
            settable: &settable,
            devices: &self.devices,
            rules: &self.publish_rules,
        });
        if let Some(c) = &self.control {
            let mut own = c.entities();
            entities::apply_rules(&mut own, &self.publish_rules);
            entities.extend(own);
        }
        let snap = Snapshot {
            now: self.last_now.map(|t| t.to_rfc3339()),
            mode: self.mode.label(),
            tick: self.tick,
            world,
            derived,
            settings: self.engine.settings.clone(),
            settings_editable: self.settings_editable(),
            settings_source: self.settings_source.label(),
            memory: MemorySummary::of(&self.engine.memory),
            commands: self.commands.iter().rev().cloned().collect(),
            notifications: self.notifications.iter().rev().cloned().collect(),
            ha: self.link(),
            mqtt: self.mqtt.as_ref().map(|i| {
                let i = i.lock().expect("inbox lock");
                LinkStatus { connected: i.connected, entities: 0, snapshots: 0, events: i.received }
            }),
            sources: self.sources.describe(),
            actuators,
            drivers: {
                let mut d = self.sources.health();
                d.extend(self.router.health());
                d.extend(self.devices.iter().map(|v| (format!("driver:{}", v.id()), v.health())));
                d
            },
            shadow: self.shadow.as_ref().map(Shadow::stats),
            control: self.control.as_ref().map(Control::view),
            persist_writes: self.persister.as_ref().map_or(0, |p| p.writes),
            entities: Arc::new(entities),
            made: Some(Instant::now()),
        };
        self.tx.send_replace(Arc::new(snap));
    }
}

fn push_bounded<T>(q: &mut VecDeque<T>, v: T, cap: usize) {
    if q.len() >= cap {
        q.pop_front();
    }
    q.push_back(v);
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::{ShadowConfig, SimConfig};
    use crate::ha::entity::{EntityState, HaStore};
    use crate::notify::LogNotifier;
    use crate::plug::tests::{Recorder, t0};
    use crate::plug::{Actuator, LogActuator, WorldSource};
    use crate::shadow::log::MismatchLog;
    use crate::sim::{House, SimHandle};
    use chrono::Duration;
    use energie_core::automations::{Automation, Cx};
    use energie_core::settings::NetNiveau;
    use serde_json::json;
    use std::sync::Mutex;

    /// Emits a fixed list of commands on every tick, and flips the net-laden latch.
    pub(crate) struct Emit(pub Vec<Command>);
    impl Automation for Emit {
        fn id(&self) -> &'static str {
            "emit"
        }
        fn step(&mut self, cx: &mut Cx) {
            for c in &self.0 {
                cx.send(c.clone());
            }
            cx.m.batterij_net_laden = !cx.m.batterij_net_laden;
            cx.mark("emit");
        }
    }

    pub(crate) fn sim_parts(mode: Mode, cmds: Vec<Command>) -> (Parts, Arc<Recorder>) {
        let sim = SimHandle::new(House::new(SimConfig::default(), t0()));
        let per_section = Section::ALL.iter().map(|s| (*s, Arc::new(sim.clone()) as Arc<dyn WorldSource>)).collect();
        let rec = Arc::new(Recorder::default());
        let mut per_device: BTreeMap<Device, Arc<dyn Actuator>> =
            Device::ALL.iter().map(|d| (*d, rec.clone() as Arc<dyn Actuator>)).collect();
        per_device.insert(Device::Prices, Arc::new(LogActuator));
        let engine = Engine::with_automations(Settings::default(), Memory::default(), vec![Box::new(Emit(cmds))]);
        let parts = Parts {
            mode,
            engine,
            sources: Sources { per_section },
            router: Router { per_device },
            notifier: Arc::new(LogNotifier),
            ha: None,
            mqtt: None,
            lockstep: false,
            settings_source: if mode == Mode::Shadow { SettingsSource::Ha } else { SettingsSource::Local },
            shadow: None,
            persister: None,
            devices: Vec::new(),
            publish: Vec::new(),
            history: None,
            control: None,
        };
        (parts, rec)
    }

    /// With the control gate the runtime audits every device command, raises a notification
    /// when one does not read back, exposes the switches as entities and in the snapshot, and
    /// writes what is still pending at shutdown.
    #[test]
    fn control_gate_in_the_runtime() {
        use crate::control::{Control, ControlConfig};
        let dir = tempfile::tempdir().unwrap();
        let (mut parts, rec) = sim_parts(Mode::Control, vec![Command::EvseCurrentDeciA(145)]);
        let cfg = ControlConfig { verify_s: 2, alarm_interval_s: 1, ..Default::default() };
        parts.control = Some(Control::new(cfg, dir.path()));
        let (mut rt, rx) = Runtime::new(parts);
        for s in 0..4 {
            rt.step(t0() + Duration::seconds(s));
        }
        assert_eq!(rec.0.lock().unwrap().len(), 4);
        let snap = rx.borrow().clone();
        let alarm = snap.notifications.iter().find(|n| n.title == "Sturing: schrijven niet gelukt").unwrap();
        assert!(alarm.high_priority && alarm.message.contains("charger_buzz_override_a"));
        let c = snap.control.clone().unwrap();
        assert!(c.enabled && c.stats.failed >= 1);
        assert!(snap.entities.iter().any(|e| e.id == "control_enabled" && e.control.is_some()));
        // The kill switch as an entity, then as a call.
        rt.set_entity(t0(), "control_enabled", &EntityValue::Bool(false)).unwrap();
        rt.step(t0() + Duration::seconds(5));
        assert_eq!(rx.borrow().commands[0].error.as_deref(), Some("blocked: control is switched off"));
        assert_eq!(rec.0.lock().unwrap().len(), 4);
        rt.set_control(Some(Device::Evse), true).unwrap();
        rt.set_control(None, true).unwrap();
        rt.step(t0() + Duration::seconds(6));
        assert_eq!(rec.0.lock().unwrap().len(), 5);
        assert_eq!(rt.control_unreported(), 0);
        rt.release(t0() + Duration::seconds(7));
        assert_eq!(rec.0.lock().unwrap().last(), Some(&Command::SolisRcMode(energie_core::world::RcMode::None)));
        rt.close_control(t0() + Duration::seconds(30));
        assert_eq!(rt.control.as_ref().unwrap().pending(), 0);
        let audit = std::fs::read_to_string(dir.path().join("control-audit.jsonl")).unwrap();
        assert!(audit.lines().count() >= 6, "{audit}");
    }

    /// Outside control mode there is no gate: the switches are refused, the release and the
    /// shutdown flush do nothing.
    #[test]
    fn no_gate_outside_control_mode() {
        let (mut parts, _rec) = sim_parts(Mode::Simulate, vec![]);
        parts.control = Some(crate::control::Control::new(Default::default(), tempfile::tempdir().unwrap().path()));
        let (mut rt, rx) = Runtime::new(parts);
        assert!(rt.control.is_none(), "only control mode keeps the gate");
        assert!(rt.set_control(None, false).is_err());
        rt.release(t0());
        rt.close_control(t0());
        assert_eq!(rt.control_unreported(), 0);
        assert!(rx.borrow().control.is_none());
    }

    /// In control mode commands go to their actuator; notifications go to the notifier and
    /// the dashboard list; a failing actuator is recorded, not fatal.
    #[test]
    fn control_mode_actuates_and_records() {
        let (mut parts, rec) = sim_parts(
            Mode::Control,
            vec![
                Command::EvseCurrentDeciA(100),
                Command::SolisRcChargeW(1),
                Command::notify_high("⚡", "piek"),
                Command::persistent("Dagbalans", "ok"),
                Command::persistent_id("heal", "EV cable auto-heal", "12:00"),
                Command::persistent_id("heal", "EV cable auto-heal", "12:30"),
            ],
        );
        parts.router.per_device.remove(&Device::Solis);
        let (mut rt, rx) = Runtime::new(parts);
        rt.step(t0());
        assert_eq!(*rec.0.lock().unwrap(), vec![Command::EvseCurrentDeciA(100)]);
        let snap = rx.borrow().clone();
        assert_eq!(snap.tick, 1);
        assert_eq!(snap.mode, "control");
        assert_eq!(snap.commands.len(), 2);
        assert!(snap.commands[0].error.as_deref().unwrap().contains("Solis"));
        assert!(snap.commands[1].actuated);
        assert_eq!(snap.notifications.len(), 3, "the second `heal` note replaced the first");
        assert_eq!(snap.notifications[0].message, "12:30");
        assert!(snap.notifications[1].persistent);
        assert!(snap.notifications[2].high_priority);
        assert_eq!(snap.world.evse.override_a, Some(6.0), "the world was read before the command");
        assert!(snap.made.is_some());
        assert_eq!(snap.sources[&Section::P1], "sim");
        assert!(snap.settings_editable);
    }

    /// Shadow mode never actuates: commands are only recorded.
    #[test]
    fn shadow_mode_never_actuates() {
        let (parts, rec) = sim_parts(Mode::Shadow, vec![Command::EvseCurrentDeciA(100), Command::CableStop]);
        let (mut rt, rx) = Runtime::new(parts);
        assert_eq!(rt.engine.automation_ids(), vec!["emit"]);
        rt.step(t0());
        rt.step(t0() + Duration::seconds(1));
        assert!(rec.0.lock().unwrap().is_empty());
        let snap = rx.borrow().clone();
        assert_eq!(snap.commands.len(), 4);
        assert!(snap.commands.iter().all(|c| !c.actuated));
    }

    /// The dashboard lists are bounded.
    #[test]
    fn recent_lists_are_bounded() {
        let (parts, _) = sim_parts(Mode::Control, vec![Command::EvseCurrentDeciA(60), Command::notify("a", "b")]);
        let (mut rt, rx) = Runtime::new(parts);
        for i in 0..60 {
            rt.step(t0() + Duration::seconds(i));
        }
        let snap = rx.borrow().clone();
        assert_eq!(snap.commands.len(), KEEP_COMMANDS);
        assert_eq!(snap.notifications.len(), KEEP_NOTIFICATIONS);
        assert_eq!(snap.commands[0].at, (t0() + Duration::seconds(59)).to_rfc3339());
    }

    /// A UI change merges into the settings, is persisted at once and published; unknown
    /// fields and bad values are refused.
    #[test]
    fn settings_from_the_ui_are_applied_and_persisted_at_once() {
        let d = tempfile::tempdir().unwrap();
        let (mut parts, _) = sim_parts(Mode::Simulate, vec![]);
        parts.persister = Some(Persister::new(d.path().into(), 3600));
        let (mut rt, rx) = Runtime::new(parts);
        rt.step(t0());
        let s = rt.apply_settings(t0(), &json!({"ev_net_niveau": "Uit", "peak_guard_actief": false})).unwrap();
        assert_eq!(s.ev_net_niveau, NetNiveau::Uit);
        assert_eq!(rx.borrow().settings.ev_net_niveau, NetNiveau::Uit);
        let on_disk: Settings =
            serde_json::from_slice(&std::fs::read(d.path().join(crate::persist::SETTINGS_FILE)).unwrap()).unwrap();
        assert!(!on_disk.peak_guard_actief);
        assert!(rt.apply_settings(t0(), &json!({"bestaat_niet": 1})).unwrap_err().0.contains("bestaat_niet"));
        assert!(rt.apply_settings(t0(), &json!({"ev_net_niveau": "Turbo"})).is_err());
        assert!(rt.apply_settings(t0(), &json!([1])).is_err());
        rt.shutdown(t0());
    }

    /// A failing persistence write is logged and the tick goes on.
    #[test]
    fn persistence_failures_do_not_stop_the_tick() {
        let d = tempfile::tempdir().unwrap();
        let (mut parts, _) = sim_parts(Mode::Simulate, vec![]);
        parts.persister = Some(Persister::new(d.path().join("weg"), 60));
        let (mut rt, rx) = Runtime::new(parts);
        rt.step(t0());
        assert_eq!(rx.borrow().tick, 1);
        assert_eq!(rx.borrow().persist_writes, 0);
    }

    fn ha_store() -> SharedStore {
        let mut st = HaStore::watching(crate::shadow::commands::act_entities());
        st.replace_all(vec![
            EntityState::new("input_select.ev_net_niveau", "6A-brug"),
            EntityState::new("input_number.batterij_setpoint_bias", "200.0"),
            EntityState::new("input_boolean.batterij_net_laden", "off"),
            EntityState::new("sensor.batterij_setpoint", "0"),
        ]);
        Arc::new(Mutex::new(st))
    }

    /// Shadow lockstep: HA's settings and memory are adopted every tick, the UI is read-only,
    /// and the comparator sees HA's state changes and MQTT messages.
    #[test]
    fn shadow_lockstep_adopts_ha_and_compares() {
        let (mut parts, _) = sim_parts(Mode::Shadow, vec![Command::EvseCurrentDeciA(100)]);
        let store = ha_store();
        let inbox: SharedInbox = Arc::default();
        inbox.lock().unwrap().push(crate::mqtt::MqttMessage {
            at: t0(),
            topic: "SmartEVSE/1234/Set/CurrentOverride".into(),
            payload: "100".into(),
        });
        parts.ha = Some(store.clone());
        parts.mqtt = Some(inbox.clone());
        parts.lockstep = true;
        let cfg = ShadowConfig { time_tolerance_s: 0, ..Default::default() };
        parts.shadow = Some(Shadow::new(&cfg, MismatchLog::new(None, 0)));
        let (mut rt, rx) = Runtime::new(parts);
        rt.step(t0());
        assert_eq!(rt.engine.settings.ev_net_niveau, NetNiveau::Brug6A);
        assert_eq!(rt.engine.memory.batterij_setpoint_bias, 200.0);
        // The Emit automation flipped the latch; HA did the same.
        store.lock().unwrap().apply_change(
            t0(),
            "input_boolean.batterij_net_laden",
            Some(EntityState::new("input_boolean.batterij_net_laden", "on")),
        );
        rt.step(t0() + Duration::seconds(1));
        assert!(inbox.lock().unwrap().messages.is_empty(), "drained");
        assert!(store.lock().unwrap().changes.is_empty(), "drained");
        let snap = rx.borrow().clone();
        assert!(!snap.settings_editable);
        let sh = snap.shadow.clone().unwrap();
        assert_eq!(sh.ticks, 2);
        let ev = sh.commands.iter().find(|k| k.key == "evse.override_a").unwrap();
        assert_eq!((ev.rust, ev.rust_matched, ev.ha, ev.ha_matched), (2, 2, 1, 1));
        assert!(snap.ha.as_ref().unwrap().connected);
        assert_eq!(snap.mqtt.as_ref().unwrap().events, 1);
        assert!(rt.apply_settings(t0(), &json!({"peak_guard_actief": false})).unwrap_err().0.contains("lockstep"));
    }

    /// While HA is down nothing is adopted or compared, but the tick still runs.
    #[test]
    fn shadow_without_ha_connection_still_ticks() {
        let (mut parts, _) = sim_parts(Mode::Shadow, vec![]);
        let store: SharedStore = Arc::new(Mutex::new(HaStore::default()));
        parts.ha = Some(store);
        parts.lockstep = true;
        parts.shadow = Some(Shadow::new(&ShadowConfig::default(), MismatchLog::new(None, 0)));
        let (mut rt, rx) = Runtime::new(parts);
        rt.step(t0());
        assert_eq!(rt.engine.settings, Settings::default());
        assert_eq!(rx.borrow().shadow.as_ref().unwrap().ticks, 0);
        // Shadow without an HA store at all compares nothing either.
        let (mut parts, _) = sim_parts(Mode::Shadow, vec![]);
        parts.shadow = Some(Shadow::new(&ShadowConfig::default(), MismatchLog::new(None, 0)));
        let (mut rt, rx) = Runtime::new(parts);
        rt.step(t0());
        assert_eq!(rx.borrow().shadow.as_ref().unwrap().ticks, 0);
    }

    /// `settings_source = "ha"` outside shadow mode adopts HA's helpers only (the memory stays
    /// the binary's own, HA's automations no longer keep it); `local` in shadow mode adopts
    /// nothing, and the settings are editable.
    #[test]
    fn settings_source_decides_what_is_adopted() {
        let (mut parts, _) = sim_parts(Mode::Control, vec![]);
        parts.ha = Some(ha_store());
        parts.settings_source = SettingsSource::Ha;
        parts.lockstep = true;
        let (mut rt, rx) = Runtime::new(parts);
        rt.step(t0());
        assert_eq!(rt.engine.settings.ev_net_niveau, NetNiveau::Brug6A);
        assert_eq!(rt.engine.memory.batterij_setpoint_bias, 0.0, "memory not adopted");
        let snap = rx.borrow().clone();
        assert!(!snap.settings_editable);
        assert_eq!(snap.settings_source, "ha");
        let (mut parts, _) = sim_parts(Mode::Shadow, vec![]);
        parts.ha = Some(ha_store());
        parts.settings_source = SettingsSource::Local;
        parts.lockstep = true;
        let (mut rt, rx) = Runtime::new(parts);
        rt.step(t0());
        assert_eq!(rt.engine.settings, Settings::default());
        assert_eq!(rt.engine.memory.batterij_setpoint_bias, 0.0);
        assert!(rx.borrow().settings_editable);
        assert_eq!(rx.borrow().settings_source, "local");
    }

    /// The memory summary shows the hidden state the dashboard needs.
    #[test]
    fn memory_summary() {
        let mut m = Memory { steering_volgende_update: Some(t0()), ..Default::default() };
        m.mark_triggered("x", t0());
        let s = MemorySummary::of(&m);
        assert_eq!(s.automations_triggered, 1);
        assert_eq!(s.steering_volgende_update.as_deref(), Some("2026-11-07T13:46:00+01:00"));
    }
}
