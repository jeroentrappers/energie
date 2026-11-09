//! The Home Assistant bridge: a WebSocket client that mirrors HA's states ([`ws`]), the
//! entity ↔ field tables ([`mapping`], [`outputs`]), and the `ha` source and actuator.

pub mod entity;
pub mod mapping;
pub mod outputs;
pub mod ws;

use energie_core::command::{AircoUnit, Command};
use energie_core::time::Ts;
use energie_core::world::World;
use serde_json::json;
use tokio::sync::mpsc;

use crate::control::Ticket;
use crate::error::{Error, Result};
use crate::plug::{Actuator, WorldSource};
use energie_drivers::{Applied, Outcome, Write};
use mapping::{evse_mode_label, rc_mode_label};
use ws::{ServiceCall, SharedStore};

/// The `ha` source: the World as HA currently shows it.
pub struct HaSource(pub SharedStore);

impl WorldSource for HaSource {
    fn name(&self) -> &str {
        "ha"
    }
    fn read(&self, _now: Ts) -> World {
        mapping::world_from(&*self.0.lock().expect("store lock"))
    }
}

/// The `ha` actuator: commands become HA service calls (control mode only).
pub struct HaActuator(pub mpsc::UnboundedSender<ServiceCall>);

impl Actuator for HaActuator {
    fn name(&self) -> &str {
        "ha"
    }
    fn submit(&self, _now: Ts, cmd: &Command) -> Result<()> {
        let call = service_call(cmd)?;
        self.0.send(call).map_err(|_| Error::new("home assistant client stopped"))
    }
    /// The service call is the write; a dry run reports it and sends nothing.
    fn submit_tracked(&self, now: Ts, cmd: &Command, dry: bool, ticket: Ticket) -> Result<()> {
        let call = service_call(cmd)?;
        let write = Write::new(format!("ha {}.{}", call.domain, call.service), call.data);
        if dry {
            ticket.finish(Ok(Outcome { applied: Applied::Done, writes: vec![write] }));
            return Ok(());
        }
        if !ticket.allowed() {
            ticket.finish(Err("blocked: control is switched off".into()));
            return Ok(());
        }
        self.submit(now, cmd)?;
        ticket.finish(Ok(Outcome { applied: Applied::Done, writes: vec![write] }));
        Ok(())
    }
}

fn call(domain: &str, service: &str, data: serde_json::Value) -> ServiceCall {
    ServiceCall { domain: domain.into(), service: service.into(), data }
}

fn set_number(entity: &str, v: f64) -> ServiceCall {
    call("number", "set_value", json!({"entity_id": entity, "value": v}))
}

fn switch(entity: &str, on: bool) -> ServiceCall {
    call("switch", if on { "turn_on" } else { "turn_off" }, json!({"entity_id": entity}))
}

