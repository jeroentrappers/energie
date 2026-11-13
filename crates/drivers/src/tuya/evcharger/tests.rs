use std::net::SocketAddr;
use std::time::Duration;

use energie_core::command::Command;
use serde_json::json;

use super::*;
use crate::driver::{Actuator, Applied, Outcome, Reading, Source, Write};
use crate::sim::tuya::{TuyaSim, evcharger_dps};
use crate::tuya::protocol;
use crate::tuya::wire::Version;

/// A made-up local key; the real one is fetched from Tuya and kept in a credentials file.
const KEY: &str = "0123456789abcdef";

fn cfg(addr: SocketAddr) -> ChargerConfig {
    ChargerConfig {
        heartbeat: Duration::from_millis(200),
        reconnect_delay: Duration::from_millis(30),
        timeout: Duration::from_millis(500),
        ..config(addr, "bf00000000000000000000", KEY)
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

async fn sim() -> TuyaSim {
    TuyaSim::start_evcharger(protocol::parse_key(KEY).as_ref().unwrap()).await
}

async fn connected(sim: &TuyaSim) -> EvChargerHandle {
    let h = EvChargerHandle::spawn(cfg(sim.addr)).unwrap();
    eventually("connect", || h.connected()).await;
    h
}

fn dps(v: serde_json::Value) -> Dps {
    v.as_object().unwrap().clone()
}

/// Every state with a car connected and not charging reads `plugged_in`, so the logic can
/// (re)start it; both fault states read `fault`; unknown values pass through.
#[test]
fn status_mapping() {
    for (dp, name) in [
        ("charger_free", "available"),
        ("charger_insert", "plugged_in"),
        ("charger_wait", "plugged_in"),
        ("charger_pause", "plugged_in"),
        ("charger_charging", "charging"),
        ("charger_end", "charged"),
        ("charger_fault", "fault"),
        ("charger_free_fault", "fault"),
        ("new_state", "new_state"),
    ] {
        assert_eq!(status_name(dp), name);
    }
}

/// DP 9 is W (kW ×1000); the current is derived from it at 230 V, the voltage stays unset;
/// the set current is DP 4 with a 16 A max. Without DP 9 there is no power and no current.
#[test]
fn dps_map_to_the_cable_reading() {
    let mut d = evcharger_dps();
    d.insert("3".into(), json!("charger_charging"));
    d.insert("4".into(), json!(10));
    d.insert("9".into(), json!(2300));
    assert_eq!(
        cable_from_dps(&d),
        Cable {
            status: Some("charging".into()),
            current_a: Some(10.0),
            voltage_v: None,
            power_kw: Some(2.3),
            set_current_a: Some(10.0),
            set_current_max_a: Some(16.0),
        }
    );
    d.insert("9".into(), json!(1395));
    assert_eq!(cable_from_dps(&d).current_a, Some(6.07));
    d.insert("3".into(), json!("charger_wait"));
    d.remove("9");
    let c = cable_from_dps(&d);
    assert_eq!((c.power_kw, c.current_a), (None, None));
}

/// The EV3's charger reports 0 W while it charges: then the set current stands for the
/// current, and the power is that at 230 V. Outside `charger_charging` 0 W stays 0 W, and a
/// reported power always wins.
#[test]
fn charging_without_power_reads_the_set_current() {
    let mut d = evcharger_dps();
    d.insert("3".into(), json!("charger_charging"));
    d.insert("4".into(), json!(10));
    for power in [Some(json!(0)), Some(json!(1)), Some(json!(99)), None] {
        match power {
            Some(p) => d.insert("9".into(), p),
            None => d.remove("9"),
        };
        let c = cable_from_dps(&d);
        assert_eq!((c.current_a, c.power_kw), (Some(10.0), Some(2.3)), "power {:?}", d.get("9"));
    }
    d.insert("9".into(), json!(1380));
    assert_eq!(cable_from_dps(&d).current_a, Some(6.0), "measured power wins");
    for (state, status) in [("charger_pause", "plugged_in"), ("charger_end", "charged")] {
        d.insert("3".into(), json!(state));
        d.insert("9".into(), json!(0));
        let c = cable_from_dps(&d);
        assert_eq!((c.status.as_deref(), c.current_a, c.power_kw), (Some(status), Some(0.0), Some(0.0)));
    }
}

/// Over protocol 3.5 the handshake and the first query give a plugged-in charger.
#[tokio::test]
async fn connects_over_35_and_reads() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    assert_eq!(h.name(), "tuya_evcharger");
    let Reading::Cable(c) = h.read().await.unwrap() else { panic!() };
    assert_eq!((c.status.as_deref(), c.set_current_a, c.power_kw), (Some("plugged_in"), Some(16.0), Some(0.0)));
    assert_eq!(sim.handshakes(), 1);
}

/// A start is one set: the current and the switch (the mode is already `charge_now`). The
/// charger reports charging at that current; nothing follows later.
#[tokio::test]
async fn start_sets_current_and_switch_at_once() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    assert_eq!(h.apply(&Command::CableStart { amps: 10 }).await.unwrap(), Applied::Done);
    eventually("charging", || h.dps().get("3") == Some(&json!("charger_charging"))).await;
    assert_eq!(sim.sets().len(), 1);
    assert_eq!(sim.sets()[0].1, dps(json!({"4": 10, "18": true})));
    let Reading::Cable(c) = h.read().await.unwrap() else { panic!() };
    assert_eq!((c.status.as_deref(), c.current_a, c.power_kw), (Some("charging"), Some(10.0), Some(2.3)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sim.sets().len(), 1, "no deferred write");
}

/// In another mode (the app's schedule, say) a start selects `charge_now` too; a current the
/// charger refuses is left out and the start still switches on.
#[tokio::test]
async fn start_selects_charge_now_and_skips_a_bad_current() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    sim.set_dp("14", json!("charge_schedule"));
    eventually("mode", || h.dps().get("14") == Some(&json!("charge_schedule"))).await;
    h.apply(&Command::CableStart { amps: 20 }).await.unwrap();
    eventually("set", || sim.sets().len() == 1).await;
    assert_eq!(sim.sets()[0].1, dps(json!({"14": "charge_now", "18": true})));
}

