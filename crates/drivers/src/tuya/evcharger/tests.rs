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
        refresh: Duration::from_millis(50),
        step_interval: Duration::from_millis(30),
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

/// DP 6 as the real charger sends it at rest (all zeros), the live sample's numbers (229 V,
/// 5.3 A, 1213 W) through the encoding, and what is not a meter reading.
#[test]
fn phase_meter_decoding() {
    let at = |v: &str| dps(json!({ "6": v }));
    assert_eq!(phase(&at("AAAAAAAAAAA=")), Some((0.0, 0.0, 0.0)));
    assert_eq!(phase(&at(&encode_phase(229.0, 5.3, 1213.0))), Some((229.0, 5.3, 1213.0)));
    // 0x08F2 = 229.0 V, 0x0014B4 = 5300 mA, 0x0004BD = 1213 W.
    assert_eq!(encode_phase(229.0, 5.3, 1213.0), "CPIAFLQABL0=");
    for bad in ["", "AAAA", "not base64!", "AAAAAAAAAAAA"] {
        assert_eq!(phase(&at(bad)), None, "{bad:?}");
    }
    assert_eq!(phase(&dps(json!({ "6": 5 }))), None);
    assert_eq!(phase(&Dps::new()), None);
}

/// A live DP 6 is the reading, whatever DP 9 says (the real charger sent 1 at 1.2 kW); at rest
/// (all zeros) the reading falls back to DP 9 and the set current.
#[test]
fn the_meter_wins_over_dp9() {
    let mut d = evcharger_dps();
    d.insert("3".into(), json!("charger_charging"));
    d.insert("4".into(), json!(6));
    d.insert("9".into(), json!(1));
    d.insert("6".into(), json!(encode_phase(229.0, 5.3, 1213.0)));
    let c = cable_from_dps(&d);
    assert_eq!((c.voltage_v, c.current_a, c.power_kw), (Some(229.0), Some(5.3), Some(1.213)));
    assert_eq!((c.status.as_deref(), c.set_current_a), (Some("charging"), Some(6.0)));
    d.insert("6".into(), json!("AAAAAAAAAAA="));
    let c = cable_from_dps(&d);
    assert_eq!((c.voltage_v, c.current_a, c.power_kw), (None, Some(6.0), Some(1.38)), "fallback");
    // Stopped: DP 6 still holds the session's last values until the next refresh.
    d.insert("3".into(), json!("charger_end"));
    d.insert("9".into(), json!(0));
    d.insert("6".into(), json!(encode_phase(229.0, 5.3, 1213.0)));
    let c = cable_from_dps(&d);
    assert_eq!((c.status.as_deref(), c.current_a, c.power_kw), (Some("charged"), Some(0.0), Some(0.0)), "stale");
}

/// The worker asks for DP 6 every refresh; the charger never sends it in a query or a push.
#[tokio::test]
async fn the_meter_is_refreshed() {
    let sim = sim().await;
    let h = connected(&sim).await;
    // The first request goes out at once, so DP 6 may already be there; it never comes without
    // one (the query does not carry it).
    assert!(h.dps().get("6").is_none() || sim.refreshes() > 0, "not in the query");
    eventually("refreshes", || sim.refreshes() >= 3).await;
    assert_eq!(h.dps().get("6"), Some(&json!("AAAAAAAAAAA=")));
}

