//! Soak test: the protocol simulators on 127.0.0.1, an `energie` in control mode against them
//! (Solis Modbus, Tuya 3.5 charger, SmartEVSE over MQTT, HomeWizard P1, Omnik, prices, the MQTT
//! bridge), dashboard clients on the web port, and device faults now and then (Tuya drops,
//! Solis stalls, a Tuya charger that stops answering heartbeats). Prints the child's RSS once a
//! minute. `SOAK_FAULTS=0` and `SOAK_CLIENTS=0` turn those off; `SOAK_LOG` sets its `RUST_LOG`.
//!
//! ```text
//! cargo build --release -p energie --examples
//! target/release/examples/soak target/release/energie [minutes]
//! ```

use std::io::Write as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use energie_drivers::sim::homewizard::{HomeWizardSim, SIM_SERVER_NAME};
use energie_drivers::sim::mqtt::{Broker, SmartEvseSim};
use energie_drivers::sim::omnik::OmnikSim;
use energie_drivers::sim::prices::PriceSim;
use energie_drivers::sim::solis::SolisSim;
use energie_drivers::sim::tuya::TuyaSim;
use energie_drivers::tuya::protocol::parse_key;

const KEY: &str = "0123456789abcdef";

fn rss_kb(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).map(|l| l.to_string()))
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let bin = args.get(1).cloned().unwrap_or_else(|| "target/release/energie".into());
    let minutes: u64 = args.get(2).and_then(|m| m.parse().ok()).unwrap_or(30);
    let faults = std::env::var("SOAK_FAULTS").map(|v| v != "0").unwrap_or(true);
    let clients = std::env::var("SOAK_CLIENTS").map(|v| v != "0").unwrap_or(true);
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();

    let solis = SolisSim::start().await;
    let tuya = TuyaSim::start_evcharger(parse_key(KEY).as_ref().unwrap()).await;
    let broker = Broker::start(None).await;
    let evse = SmartEvseSim::start(broker.addr, "SmartEVSE/1234", None).await;
    let hw = HomeWizardSim::start("hw-token").await;
    let omnik = OmnikSim::start().await;
    let prices = PriceSim::start("entsoe").await;
    let today = chrono::Local::now().date_naive();
    for d in [today.pred_opt().unwrap(), today, today.succ_opt().unwrap()] {
        prices.set_day(d, (0..96).map(|q| 60.0 + 40.0 * ((q as f64) / 15.0).sin()).collect());
    }
    omnik.set_power(Some(1200));
    solis.set_pv(800);
    solis.set_battery(-400);

    std::fs::write(d.join("key"), KEY).unwrap();
    std::fs::write(d.join("hw-token"), "hw-token").unwrap();
    std::fs::write(d.join("entsoe"), "entsoe").unwrap();
    std::fs::write(d.join("ca.pem"), hw.config("hw-token").ca_pem).unwrap();
    let serial = SIM_SERVER_NAME.split('.').next().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cli_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = format!(
        r#"
mode = "control"
listen = "127.0.0.1:{port}"
cli_listen = "127.0.0.1:{cli_port}"
state_dir = "{state}"
tick_ms = 1000

[[device]]
id = "homewizard"
kind = "homewizard"
role = "grid_meter"
address = "{hw}"
serial = "{serial}"
token_file = "{d}/hw-token"
ca_file = "{d}/ca.pem"

[[device]]
id = "solis"
kind = "solis_modbus"
role = "battery_inverter"
control = "battery_inverter"
read_only = false
address = "{solis}"

[[device]]
id = "omnik"
kind = "omnik"
role = "pv"
base_url = "{omnik}"

[[device]]
id = "smartevse"
kind = "smartevse_mqtt"
role = "charger:buzz"
control = "charger:buzz"
read_only = false
host = "127.0.0.1"
port = {mqtt}
prefix = "SmartEVSE/1234"
client_id = "soak-evse"

[[device]]
id = "cable"
kind = "tuya_evcharger"
role = "charger:ev3"
control = "charger:ev3"
read_only = false
address = "{tuya}"
device_id = "soak"
local_key_file = "{d}/key"

[[device]]
id = "prices"
kind = "energy_charts"
role = "price_feed"
control = "price_feed"
read_only = false
entsoe_token_file = "{d}/entsoe"
energy_charts_url = "{ec}"
entsoe_url = "{entsoe}"

[mqtt_publish]
enabled = true
broker = "127.0.0.1"
port = {mqtt}
client_id = "soak-bridge"
discovery = true
include = ["*"]
allow_set = true

[history]
enabled = true

[control]
enabled = true
dry_run = false
"#,
        state = d.join("state").display(),
        d = d.display(),
        hw = hw.config("hw-token").addr,
        solis = solis.addr,
        omnik = omnik.base_url(),
        mqtt = broker.addr.port(),
        tuya = tuya.addr,
        ec = prices.energy_charts_url(),
        entsoe = prices.entsoe_url(),
    );
    let cfg_path = d.join("energie.toml");
    std::fs::write(&cfg_path, cfg).unwrap();
    std::fs::create_dir_all(d.join("state")).unwrap();
    // A fresh state dir starts with control off; turn it on through the API below.
    let log = std::fs::File::create(d.join("energie.log")).unwrap();
    let mut child = Command::new(&bin)
        .arg("--config")
        .arg(&cfg_path)
        .env("RUST_LOG", std::env::var("SOAK_LOG").unwrap_or_else(|_| "warn".into()))
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("energie starts");
    let pid = child.id();
    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    for _ in 0..50 {
        if http.get(format!("{base}/healthz")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let _ = http.post(format!("{base}/api/control")).json(&serde_json::json!({"enabled": true})).send().await;

    // Dashboard clients: an SSE stream reopened every minute, and polls of state and history.
    if clients {
        let (h, b) = (http.clone(), base.clone());
        tokio::spawn(async move {
            loop {
                if let Ok(mut r) = h.get(format!("{b}/api/events")).send().await {
                    let until = Instant::now() + Duration::from_secs(60);
                    while Instant::now() < until {
                        match tokio::time::timeout(Duration::from_secs(5), r.chunk()).await {
                            Ok(Ok(Some(_))) | Err(_) => {}
                            _ => break,
                        }
                    }
                }
            }
        });
        let (h, b) = (http.clone(), base.clone());
        tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                n += 1;
                let _ = h.get(format!("{b}/api/state")).send().await.map(|r| r.bytes());
                if n.is_multiple_of(10) {
                    if let Ok(r) = h.get(format!("{b}/api/history?series=p1_w&step=60")).send().await {
                        let _ = r.bytes().await;
                    }
                    if let Ok(r) = h.get(format!("{b}/")).send().await {
                        let _ = r.bytes().await;
                    }
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    let start = Instant::now();
    let mut minute = 0;
    let mut out = std::io::stdout();
    while start.elapsed() < Duration::from_secs(minutes * 60) {
        tokio::time::sleep(Duration::from_secs(10)).await;
        let t = start.elapsed().as_secs();
        if faults {
            match (t / 10) % 18 {
                3 => tuya.drop_connection(),
                6 => solis.set_stall(true),
                8 => solis.set_stall(false),
                12 => tuya.answer_heartbeats(false),
                15 => tuya.answer_heartbeats(true),
                _ => {}
            }
        }
        // A changing house: a car drawing and the sun going up and down.
        omnik.set_power(Some(((t as f64 / 120.0).sin() * 1500.0 + 1500.0) as i64));
        evse.set("ChargeCurrent", if (t / 300).is_multiple_of(2) { "160" } else { "60" });
        if t / 60 > minute {
            minute = t / 60;
            let alive = child.try_wait().unwrap().is_none();
            let health =
                http.get(format!("{base}/healthz")).timeout(Duration::from_secs(5)).send().await.map(|r| r.status());
            writeln!(out, "{minute:>4} min  rss {:>7} kB  alive {alive}  healthz {health:?}", rss_kb(pid)).unwrap();
            if !alive {
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let logs = std::fs::read_to_string(d.join("energie.log")).unwrap_or_default();
    let applied = logs.lines().filter(|l| l.contains("command applied")).count();
    let failed = logs.lines().filter(|l| l.contains("command failed")).count();
    println!("commands applied {applied}, failed {failed}");
    let tail: Vec<&str> = logs.lines().rev().filter(|l| !l.contains("command applied")).take(20).collect();
    for l in tail.iter().rev() {
        println!("log: {l}");
    }
}