/// The service call HA's own automations use for each command.
pub fn service_call(cmd: &Command) -> Result<ServiceCall> {
    use Command::*;
    Ok(match cmd {
        // The number entity rounds to whole amps; the automations publish tenths over MQTT.
        EvseCurrentDeciA(d) => {
            call("mqtt", "publish", json!({"topic": "SmartEVSE/1234/Set/CurrentOverride", "payload": d.to_string()}))
        }
        EvseMode(m) => call(
            "select",
            "select_option",
            json!({"entity_id": "select.smartevse_1234_mode", "option": evse_mode_label(*m)}),
        ),
        CableStart { amps } => call("script", "laadkabel_start_laden", json!({"stroom": amps})),
        CableStop => call("script", "laadkabel_stop_laden", json!({})),
        CableSetCurrent(a) => set_number("number.ev_charging_cable_charge_current", f64::from(*a)),
        CableReconnect => call(
            "homeassistant",
            "reload_config_entry",
            json!({"entity_id": "select.ev_charging_cable_charging_operation"}),
        ),
        SolisRcMode(m) => call(
            "select",
            "select_option",
            json!({"entity_id": "select.solis_s6_eh1p_rc_force_charge_discharge_2", "option": rc_mode_label(*m)}),
        ),
        SolisRcChargeW(w) => set_number("number.solis_s6_eh1p_rc_force_battery_charge_power_2", f64::from(*w)),
        SolisRcDischargeW(w) => set_number("number.solis_s6_eh1p_rc_force_battery_discharge_power_2", f64::from(*w)),
        SolisRcTimeoutMin(m) => set_number("number.solis_s6_eh1p_rc_timeout_2", f64::from(*m)),
        SolisAllowGridCharge(on) => switch("switch.solis_s6_eh1p_allow_grid_to_charge_the_battery_2", *on),
        SolisFeedInLimit(on) => switch("switch.solis_s6_eh1p_grid_feed_in_power_limit_switch_2", *on),
        SolisBackflowW(w) => set_number("number.solis_s6_eh1p_backflow_power_2", f64::from(*w)),
        Airco { unit: AircoUnit::Beneden, on } => switch("switch.airco_beneden_power", *on),
        Airco { unit: AircoUnit::Zolder, on } => switch("switch.airco_zolder_power", *on),
        RefreshPrices => call(
            "homeassistant",
            "update_entity",
            json!({"entity_id": ["sensor.jeroen_average_electricity_price", "sensor.injectie_average_electricity_price"]}),
        ),
        SolisReconnect => return Err(Error::new("a Modbus reconnect needs the native driver (HA restarts itself)")),
        Notify { .. } | Persistent { .. } => return Err(Error::new("notifications go to the notifier")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::entity::{EntityState, HaStore};
    use crate::plug::tests::t0;
    use energie_core::world::{EvseMode as Em, RcMode};
    use std::sync::{Arc, Mutex};

    /// The `ha` source maps the mirrored states into a World.
    #[test]
    fn ha_source_reads_the_store() {
        let mut st = HaStore::default();
        st.replace_all(vec![EntityState::new("sensor.p1_meter_vermogen", "-250")]);
        let src = HaSource(Arc::new(Mutex::new(st)));
        assert_eq!(src.name(), "ha");
        assert_eq!(src.read(t0()).p1.power_w, Some(-250.0));
    }

    /// Each command becomes the service call HA's automations make for it.
    #[test]
    fn commands_become_the_service_calls_ha_uses() {
        use Command::*;
        let c = |cmd: Command| service_call(&cmd).unwrap();
        let mqtt = c(EvseCurrentDeciA(145));
        assert_eq!((mqtt.domain.as_str(), mqtt.data["payload"].as_str()), ("mqtt", Some("145")));
        assert_eq!(c(EvseMode(Em::Normal)).data["option"], "Normal");
        assert_eq!(c(CableStart { amps: 8 }).data["stroom"], 8);
        assert_eq!(c(CableStop).service, "laadkabel_stop_laden");
        assert_eq!(c(CableSetCurrent(10)).data["value"], 10.0);
        assert_eq!(c(CableReconnect).service, "reload_config_entry");
        assert_eq!(c(SolisRcMode(RcMode::ForceDischarge)).data["option"], "Solis RC Force Battery Discharge");
        assert_eq!(c(SolisRcChargeW(1500)).data["value"], 1500.0);
        assert_eq!(c(SolisRcDischargeW(900)).data["value"], 900.0);
        assert_eq!(c(SolisRcTimeoutMin(10)).data["value"], 10.0);
        assert_eq!(c(SolisAllowGridCharge(true)).service, "turn_on");
        assert_eq!(c(SolisFeedInLimit(false)).service, "turn_off");
        assert_eq!(c(SolisBackflowW(0)).data["value"], 0.0);
        assert_eq!(c(Airco { unit: AircoUnit::Beneden, on: true }).data["entity_id"], "switch.airco_beneden_power");
        assert_eq!(c(Airco { unit: AircoUnit::Zolder, on: false }).data["entity_id"], "switch.airco_zolder_power");
        assert_eq!(c(RefreshPrices).service, "update_entity");
        assert!(service_call(&SolisReconnect).is_err());
        assert!(service_call(&Command::notify("a", "b")).is_err());
    }

    /// In control mode the service call is the reported write; a dry run sends nothing, and a
    /// switched-off ticket is refused.
    #[test]
    fn ha_actuator_tracked() {
        use std::sync::atomic::AtomicBool;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let a = HaActuator(tx);
        let ticket = |on: bool| Ticket::new(Arc::new(AtomicBool::new(on)));
        let (dry, off, real) = (ticket(true), ticket(false), ticket(true));
        a.submit_tracked(t0(), &Command::CableStop, true, dry.clone()).unwrap();
        a.submit_tracked(t0(), &Command::CableStop, false, off.clone()).unwrap();
        assert!(rx.try_recv().is_err(), "nothing sent");
        a.submit_tracked(t0(), &Command::CableStop, false, real.clone()).unwrap();
        assert_eq!(rx.try_recv().unwrap().service, "laadkabel_stop_laden");
        assert!(dry.done() && off.done() && real.done());
        assert!(format!("{off:?}").contains("allowed: false"));
        assert!(a.submit_tracked(t0(), &Command::SolisReconnect, true, ticket(true)).is_err());
    }

    /// The `ha` actuator queues service calls and reports a stopped client.
    #[test]
    fn ha_actuator_queues_calls() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let a = HaActuator(tx);
        assert_eq!(a.name(), "ha");
        a.submit(t0(), &Command::CableStop).unwrap();
        assert_eq!(rx.try_recv().unwrap().service, "laadkabel_stop_laden");
        assert!(a.submit(t0(), &Command::SolisReconnect).is_err());
        drop(rx);
        assert!(a.submit(t0(), &Command::CableStop).unwrap_err().0.contains("stopped"));
    }
}
