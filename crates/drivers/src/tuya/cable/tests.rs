use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use energie_core::command::Command;

use serde_json::json;
use tokio::net::TcpListener;

use super::*;
use crate::driver::{Actuator, Applied, Outcome, Reading, Source, Write};
use crate::sim::tuya::{TuyaSim, default_dps};
use crate::tuya::charger::dps_of;
use crate::tuya::protocol;

/// A made-up local key; the real one lives only in the HA config entry.
const KEY: &str = "0123456789abcdef";

fn cfg(addr: SocketAddr) -> CableConfig {
    CableConfig {
        heartbeat: Duration::from_millis(200),
        start_delay: Duration::from_millis(60),
        reconnect_delay: Duration::from_millis(30),
        timeout: Duration::from_millis(500),
        ..CableConfig::new(addr, "bf00000000000000000000", KEY)
    }
}

async fn eventually(what: &str, mut pred: impl FnMut() -> bool) {
    for _ in 0..400 {
        if pred() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn connected(sim: &TuyaSim) -> CableHandle {
    let h = CableHandle::spawn(cfg(sim.addr)).unwrap();
    eventually("connect", || h.connected()).await;
    h
}

async fn sim() -> TuyaSim {
    TuyaSim::start(protocol::parse_key(KEY).as_ref().unwrap()).await
}

/// `kolanky_evcharger.yaml` status mapping, including the device's own spellings
/// (`no_connet`, `charing`); unknown values pass through.
#[test]
fn status_mapping_follows_device_definition() {
    for (dp, name) in [
        ("no_connet", "available"),
        ("connect", "plugged_in"),
        ("charing", "charging"),
        ("wait_rfid", "waiting_for_authorization"),
        ("finish", "charged"),
        ("wait_charing", "waiting"),
        ("error", "fault"),
        ("new_state", "new_state"),
    ] {
        assert_eq!(status_name(dp), name);
    }
}

/// DP 105 and 109 are scaled by 10 (A and kW), 102 is V, the charge current is DP 114 with a
/// 16 A max unless DP 113 says `Max32A`, then DP 115 with 32 A.
#[test]
fn dps_map_to_cable_entities() {
    let mut dps = default_dps();
    dps.insert("101".into(), json!("charing"));
    dps.insert("105".into(), json!(101));
    dps.insert("109".into(), json!(23));
    assert_eq!(
        cable_from_dps(&dps),
        Cable {
            status: Some("charging".into()),
            current_a: Some(10.1),
            voltage_v: Some(230.0),
            power_kw: Some(2.3),
            set_current_a: Some(8.0),
            set_current_max_a: Some(16.0),
        }
    );
    dps.insert("113".into(), json!("Max32A"));
    dps.insert("115".into(), json!(24));
    let c = cable_from_dps(&dps);
    assert_eq!((c.set_current_a, c.set_current_max_a), (Some(24.0), Some(32.0)));
}

/// After the handshake and the first DP query the cable reads as plugged in at 230 V.
#[tokio::test]
async fn connects_and_reads_dps() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    assert_eq!(h.name(), "tuya_cable");
    let Reading::Cable(c) = h.read().await.unwrap() else { panic!() };
    assert_eq!(c.status.as_deref(), Some("plugged_in"));
    assert_eq!(c.voltage_v, Some(230.0));
    assert_eq!(h.unavailable(), Reading::Cable(Cable::default()));
    assert_eq!(sim.handshakes(), 1);
}

/// `script.laadkabel_start_laden`: select `Start charging`, wait, then set the current. The
/// device's status push then shows it charging at that current.
#[tokio::test]
async fn start_selects_then_sets_current_after_delay() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    assert_eq!(h.apply(&Command::CableStart { amps: 10 }).await.unwrap(), Applied::Scheduled);
    eventually("two sets", || sim.sets().len() == 2).await;
    let sets = sim.sets();
    assert_eq!(sets[0].1, json!({"124": "OpenCharging"}).as_object().unwrap().clone());
    assert_eq!(sets[1].1, json!({"114": 10}).as_object().unwrap().clone());
    assert!(sets[1].0 - sets[0].0 >= Duration::from_millis(60));
    eventually("charging at 10 A", || h.dps().get("105") == Some(&json!(100))).await;
    let Reading::Cable(c) = h.read().await.unwrap() else { panic!() };
    assert_eq!((c.status.as_deref(), c.current_a, c.power_kw), (Some("charging"), Some(10.0), Some(2.3)));
}

