use std::net::SocketAddr;

use super::*;
use crate::sim::mqtt::{Broker, SmartEvseSim};
use crate::sim::proxy::TcpProxy;

const PREFIX: &str = "SmartEVSE/1234";

fn cfg(addr: SocketAddr) -> EvseConfig {
    EvseConfig {
        reconnect_delay: Duration::from_millis(20),
        ..EvseConfig::new(addr.ip().to_string(), addr.port(), PREFIX)
    }
}

async fn eventually(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..600 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn evse_reading(e: &mut SmartEvse) -> Result<Evse> {
    match e.read().await? {
        Reading::Evse(r) => Ok(r),
        _ => unreachable!(),
    }
}

async fn ready(e: &mut SmartEvse) -> Evse {
    for _ in 0..600 {
        if let Ok(r) = evse_reading(e).await
            && r.override_max_a.is_some()
            && r.mode.is_some()
        {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("charger never became available");
}

/// The HA templates: `value | int / 10` (non-numbers become 0) for the charge current and
/// `value | int / 10 if value | is_number else none` for the override.
#[test]
fn ha_value_templates() {
    assert_eq!(jinja_int("151"), 151);
    assert_eq!(jinja_int("15.7"), 15);
    assert_eq!(jinja_int("x"), 0);
    assert!(is_number(" 151 "));
    assert!(!is_number("nan") && !is_number("x"));
    let mut s = State { broker_up: true, ..State::default() };
    for (k, v) in
        [("connected", "online"), ("ChargeCurrentOverride", "n/a"), ("ChargeCurrent", "abc"), ("MaxCurrent", "160")]
    {
        s.topics.insert(k.into(), v.into());
    }
    let e = s.evse().unwrap();
    assert_eq!(e.override_a, None);
    assert_eq!(e.charge_current_a, Some(0.0));
    assert_eq!(e.override_max_a, Some(16.0), "without discovery the max follows MaxCurrent");
    assert_eq!(e.mode, None);
}

/// Every select option round-trips; anything else is no mode.
#[test]
fn mode_names_round_trip() {
    for m in [EvseMode::Off, EvseMode::Normal, EvseMode::Smart, EvseMode::Solar, EvseMode::Pause] {
        assert_eq!(parse_mode(mode_name(m)), Some(m));
    }
    assert_eq!(parse_mode("Turbo"), None);
}

/// The live topics of 2026-11-07 read as HA showed them: Connected, Charging, pilot C, Normal,
/// override 15.1 A with max 29 (from discovery), charge current 15.1 A.
#[tokio::test]
async fn reads_live_topics_through_broker() {
    let broker = Broker::start(None).await;
    let _sim = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let mut e = SmartEvse::spawn(cfg(broker.addr));
    assert_eq!(e.name(), "smartevse");
    let r = ready(&mut e).await;
    assert_eq!(
        r,
        Evse {
            plug: Some("Connected".into()),
            state: Some("Charging".into()),
            state_id: Some("C".into()),
            mode: Some(EvseMode::Normal),
            override_a: Some(15.1),
            override_max_a: Some(29.0),
            charge_current_a: Some(15.1),
        }
    );
    assert_eq!(e.unavailable(), Reading::Evse(Evse::default()));
    assert_eq!(e.topic("Access").as_deref(), Some("Allow"));
}

/// `EvseCurrentDeciA(145)` publishes `145` on `Set/CurrentOverride`; the charger takes it and
/// reports 14.5 A. A value below 6 A is ignored by the firmware.
#[tokio::test]
async fn current_override_in_tenths() {
    let broker = Broker::start(None).await;
    let sim = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let mut e = SmartEvse::spawn(cfg(broker.addr));
    ready(&mut e).await;
    assert_eq!(e.apply(&Command::EvseCurrentDeciA(145)).await.unwrap(), Applied::Done);
    eventually("override 14.5", || e.topic("ChargeCurrentOverride").as_deref() == Some("145")).await;
    assert_eq!(evse_reading(&mut e).await.unwrap().override_a, Some(14.5));
    e.apply(&Command::EvseCurrentDeciA(50)).await.unwrap();
    eventually("received", || sim.received().len() == 2).await;
    assert_eq!(sim.received()[0], ("SmartEVSE/1234/Set/CurrentOverride".to_string(), "145".to_string()));
    assert_eq!(sim.get("ChargeCurrentOverride").as_deref(), Some("145"));
}

/// `EvseMode` publishes the option string on `Set/Mode`.
#[tokio::test]
async fn mode_select_publishes_option() {
    let broker = Broker::start(None).await;
    let sim = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let mut e = SmartEvse::spawn(cfg(broker.addr));
    ready(&mut e).await;
    e.apply(&Command::EvseMode(EvseMode::Solar)).await.unwrap();
    eventually("solar", || e.topic("Mode").as_deref() == Some("Solar")).await;
    assert_eq!(evse_reading(&mut e).await.unwrap().mode, Some(EvseMode::Solar));
    assert_eq!(sim.received(), vec![("SmartEVSE/1234/Set/Mode".to_string(), "Solar".to_string())]);
}

/// Unplugging shows up as `EVPlugState = Disconnected`.
#[tokio::test]
async fn plug_state_follows_charger() {
    let broker = Broker::start(None).await;
    let sim = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let mut e = SmartEvse::spawn(cfg(broker.addr));
    ready(&mut e).await;
    sim.set("EVPlugState", "Disconnected");
    sim.set("StateID", "A");
    eventually("unplugged", || e.topic("StateID").as_deref() == Some("A")).await;
    assert_eq!(evse_reading(&mut e).await.unwrap().plug.as_deref(), Some("Disconnected"));
}

/// When the charger drops off, the broker publishes its `offline` will and the reading becomes
/// unavailable (HA's availability topic).
#[tokio::test]
async fn charger_offline_is_unavailable() {
    let broker = Broker::start(None).await;
    let sim = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let mut e = SmartEvse::spawn(cfg(broker.addr));
    ready(&mut e).await;
    sim.kill();
    eventually("offline", || e.topic("connected").as_deref() == Some("offline")).await;
    assert!(matches!(e.read().await, Err(DriverError::NotConnected)));
}

/// Without a broker nothing is read and commands are refused rather than queued.
#[tokio::test]
async fn no_broker_no_reading_no_commands() {
    let mut e = SmartEvse::spawn(cfg("127.0.0.1:9".parse().unwrap()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!e.broker_up());
    assert!(matches!(e.read().await, Err(DriverError::NotConnected)));
    assert!(matches!(e.apply(&Command::EvseMode(EvseMode::Off)).await, Err(DriverError::NotConnected)));
}

/// After the broker link drops the client reconnects, re-subscribes and gets the retained state
/// again; commands work once more.
#[tokio::test]
async fn reconnects_after_broker_link_drops() {
    let broker = Broker::start(None).await;
    let sim = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let proxy = TcpProxy::start(broker.addr).await;
    let mut e = SmartEvse::spawn(cfg(proxy.addr));
    ready(&mut e).await;
    proxy.drop_all();
    eventually("link down", || !e.broker_up()).await;
    eventually("link up", || e.broker_up()).await;
    ready(&mut e).await;
    e.apply(&Command::EvseCurrentDeciA(100)).await.unwrap();
    eventually("received", || !sim.received().is_empty()).await;
}

/// With broker authentication, only the right credentials get a session.
#[tokio::test]
async fn broker_credentials() {
    let broker = Broker::start(Some(("ha", "made-up-password"))).await;
    let _sim = SmartEvseSim::start(broker.addr, PREFIX, Some(("ha", "made-up-password"))).await;
    let bad = SmartEvse::spawn(EvseConfig { credentials: Some(("ha".into(), "wrong".into())), ..cfg(broker.addr) });
    let mut good = SmartEvse::spawn(EvseConfig {
        client_id: "energie-2".into(),
        credentials: Some(("ha".into(), "made-up-password".into())),
        ..cfg(broker.addr)
    });
    ready(&mut good).await;
    assert!(!bad.broker_up());
}

/// Commands for other devices are ignored; a full request queue is an MQTT error.
#[tokio::test]
async fn other_commands_and_full_queue() {
    let broker = Broker::start(None).await;
    let _sim = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let mut e = SmartEvse::spawn(cfg(broker.addr));
    ready(&mut e).await;
    assert_eq!(e.apply(&Command::CableStop).await.unwrap(), Applied::NotMine);
    e.task.abort();
    let mut last = Ok(Applied::Done);
    for _ in 0..100 {
        last = e.apply(&Command::EvseCurrentDeciA(60)).await;
    }
    assert!(matches!(last, Err(DriverError::Mqtt(_))));
}

/// The discovery topic of the override number.
#[test]
fn discovery_topic() {
    assert_eq!(
        EvseConfig::new("h", 1883, PREFIX).override_discovery_topic(),
        "homeassistant/number/SmartEVSE/1234-ChargeCurrentOverride/config"
    );
}

/// `execute` reports the publish (topic, payload, the echoing state topic as `before`, QoS 0
/// and no retain, as HA's `mqtt.publish` and the mode select send it); `plan` reports the same
/// and publishes nothing.
#[tokio::test]
async fn execute_and_plan_report_the_publish() {
    let broker = Broker::start(None).await;
    let sim = SmartEvseSim::start(broker.addr, PREFIX, None).await;
    let mut e = SmartEvse::spawn(cfg(broker.addr));
    ready(&mut e).await;
    let want =
        Write::new("mqtt SmartEVSE/1234/Set/CurrentOverride", "145").before(Some("151")).note("qos 0, retain false");
    let plan = e.plan(&Command::EvseCurrentDeciA(145)).await.unwrap();
    assert_eq!(plan, Outcome { applied: Applied::Done, writes: vec![want.clone()] });
    let mode = e.plan(&Command::EvseMode(EvseMode::Pause)).await.unwrap();
    assert_eq!(mode.writes[0].target, "mqtt SmartEVSE/1234/Set/Mode");
    assert_eq!(mode.writes[0].value, "Pause");
    assert_eq!(mode.writes[0].before, Some("Normal".into()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(sim.received().is_empty(), "a dry run publishes nothing");
    assert_eq!(e.execute(&Command::EvseCurrentDeciA(145)).await.unwrap(), plan);
    eventually("received", || sim.received().len() == 1).await;
    assert_eq!(e.plan(&Command::CableStop).await.unwrap().applied, Applied::NotMine);
    assert_eq!(e.execute(&Command::CableStop).await.unwrap().applied, Applied::NotMine);
}
