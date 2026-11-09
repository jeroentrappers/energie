//! SmartEVSE v3 (firmware v3.11.2) for the Buzz, over MQTT with prefix `SmartEVSE/1234`.
//!
//! The charger publishes its state as plain strings under `<prefix>/<Name>` and announces HA
//! entities under `homeassistant/<platform>/SmartEVSE/1234-<Name>/config`; its availability is
//! `<prefix>/connected` (`online`, last will `offline`). Observed on the home broker
//! 2026-11-07 (passive subscription):
//!
//! | Topic | HA entity | Encoding |
//! | --- | --- | --- |
//! | `EVPlugState` | `sensor.smartevse_1234_ev_plug_state` | `Connected` / `Disconnected` |
//! | `State` | `sensor.smartevse_1234_state` | text (`Charging`, ...) |
//! | `StateID` | `sensor.smartevse_1234_stateid` | pilot state `A`..`F` |
//! | `Mode` | `select.smartevse_1234_mode` | `Off` `Normal` `Smart` `Solar` `Pause` |
//! | `ChargeCurrentOverride` | `number.smartevse_1234_charge_current_override` | tenths of A (`value \| int / 10 if value \| is_number else none`) |
//! | `ChargeCurrent` | `sensor.smartevse_1234_charge_current` | tenths of A (`value \| int / 10`) |
//! | `MaxCurrent` | `sensor.smartevse_1234_max_current` | tenths of A |
//!
//! The override's `max` (29) comes from its discovery config. Commands go to
//! `<prefix>/Set/CurrentOverride` (tenths of A, as the HA automations publish them) and
//! `<prefix>/Set/Mode` (the option string; the discovery `command_topic`), QoS 0, not retained.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use energie_core::command::Command;
use energie_core::world::{Evse, EvseMode};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::task::JoinHandle;

use crate::driver::{Actuator, Applied, Outcome, Reading, Source, Write};
use crate::error::{DriverError, Result};

/// Broker and topic settings.
#[derive(Debug, Clone)]
pub struct EvseConfig {
    pub host: String,
    pub port: u16,
    /// Broker user and password (never commit them).
    pub credentials: Option<(String, String)>,
    /// `SmartEVSE/1234`.
    pub prefix: String,
    pub client_id: String,
    pub keep_alive: Duration,
    /// Wait after a broker error before the client reconnects.
    pub reconnect_delay: Duration,
}

impl EvseConfig {
    pub fn new(host: impl Into<String>, port: u16, prefix: impl Into<String>) -> Self {
        EvseConfig {
            host: host.into(),
            port,
            credentials: None,
            prefix: prefix.into(),
            client_id: "energie-smartevse".into(),
            keep_alive: Duration::from_secs(30),
            reconnect_delay: Duration::from_secs(2),
        }
    }

    /// `homeassistant/number/SmartEVSE/1234-ChargeCurrentOverride/config`.
    pub fn override_discovery_topic(&self) -> String {
        format!("homeassistant/number/{}-ChargeCurrentOverride/config", self.prefix)
    }
}

/// HA's `is_number`: parses as a finite float.
fn is_number(s: &str) -> bool {
    s.trim().parse::<f64>().is_ok_and(f64::is_finite)
}

/// Jinja's `int` filter: integer, else truncated float, else 0.
fn jinja_int(s: &str) -> i64 {
    let s = s.trim();
    s.parse::<i64>().unwrap_or_else(|_| s.parse::<f64>().map(|f| f.trunc() as i64).unwrap_or(0))
}

/// `select.smartevse_1234_mode` option for a payload.
pub fn parse_mode(s: &str) -> Option<EvseMode> {
    match s {
        "Off" => Some(EvseMode::Off),
        "Normal" => Some(EvseMode::Normal),
        "Smart" => Some(EvseMode::Smart),
        "Solar" => Some(EvseMode::Solar),
        "Pause" => Some(EvseMode::Pause),
        _ => None,
    }
}

/// The option string for a mode (the `Set/Mode` payload).
pub fn mode_name(m: EvseMode) -> &'static str {
    match m {
        EvseMode::Off => "Off",
        EvseMode::Normal => "Normal",
        EvseMode::Smart => "Smart",
        EvseMode::Solar => "Solar",
        EvseMode::Pause => "Pause",
    }
}

#[derive(Debug, Default)]
struct State {
    broker_up: bool,
    /// Payload per topic suffix under the prefix (`Mode`, `State`, ...).
    topics: HashMap<String, String>,
    discovery_max: Option<f64>,
}

impl State {
    fn evse(&self) -> Result<Evse> {
        if !self.broker_up || self.topics.get("connected").map(String::as_str) != Some("online") {
            return Err(DriverError::NotConnected);
        }
        let t = |k: &str| self.topics.get(k);
        let tenths = |k: &str| t(k).map(|v| jinja_int(v) as f64 / 10.0);
        Ok(Evse {
            plug: t("EVPlugState").cloned(),
            state: t("State").cloned(),
            state_id: t("StateID").cloned(),
            mode: t("Mode").and_then(|m| parse_mode(m)),
            override_a: t("ChargeCurrentOverride").filter(|v| is_number(v)).map(|v| jinja_int(v) as f64 / 10.0),
            override_max_a: self.discovery_max.or_else(|| tenths("MaxCurrent")),
            charge_current_a: tenths("ChargeCurrent"),
        })
    }
}