/// `mode: restart`: a second start during the wait replaces the first; only the last current
/// is set, a full delay after the second `Start charging`.
#[tokio::test]
async fn second_start_restarts_the_sequence() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    h.apply(&Command::CableStart { amps: 8 }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.apply(&Command::CableStart { amps: 12 }).await.unwrap();
    eventually("three sets", || sim.sets().len() == 3).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let sets = sim.sets();
    assert_eq!(sets.len(), 3);
    assert_eq!(sets[2].1, json!({"114": 12}).as_object().unwrap().clone());
    assert!(sets[2].0 - sets[1].0 >= Duration::from_millis(60));
}

/// `script.laadkabel_stop_laden` selects `Stop charging`.
#[tokio::test]
async fn stop_selects_stop_charging() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    sim.set_dp("101", json!("charing"));
    eventually("charging", || h.dps().get("101") == Some(&json!("charing"))).await;
    assert_eq!(h.apply(&Command::CableStop).await.unwrap(), Applied::Done);
    eventually("stopped", || h.dps().get("101") == Some(&json!("connect"))).await;
    assert_eq!(sim.sets()[0].1, json!({"124": "CloseCharging"}).as_object().unwrap().clone());
}

/// The charge current number accepts 6..16 A on DP 114; with `Max32A` it is DP 115, 8..32 A.
/// Out-of-range values are refused without a write.
#[tokio::test]
async fn set_current_respects_number_range() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    assert_eq!(h.apply(&Command::CableSetCurrent(16)).await.unwrap(), Applied::Done);
    assert!(matches!(h.apply(&Command::CableSetCurrent(17)).await, Err(DriverError::OutOfRange(_))));
    assert!(matches!(h.apply(&Command::CableSetCurrent(5)).await, Err(DriverError::OutOfRange(_))));
    sim.set_dp("113", json!("Max32A"));
    eventually("max32", || h.dps().get("113") == Some(&json!("Max32A"))).await;
    h.apply(&Command::CableSetCurrent(20)).await.unwrap();
    eventually("two sets", || sim.sets().len() == 2).await;
    assert_eq!(sim.sets()[0].1, json!({"114": 16}).as_object().unwrap().clone());
    assert_eq!(sim.sets()[1].1, json!({"115": 20}).as_object().unwrap().clone());
}

/// A start with a current the number refuses still selects `Start charging` (the HA script
/// fails only at its second step) and keeps the session.
#[tokio::test]
async fn start_with_bad_current_only_selects() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    h.apply(&Command::CableStart { amps: 20 }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(sim.sets().len(), 1);
    assert!(h.connected());
}

/// Status pushes from the device update the state without a query.
#[tokio::test]
async fn pushes_update_state() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    sim.set_dp("101", json!("finish"));
    eventually("finish", || h.dps().get("101") == Some(&json!("finish"))).await;
    let Reading::Cable(c) = h.read().await.unwrap() else { panic!() };
    assert_eq!(c.status.as_deref(), Some("charged"));
    assert_eq!(sim.dps().get("101"), Some(&json!("finish")));
}

/// When the device drops the link the cable reads unavailable and commands fail fast; after
/// the reconnect delay a new session is negotiated and the state is back.
#[tokio::test]
async fn reconnects_after_device_drops_link() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    sim.drop_connection();
    eventually("disconnect", || !h.connected()).await;
    assert!(matches!(h.read().await, Err(DriverError::NotConnected)));
    assert!(matches!(h.apply(&Command::CableStop).await, Err(DriverError::NotConnected)));
    eventually("reconnect", || h.connected()).await;
    assert_eq!(sim.handshakes(), 2);
    assert!(h.read().await.is_ok());
}

/// `CableReconnect` (HA: reload the config entry) closes the session and negotiates a new one,
/// also while waiting to reconnect.
#[tokio::test]
async fn reconnect_command_renegotiates() {
    let sim = sim().await;
    let mut h = CableHandle::spawn(CableConfig { reconnect_delay: Duration::from_secs(60), ..cfg(sim.addr) }).unwrap();
    eventually("connect", || h.connected()).await;
    assert_eq!(h.apply(&Command::CableReconnect).await.unwrap(), Applied::Done);
    eventually("second handshake", || sim.handshakes() == 2 && h.connected()).await;
    sim.drop_connection();
    eventually("disconnect", || !h.connected()).await;
    assert_eq!(h.apply(&Command::CableReconnect).await.unwrap(), Applied::Done);
    eventually("third handshake", || sim.handshakes() == 3 && h.connected()).await;
}

