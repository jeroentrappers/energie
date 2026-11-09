//! The whole binary in control mode, on the wall clock, against the protocol simulators: the
//! start-up wait, a command from the API through the gate to the inverter and its read-back,
//! the kill switch (API and entity), the release at shutdown, and the dry run.

use std::net::SocketAddr;
use std::time::Duration;

use energie_drivers::sim::mqtt::{Broker, SmartEvseSim};
use energie_drivers::sim::solis::SolisSim;
use energie_drivers::sim::tuya::TuyaSim;
use serde_json::{Value, json};

use crate::app::start;
use crate::config::Config;
use crate::drivers::Drivers;
use crate::ha::ws::tests::until;

const KEY: &str = "0123456789abcdef";

async fn http(addr: SocketAddr, method: &str, path: &str, body: Option<Value>) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    (buf[9..12].parse().unwrap(), buf.split("\r\n\r\n").nth(1).unwrap_or_default().to_string())
}

fn config(dir: &std::path::Path, solis: SocketAddr, cable: SocketAddr, broker: SocketAddr, control: &str) -> Config {
    let key = dir.join("tuya.key");
    std::fs::write(&key, KEY).unwrap();
    let text = format!(
        "mode = \"control\"\nlisten = \"127.0.0.1:0\"\nstate_dir = {dir:?}\ntick_ms = 100\n\
         [[device]]\nid = \"house\"\nkind = \"sim\"\nrole = \"*\"\n\
         [[device]]\nid = \"solis\"\nkind = \"solis_modbus\"\nrole = \"battery_inverter\"\ncontrol = \"battery_inverter\"\nread_only = false\n\
         address = \"{solis}\"\nframe_gap_ms = 0\nsettings_every = 1\npoll_ms = 50\n\
         [[device]]\nid = \"cable\"\nkind = \"tuya_cable\"\nrole = \"charger:ev3\"\ncontrol = \"charger:ev3\"\nread_only = false\n\
         address = \"{cable}\"\ndevice_id = \"bf00000000000000000000\"\nlocal_key_file = {key:?}\nstart_delay_ms = 10\npoll_ms = 50\n\
         [[device]]\nid = \"evse\"\nkind = \"smartevse_mqtt\"\nrole = \"charger:buzz\"\ncontrol = \"charger:buzz\"\nread_only = false\n\
         host = \"127.0.0.1\"\nport = {}\npoll_ms = 50\n\
         [control]\nverify_s = 5\nrelease_on_shutdown = [\"solis\", \"evse\"]\n{control}",
        broker.port()
    );
    Config::parse(&text).unwrap()
}

fn rc_mode_writes(sim: &SolisSim) -> Vec<u16> {
    sim.writes().into_iter().filter(|(r, _)| *r == 43135).map(|(_, v)| v).collect()
}