/// MQTT client for one SmartEVSE: a background task keeps the session (and re-subscribes after
/// every reconnect); reads come from the retained and live state topics.
pub struct SmartEvse {
    cfg: EvseConfig,
    client: AsyncClient,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl Drop for SmartEvse {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SmartEvse {
    /// Connect (in the background) and subscribe to `<prefix>/#` and the override's discovery
    /// config. Subscribing only: this client publishes nothing unless a command is applied.
    pub fn spawn(cfg: EvseConfig) -> SmartEvse {
        let mut opts = MqttOptions::new(&cfg.client_id, &cfg.host, cfg.port);
        opts.set_keep_alive(cfg.keep_alive);
        if let Some((u, p)) = &cfg.credentials {
            opts.set_credentials(u, p);
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 32);
        let state: Arc<Mutex<State>> = Arc::default();
        let (st, c, prefix, discovery, delay) = (
            state.clone(),
            client.clone(),
            format!("{}/", cfg.prefix),
            cfg.override_discovery_topic(),
            cfg.reconnect_delay,
        );
        let task = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        st.lock().unwrap().broker_up = true;
                        let _ = c.try_subscribe(format!("{prefix}#"), QoS::AtMostOnce);
                        let _ = c.try_subscribe(&discovery, QoS::AtMostOnce);
                    }
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        let payload = String::from_utf8_lossy(&p.payload).to_string();
                        let mut s = st.lock().unwrap();
                        if p.topic == discovery {
                            let v: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
                            s.discovery_max = v["max"].as_str().and_then(|m| m.parse().ok()).or(v["max"].as_f64());
                        } else if let Some(suffix) = p.topic.strip_prefix(&prefix) {
                            s.topics.insert(suffix.to_string(), payload);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        st.lock().unwrap().broker_up = false;
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        });
        SmartEvse { cfg, client, state, task }
    }

    /// Whether the broker session is up.
    pub fn broker_up(&self) -> bool {
        self.state.lock().unwrap().broker_up
    }

    /// The last payload seen on `<prefix>/<suffix>`.
    pub fn topic(&self, suffix: &str) -> Option<String> {
        self.state.lock().unwrap().topics.get(suffix).cloned()
    }

    fn publish(&self, suffix: &str, payload: String) -> Result<Applied> {
        if !self.broker_up() {
            return Err(DriverError::NotConnected);
        }
        let topic = format!("{}/{suffix}", self.cfg.prefix);
        self.client
            .try_publish(topic, QoS::AtMostOnce, false, payload)
            .map_err(|e| DriverError::Mqtt(e.to_string()))?;
        Ok(Applied::Done)
    }
}

#[async_trait]
impl Source for SmartEvse {
    fn name(&self) -> &'static str {
        "smartevse"
    }

    /// `NotConnected` while the broker is down or the charger is not `online`.
    async fn read(&mut self) -> Result<Reading> {
        Ok(Reading::Evse(self.state.lock().unwrap().evse()?))
    }

    fn unavailable(&self) -> Reading {
        Reading::Evse(Evse::default())
    }
}

/// The publish a command becomes: topic suffix, payload, and the state topic that echoes it.
fn publish_of(cmd: &Command) -> Option<(&'static str, String, &'static str)> {
    match *cmd {
        Command::EvseCurrentDeciA(deci) => Some(("Set/CurrentOverride", deci.to_string(), "ChargeCurrentOverride")),
        Command::EvseMode(m) => Some(("Set/Mode", mode_name(m).to_string(), "Mode")),
        _ => None,
    }
}

impl SmartEvse {
    fn describe(&self, suffix: &str, payload: &str, echo: &str) -> Write {
        Write::new(format!("mqtt {}/{suffix}", self.cfg.prefix), payload)
            .before(self.topic(echo))
            .note("qos 0, retain false")
    }
}

#[async_trait]
impl Actuator for SmartEvse {
    async fn apply(&mut self, cmd: &Command) -> Result<Applied> {
        Ok(self.execute(cmd).await?.applied)
    }

    /// One publish, as the HA automations (`mqtt.publish`) and the mode select (its discovery
    /// `command_topic`, no `command_template`) send it.
    async fn execute(&mut self, cmd: &Command) -> Result<Outcome> {
        let Some((suffix, payload, echo)) = publish_of(cmd) else {
            return Ok(Outcome { applied: Applied::NotMine, writes: Vec::new() });
        };
        let write = self.describe(suffix, &payload, echo);
        let applied = self.publish(suffix, payload)?;
        Ok(Outcome { applied, writes: vec![write] })
    }

    /// The publish `execute` would send; nothing is sent.
    async fn plan(&mut self, cmd: &Command) -> Result<Outcome> {
        let Some((suffix, payload, echo)) = publish_of(cmd) else {
            return Ok(Outcome { applied: Applied::NotMine, writes: Vec::new() });
        };
        Ok(Outcome { applied: Applied::Done, writes: vec![self.describe(suffix, &payload, echo)] })
    }
}

#[cfg(test)]
mod tests;