/// A stop switches off; the paused charger still reads `plugged_in`.
#[tokio::test]
async fn stop_switches_off() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    h.apply(&Command::CableStart { amps: 8 }).await.unwrap();
    eventually("charging", || h.dps().get("3") == Some(&json!("charger_charging"))).await;
    assert_eq!(h.apply(&Command::CableStop).await.unwrap(), Applied::Done);
    eventually("paused", || h.dps().get("3") == Some(&json!("charger_pause"))).await;
    assert_eq!(sim.sets()[1].1, dps(json!({"18": false})));
    let Reading::Cable(c) = h.read().await.unwrap() else { panic!() };
    assert_eq!((c.status.as_deref(), c.current_a), (Some("plugged_in"), Some(0.0)));
}

/// The current goes to DP 4 within 6..16 A; anything else is refused without a write. A new
/// current while charging changes the draw.
#[tokio::test]
async fn set_current_within_range() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    h.apply(&Command::CableStart { amps: 6 }).await.unwrap();
    assert_eq!(h.apply(&Command::CableSetCurrent(13)).await.unwrap(), Applied::Done);
    eventually("13 A", || h.dps().get("9") == Some(&json!(2990))).await;
    for bad in [5, 17] {
        assert!(matches!(h.apply(&Command::CableSetCurrent(bad)).await, Err(DriverError::OutOfRange(_))));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(sim.sets().len(), 2);
    assert_eq!(sim.sets()[1].1, dps(json!({"4": 13})));
}