/// A zero refresh never asks for the meter.
#[tokio::test]
async fn a_zero_refresh_never_asks() {
    let sim = sim().await;
    let h = EvChargerHandle::spawn(ChargerConfig { refresh: Duration::ZERO, ..cfg(sim.addr) }).unwrap();
    eventually("connect", || h.connected()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(sim.refreshes(), 0);
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
    eventually("the meter", || phase(&h.dps()).is_some_and(|(v, _, _)| v > 0.0)).await;
    let Reading::Cable(c) = h.read().await.unwrap() else { panic!() };
    assert_eq!(
        (c.status.as_deref(), c.voltage_v, c.current_a, c.power_kw),
        (Some("charging"), Some(230.0), Some(10.0), Some(2.3))
    );
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

/// A stop switches off. The charger then ends the session (`charger_end`, which reads
/// `charged`), but the car is not full: after energie's own stop it reads `plugged_in`, so the
/// logic does not hold back the next start.
#[tokio::test]
async fn stop_switches_off_and_reads_plugged_in() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    h.apply(&Command::CableStart { amps: 8 }).await.unwrap();
    eventually("charging", || h.dps().get("3") == Some(&json!("charger_charging"))).await;
    assert_eq!(h.apply(&Command::CableStop).await.unwrap(), Applied::Done);
    eventually("ended", || h.dps().get("3") == Some(&json!("charger_end"))).await;
    assert_eq!(sim.sets()[1].1, dps(json!({"18": false})));
    let Reading::Cable(c) = h.read().await.unwrap() else { panic!() };
    assert_eq!((c.status.as_deref(), c.current_a), (Some("plugged_in"), Some(0.0)));
    // It starts again from there.
    h.apply(&Command::CableStart { amps: 8 }).await.unwrap();
    eventually("charging again", || h.dps().get("3") == Some(&json!("charger_charging"))).await;
}

/// A session the charger ends by itself (the car is full) reads `charged`, also when energie
/// stopped an earlier session: charging again clears that, whoever started it.
#[tokio::test]
async fn an_end_of_its_own_reads_charged() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    let status = |c: Reading| match c {
        Reading::Cable(c) => c.status,
        _ => panic!(),
    };
    sim.set_dp("3", json!("charger_charging"));
    sim.set_dp("3", json!("charger_end"));
    eventually("ended", || h.dps().get("3") == Some(&json!("charger_end"))).await;
    assert_eq!(status(h.read().await.unwrap()).as_deref(), Some("charged"), "never stopped by energie");
    h.apply(&Command::CableStop).await.unwrap();
    assert_eq!(status(h.read().await.unwrap()).as_deref(), Some("plugged_in"), "stopped by energie");
    eventually("the stop arrived", || sim.sets().len() == 1).await;
    sim.set_dp("3", json!("charger_charging")); // started from the app
    eventually("charging", || h.dps().get("3") == Some(&json!("charger_charging"))).await;
    sim.set_dp("3", json!("charger_end"));
    eventually("ended", || h.dps().get("3") == Some(&json!("charger_end"))).await;
    assert_eq!(status(h.read().await.unwrap()).as_deref(), Some("charged"), "the car's own end");
}

/// The current goes to DP 4 within 6..16 A; anything else is refused without a write. A new
/// current while charging changes the draw.
#[tokio::test]
async fn set_current_within_range() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    h.apply(&Command::CableStart { amps: 6 }).await.unwrap();
    assert_eq!(h.apply(&Command::CableSetCurrent(13)).await.unwrap(), Applied::Done);
    eventually("13 A", || phase(&h.dps()).is_some_and(|(_, a, _)| a == 13.0)).await;
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
    let done = h.execute(&Command::CableSetCurrent(14)).await.unwrap();
    assert_eq!(done, Outcome { applied: Applied::Done, writes: vec![dp("4", json!(14), json!(16))] });
    eventually("set", || sim.sets().len() == 1).await;
    eventually("14 A reported", || h.dps().get("4") == Some(&json!(14))).await;
    let stepped = h.plan(&Command::CableSetCurrent(6)).await.unwrap().writes;
    assert_eq!(stepped, vec![dp("4", json!(6), json!(14)).note("in steps of 3 A")]);
}

/// A drop of more than 3 A goes down in steps of 3 A, so the car follows before the charger
/// sees it over the new setting (it latched an overcurrent fault on 13 → 6 A). Raising, and a
/// small drop, are written at once; a new current or a stop replaces the steps.
#[tokio::test]
async fn a_large_drop_goes_down_in_steps() {
    let sim = sim().await;
    let mut h = connected(&sim).await;
    h.apply(&Command::CableStart { amps: 16 }).await.unwrap();
    assert_eq!(h.apply(&Command::CableSetCurrent(6)).await.unwrap(), Applied::Done);
    eventually("the last step", || sim.sets().len() == 5).await;
    let amps: Vec<_> = sim.sets().iter().skip(1).map(|(_, m)| m["4"].clone()).collect();
    assert_eq!(amps, [json!(13), json!(10), json!(7), json!(6)]);
    let gaps: Vec<_> = sim.sets().windows(2).skip(1).map(|w| w[1].0 - w[0].0).collect();
    assert!(gaps.iter().all(|g| *g >= Duration::from_millis(25)), "{gaps:?}");

    h.apply(&Command::CableSetCurrent(9)).await.unwrap();
    h.apply(&Command::CableSetCurrent(7)).await.unwrap();
    eventually("two sets", || sim.sets().len() == 7).await;
    assert_eq!((sim.sets()[5].1["4"].clone(), sim.sets()[6].1["4"].clone()), (json!(9), json!(7)));

    h.apply(&Command::CableSetCurrent(16)).await.unwrap();
    h.apply(&Command::CableSetCurrent(6)).await.unwrap();
    h.apply(&Command::CableStop).await.unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let tail: Vec<_> = sim.sets().iter().skip(7).map(|(_, m)| m.clone()).collect();
    assert_eq!(tail, [dps(json!({"4": 16})), dps(json!({"4": 13})), dps(json!({"18": false}))]);
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
    assert_eq!((c.version, c.heartbeat, c.refresh), (Version::V35, Duration::from_secs(10), Duration::from_secs(5)));
}