/// Control mode end to end: commands wait for the devices' first readings, a command from the
/// API reaches the inverter and is audited as verified once it reads back; the kill switch
/// stops every write until it is switched on again (here through its entity); at shutdown the
/// RC mode is released and the Buzz override handed back to the charger.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn control_mode_end_to_end() {
    let solis = SolisSim::start().await;
    let tuya = TuyaSim::start(energie_drivers::tuya::protocol::parse_key(KEY).as_ref().unwrap()).await;
    let broker = Broker::start(None).await;
    let evse = SmartEvseSim::start(broker.addr, "SmartEVSE/1234", None).await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path(), solis.addr, tuya.addr, broker.addr, "");
    let drivers = Drivers::new(&cfg).unwrap();
    let r = start(cfg, &drivers).await.unwrap();
    until(|| r.rt.lock().unwrap().tick > 0).await;
    let d = r.rt.lock().unwrap().router.device_health(crate::plug::Device::Solis).unwrap();
    assert!(d.reads_ok > 0, "started only after the first reading");

    let (code, _) =
        http(r.addr, "POST", "/api/entities/world_battery_inverter_rc_mode", Some(json!({"value": "ForceDischarge"})))
            .await;
    assert_eq!(code, 200);
    until(|| rc_mode_writes(&solis).contains(&2)).await;
    let verified = || {
        let snap = r.rt.lock().unwrap().snapshot_rx().borrow().clone();
        let c = snap.control.clone().unwrap();
        c.recent.iter().any(|e| {
            e.command == energie_core::command::Command::SolisRcMode(energie_core::world::RcMode::ForceDischarge)
                && e.result == super::Verdict::Verified
        })
    };
    until(verified).await;
    let (code, body) = http(r.addr, "GET", "/api/control", None).await;
    assert_eq!(code, 200, "{body}");
    let view: Value = serde_json::from_str(&body).unwrap();
    assert_eq!((view["enabled"].as_bool(), view["dry_run"].as_bool()), (Some(true), Some(false)));

    // The kill switch: nothing reaches the devices any more, whoever asks.
    let (code, body) = http(r.addr, "POST", "/api/control", Some(json!({"enabled": false}))).await;
    assert_eq!(code, 200, "{body}");
    let before = (solis.writes().len(), tuya.sets().len(), evse.received().len());
    let (code, _) =
        http(r.addr, "POST", "/api/entities/world_battery_inverter_rc_mode", Some(json!({"value": "ForceCharge"})))
            .await;
    assert_eq!(code, 200);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!((solis.writes().len(), tuya.sets().len(), evse.received().len()), before);
    let snap = r.rt.lock().unwrap().snapshot_rx().borrow().clone();
    assert_eq!(snap.commands[0].error.as_deref(), Some("blocked: control is switched off"));
    assert!(!snap.commands[0].actuated);
    for (bad, code) in [(json!({"enabled": "ja"}), 400), (json!({"enabled": true, "device": "wasmachine"}), 400)] {
        assert_eq!(http(r.addr, "POST", "/api/control", Some(bad)).await.0, code);
    }
    let (code, _) = http(r.addr, "POST", "/api/control", Some(json!({"enabled": false, "device": "cable"}))).await;
    assert_eq!(code, 200);
    let (code, body) = http(r.addr, "POST", "/api/entities/control_enabled", Some(json!({"value": true}))).await;
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("\"value\":true"));
    assert!(std::fs::read_to_string(dir.path().join("control.json")).unwrap().contains("\"cable\": false"));

    r.stop().await;
    let modes = rc_mode_writes(&solis);
    assert_eq!(modes.last(), Some(&0), "released at shutdown: {modes:?}");
    assert!(evse.received().iter().any(|(t, p)| t == "SmartEVSE/1234/Set/CurrentOverride" && p == "0"));
    let audit = std::fs::read_to_string(dir.path().join("control-audit.jsonl")).unwrap();
    assert!(audit.contains(r#""command":{"SolisRcMode":"None"}"#), "{audit}");
}

/// The API is up at once while a device does not answer; the first decision waits for it, and a
/// stop does not have to wait for `startup_wait_s` either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serves_before_the_first_reading() {
    let solis = SolisSim::start().await;
    let broker = Broker::start(None).await;
    let _evse = SmartEvseSim::start(broker.addr, "SmartEVSE/1234", None).await;
    let dir = tempfile::tempdir().unwrap();
    let nobody = "127.0.0.1:9".parse().unwrap();
    let cfg = config(dir.path(), solis.addr, nobody, broker.addr, "dry_run = true\nstartup_wait_s = 30\n");
    let drivers = Drivers::new(&cfg).unwrap();
    let begin = std::time::Instant::now();
    let r = start(cfg, &drivers).await.unwrap();
    assert!(begin.elapsed() < Duration::from_secs(5), "start blocked for {:?}", begin.elapsed());
    let (code, body) = http(r.addr, "GET", "/api/state", None).await;
    assert_eq!(code, 200);
    assert!(body.contains(r#""tick":0"#), "{body}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(r.rt.lock().unwrap().tick, 0, "no decision without the first reading");
    let stopping = std::time::Instant::now();
    r.stop().await;
    assert!(stopping.elapsed() < Duration::from_secs(10), "stop waited for {:?}", stopping.elapsed());
}

/// Dry run: native reads, full decisions, every command worked out by its driver and logged
/// with the exact register writes, and nothing written, the shutdown release included. A
/// device that never answers delays the first decision by `startup_wait_s` and gets no command.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dry_run_writes_nothing() {
    let solis = SolisSim::start().await;
    let broker = Broker::start(None).await;
    let _evse = SmartEvseSim::start(broker.addr, "SmartEVSE/1234", None).await;
    let dir = tempfile::tempdir().unwrap();
    let nobody = "127.0.0.1:9".parse().unwrap();
    let cfg = config(dir.path(), solis.addr, nobody, broker.addr, "dry_run = true\nstartup_wait_s = 1\n");
    let drivers = Drivers::new(&cfg).unwrap();
    let r = start(cfg, &drivers).await.unwrap();
    until(|| r.rt.lock().unwrap().tick > 0).await;
    let (code, _) =
        http(r.addr, "POST", "/api/entities/world_battery_inverter_rc_mode", Some(json!({"value": "ForceDischarge"})))
            .await;
    assert_eq!(code, 200);
    let (code, _) =
        http(r.addr, "POST", "/api/entities/world_charger_ev3_set_current_a", Some(json!({"value": 10}))).await;
    assert_eq!(code, 200);
    let path = dir.path().join("control-dryrun.jsonl");
    until(|| std::fs::read_to_string(&path).is_ok_and(|t| t.contains(r#""SolisRcMode":"ForceDischarge""#))).await;
    let snap = r.rt.lock().unwrap().snapshot_rx().borrow().clone();
    assert!(snap.commands.iter().any(|c| c.error.as_deref() == Some("blocked: no reading of cable yet")));
    r.stop().await;
    assert!(solis.writes().is_empty(), "a dry run writes nothing: {:?}", solis.writes());
    let log = std::fs::read_to_string(&path).unwrap();
    let line: Value = log
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .find(|l| l["command"] == json!({"SolisRcMode": "ForceDischarge"}))
        .unwrap();
    assert_eq!(line["result"], "dry_run");
    assert_eq!(line["writes"][0], json!({"target": "holding 43135", "value": 2, "before": 1}));
    assert!(log.contains(r#""command":{"SolisRcMode":"None"}"#), "the release is logged too");
    assert!(!dir.path().join("control-audit.jsonl").exists());
}