/// A frame with a bad HMAC ends the session (wrong key or corruption); the worker reconnects.
#[tokio::test]
async fn bad_hmac_from_device_drops_session() {
    let sim = sim().await;
    let h = connected(&sim).await;
    sim.corrupt_next_frame();
    sim.set_dp("101", json!("finish"));
    eventually("reconnect", || sim.handshakes() == 2 && h.connected()).await;
    assert_eq!(h.dps().get("101"), Some(&json!("finish")));
}

/// With the wrong local key the device rejects the handshake and the cable never connects.
#[tokio::test]
async fn wrong_local_key_never_connects() {
    let sim = sim().await;
    let mut h = CableHandle::spawn(CableConfig { local_key: "fedcba9876543210".into(), ..cfg(sim.addr) }).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!h.connected());
    assert_eq!(sim.handshakes(), 0);
    assert!(matches!(h.read().await, Err(DriverError::NotConnected)));
}

/// Heartbeats keep a quiet link alive; when the device stops answering for three periods the
/// worker gives up on the session and reconnects.
#[tokio::test]
async fn silent_device_is_detected_by_heartbeat() {
    let sim = sim().await;
    let h = CableHandle::spawn(CableConfig { heartbeat: Duration::from_millis(20), ..cfg(sim.addr) }).unwrap();
    eventually("connect", || h.connected()).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(sim.handshakes(), 1, "answered heartbeats keep the session");
    sim.answer_heartbeats(false);
    eventually("reconnect", || sim.handshakes() >= 2).await;
}

/// The cable takes one local client: a second one never gets a session while the first holds it.
#[tokio::test]
async fn second_client_is_refused() {
    let sim = sim().await;
    let first = connected(&sim).await;
    let second = CableHandle::spawn(cfg(sim.addr)).unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(first.connected());
    assert!(!second.connected());
}

/// A device that accepts TCP but never answers the handshake times out and is retried.
#[tokio::test]
async fn mute_device_times_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(Mutex::new(Vec::new()));
    let a = accepted.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = listener.accept().await {
            a.lock().unwrap().push(s);
        }
    });
    let h = CableHandle::spawn(CableConfig { timeout: Duration::from_millis(40), ..cfg(addr) }).unwrap();
    eventually("retry", || accepted.lock().unwrap().len() >= 2).await;
    assert!(!h.connected());
}

/// A query answer without DPs is not a session.
#[tokio::test]
async fn query_without_dps_is_refused() {
    let sim = sim().await;
    sim.set_query_reply(json!({"devId": "x"}));
    let h = CableHandle::spawn(cfg(sim.addr)).unwrap();
    eventually("retries", || sim.handshakes() >= 2).await;
    assert!(!h.connected());
}

/// Dropping the handle ends the worker and frees the device's single connection, whether the
/// worker was connected or waiting to reconnect.
#[tokio::test]
async fn dropping_handle_frees_the_device() {
    let sim = sim().await;
    let h = connected(&sim).await;
    drop(h);
    let h2 = connected(&sim).await;
    sim.drop_connection();
    eventually("disconnect", || !h2.connected()).await;
    drop(h2);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _h3 = connected(&sim).await;
}

/// Commands for other devices are ignored; a malformed key is refused at start.
#[tokio::test]
async fn other_commands_and_bad_key() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    assert_eq!(h.apply(&Command::SolisReconnect).await.unwrap(), Applied::NotMine);
    assert!(matches!(
        CableHandle::spawn(CableConfig { local_key: "short".into(), ..cfg(sim.addr) }),
        Err(DriverError::Config(_))
    ));
}

/// The production defaults: 10 s heartbeat, 3 s start delay (the HA script's `delay`).
#[test]
fn production_defaults() {
    let c = CableConfig::new("192.168.1.60:6668".parse().unwrap(), "id", KEY);
    assert_eq!((c.heartbeat, c.start_delay), (Duration::from_secs(10), Duration::from_secs(3)));
}