/// `plan` reports the DP sets with the values they replace and sends nothing; `execute`
/// sends them and reports the same.
#[tokio::test]
async fn plan_and_execute_report_the_dp_sets() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    let dp = |id: &str, v: serde_json::Value, before: serde_json::Value| {
        Write::new(format!("dp {id}"), v).before(Some(before))
    };
    let start = h.plan(&Command::CableStart { amps: 10 }).await.unwrap();
    assert_eq!(
        start,
        Outcome {
            applied: Applied::Done,
            writes: vec![dp("18", json!(true), json!(false)), dp("4", json!(10), json!(16))]
        }
    );
    assert_eq!(h.plan(&Command::CableStop).await.unwrap().writes, vec![dp("18", json!(false), json!(false))]);
    assert!(matches!(h.plan(&Command::CableSetCurrent(32)).await, Err(DriverError::OutOfRange(_))));
    assert_eq!(h.plan(&Command::SolisReconnect).await.unwrap().applied, Applied::NotMine);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(sim.sets().is_empty(), "a dry run sends nothing");
    let done = h.execute(&Command::CableSetCurrent(12)).await.unwrap();
    assert_eq!(done, Outcome { applied: Applied::Done, writes: vec![dp("4", json!(12), json!(16))] });
    eventually("set", || sim.sets().len() == 1).await;
}

/// The extra entities come from the DPs already reported, with the Tuya cloud entities they
/// stand for.
#[tokio::test]
async fn extra_entities_from_the_dps() {
    use crate::entity::Value as V;
    let sim = sim().await;
    let h = connected(&sim).await;
    let es = h.entities();
    let v = |k: &str| es.iter().find(|e| e.key == k).unwrap().value.clone();
    assert_eq!(v("energy_total_kwh"), V::Number(0.01));
    assert_eq!(v("energy_kwh"), V::Number(0.0));
    assert_eq!(v("temperature_c"), V::Number(18.0));
    assert_eq!(v("mode"), V::Text("charge_now".into()));
    assert_eq!(v("switch"), V::Bool(false));
    assert_eq!(v("status_raw"), V::Text("charger_insert".into()));
    assert_eq!(v("fault"), V::Bool(false));
    assert_eq!(v("firmware"), V::Text("2.0.0".into()));
    let keys: Vec<_> = es.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(keys, HA_ENTITIES.iter().map(|(k, _)| *k).collect::<Vec<_>>());
    assert!(es.iter().zip(HA_ENTITIES).all(|(e, (_, ha))| e.ha_entity.as_deref() == ha));
}

/// A frame whose GCM tag fails ends the session; the worker reconnects and picks up the state.
#[tokio::test]
async fn bad_tag_drops_the_session() {
    let sim = sim().await;
    let h = connected(&sim).await;
    sim.corrupt_next_frame();
    sim.set_dp("3", json!("charger_end"));
    eventually("reconnect", || sim.handshakes() == 2 && h.connected()).await;
    assert_eq!(h.dps().get("3"), Some(&json!("charger_end")));
}

/// The wrong local key, or a 3.4 client, never gets a session with a 3.5 device.
#[tokio::test]
async fn wrong_key_or_version_never_connects() {
    let sim = sim().await;
    let bad_key =
        EvChargerHandle::spawn(ChargerConfig { local_key: "fedcba9876543210".into(), ..cfg(sim.addr) }).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!bad_key.connected());
    drop(bad_key);
    let v34 = EvChargerHandle::spawn(ChargerConfig { version: Version::V34, ..cfg(sim.addr) }).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!v34.connected());
    assert_eq!(sim.handshakes(), 0);
}

/// The production config: protocol 3.5, the shared timings.
#[test]
fn production_config() {
    let c = config("192.168.1.179:6668".parse().unwrap(), "id", KEY);
    assert_eq!((c.version, c.heartbeat), (Version::V35, Duration::from_secs(10)));
}