/// A query reply and a status push both carry DPs; anything else does not.
#[test]
fn dps_from_both_payload_shapes() {
    assert!(dps_of(br#"{"dps":{"1":2}}"#).is_some());
    assert!(dps_of(br#"{"protocol":4,"data":{"dps":{"1":2}}}"#).is_some());
    assert!(dps_of(b"{}").is_none());
    assert!(dps_of(b"nope").is_none());
}

/// `execute` reports the DP sets with the DPs they replace; `plan` (the dry run) reports the
/// same and sends nothing. A start's current follows after the delay; an out-of-range current
/// is refused as by the number, and without a session the plan fails like the command.
#[tokio::test]
async fn execute_and_plan_report_the_dp_sets() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    let dp = |id: &str, v: Value, before: Value| Write::new(format!("dp {id}"), v).before(Some(before));
    let start = h.plan(&Command::CableStart { amps: 10 }).await.unwrap();
    let delayed = dp("114", json!(10), json!(8)).note("after start_delay (script.laadkabel_start_laden: delay 3 s)");
    let open = dp("124", json!("OpenCharging"), json!("WaitOperation"));
    assert_eq!(start, Outcome { applied: Applied::Scheduled, writes: vec![open, delayed] });
    let bad_start = h.plan(&Command::CableStart { amps: 20 }).await.unwrap();
    assert_eq!(bad_start.writes.len(), 1, "the script fails at its number.set_value");
    let stop = h.plan(&Command::CableStop).await.unwrap();
    assert_eq!(stop.writes, vec![dp("124", json!("CloseCharging"), json!("WaitOperation"))]);
    assert_eq!(h.plan(&Command::CableSetCurrent(12)).await.unwrap().writes, vec![dp("114", json!(12), json!(8))]);
    assert!(matches!(h.plan(&Command::CableSetCurrent(17)).await, Err(DriverError::OutOfRange(_))));
    let rc = h.plan(&Command::CableReconnect).await.unwrap();
    assert_eq!(rc.writes, vec![Write::new("tuya session", "reconnect")]);
    assert_eq!(h.plan(&Command::SolisReconnect).await.unwrap().applied, Applied::NotMine);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(sim.sets().is_empty(), "a dry run sends nothing");
    let done = h.execute(&Command::CableSetCurrent(12)).await.unwrap();
    assert_eq!(done, Outcome { applied: Applied::Done, writes: vec![dp("114", json!(12), json!(8))] });
    eventually("set", || sim.sets().len() == 1).await;
    assert_eq!(h.execute(&Command::SolisReconnect).await.unwrap().applied, Applied::NotMine);
    assert!(h.execute(&Command::CableSetCurrent(17)).await.is_err());
    sim.drop_connection();
    eventually("disconnected", || !h.connected()).await;
    assert!(matches!(h.plan(&Command::CableStop).await, Err(DriverError::NotConnected)));
    assert!(h.execute(&Command::CableStop).await.is_err());
}

/// The cable's other `tuya_local` entities come from the DPs it already reports: temperature and
/// session energy ×10, the operation select's option, the raw status, the fault bitfield.
#[tokio::test]
async fn extra_entities_from_the_dps() {
    use crate::entity::Value as V;
    let sim = sim().await;
    let h = connected(&sim).await;
    let es = h.entities();
    let v = |k: &str| es.iter().find(|e| e.key == k).unwrap().value.clone();
    assert_eq!(v("temperature_c"), V::Number(21.5));
    assert_eq!(v("energy_kwh"), V::Number(123.4));
    assert_eq!(v("charging_time_h"), V::Null);
    assert_eq!(v("operation"), V::Text("No operation".into()));
    assert_eq!(v("status_raw"), V::Text("connect".into()));
    assert_eq!(v("fault"), V::Bool(false));
    assert_eq!((v("delay_h"), v("charging_duration_h")), (V::Number(0.0), V::Number(0.0)));
    let keys: Vec<_> = es.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys, HA_ENTITIES.iter().map(|(k, _)| *k).collect::<Vec<_>>());
    assert!(es.iter().zip(HA_ENTITIES).all(|(e, (_, ha))| e.ha_entity.as_deref() == Some(ha)));
    assert_eq!(operation_name("OpenCharging"), "Start charging");
    assert_eq!(operation_name("CloseCharging"), "Stop charging");
    assert_eq!(operation_name("x"), "x");
}
