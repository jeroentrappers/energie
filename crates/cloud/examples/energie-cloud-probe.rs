//! Read-only live check of one cloud integration: one fetch, the parsed reading printed.
//!
//! ```text
//! energie-cloud-probe kia     /etc/energie/cloud-kia.json     [--state-dir /var/lib/energie] [--vehicle EV3]
//! energie-cloud-probe cupra   /etc/energie/cloud-cupra.json
//! energie-cloud-probe lgthinq /etc/energie/cloud-lgthinq.json
//! energie-cloud-probe fluvius /etc/energie/cloud-fluvius.json [--state-dir /var/lib/energie] [--ean 5414...]
//! energie-cloud-probe forecast-solar
//! ```
//!
//! Nothing is written to a car, an airco or a portal: Kia reads the cached state (no wake-up),
//! LG reads the two units' state, Fluvius and the EU Data Act portal are read-only anyway. A Kia
//! or Fluvius token that gets refreshed is written to the state dir (never to /etc), the same
//! file the service uses. Readings carry no secrets; tokens are never printed.

use std::path::PathBuf;

use chrono::Utc;
use energie_cloud::cupra::{Cupra, CupraConfig, CupraEndpoints};
use energie_cloud::fluvius::{Fluvius, FluviusConfig, FluviusEndpoints, reading};
use energie_cloud::forecast_solar::{ForecastSolar, ForecastSolarConfig};
use energie_cloud::kia::{Kia, KiaConfig, KiaEndpoints};
use energie_cloud::lgthinq::{Thinq, ThinqConfig};
use energie_cloud::{CloudReading, CloudSource};

fn usage() -> ! {
    eprintln!(
        "usage: energie-cloud-probe <kia|cupra|lgthinq|fluvius|forecast-solar> [credentials.json] \
         [--state-dir DIR] [--vehicle NAME] [--ean EAN]"
    );
    std::process::exit(2)
}

fn print(source: &dyn CloudSource, reading: &CloudReading) {
    println!("{reading:#?}");
    println!("entities:");
    for (id, value) in reading.entity_states() {
        println!("  {id} = {}", value.map_or("unavailable".to_string(), |v| format!("{v:?}")));
    }
    let writable: Vec<_> = source.entities().into_iter().filter(|e| e.writable).map(|e| e.entity_id).collect();
    if !writable.is_empty() {
        println!("writable (not touched by this probe): {}", writable.join(", "));
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let kind = args.next().unwrap_or_else(|| usage());
    let mut positional = Vec::new();
    let (mut state_dir, mut vehicle, mut ean) = (PathBuf::from("/var/lib/energie"), None, None);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--state-dir" => state_dir = args.next().unwrap_or_else(|| usage()).into(),
            "--vehicle" => vehicle = Some(args.next().unwrap_or_else(|| usage())),
            "--ean" => ean = Some(args.next().unwrap_or_else(|| usage())),
            _ if a.starts_with("--") => usage(),
            _ => positional.push(a),
        }
    }
    let creds = || PathBuf::from(positional.first().cloned().unwrap_or_else(|| usage()));
    let result = match kind.as_str() {
        "kia" => {
            let cfg = KiaConfig {
                credentials: creds(),
                credentials_store: None,
                state_file: state_dir.join("kia-token.json"),
                vehicle,
                poll_interval_s: 1800,
                endpoints: KiaEndpoints::default(),
            };
            match Kia::new(cfg) {
                Ok(mut k) => {
                    let before = k.token().valid_until.clone();
                    let r = k.status().await.map(CloudReading::Ev3);
                    let refreshed = k.token().valid_until != before;
                    println!("token refreshed: {}", if refreshed { "yes (saved to the state dir)" } else { "no" });
                    r.map(|r| print(&k, &r))
                }
                Err(e) => Err(e),
            }
        }
        "cupra" => {
            let cfg = CupraConfig {
                credentials: creds(),
                credentials_store: None,
                poll_interval_s: 900,
                endpoints: CupraEndpoints::default(),
            };
            match Cupra::new(cfg) {
                Ok(mut c) => c.fetch().await.map(|r| print(&c, &CloudReading::Buzz(r))),
                Err(e) => Err(e),
            }
        }
        "lgthinq" => {
            let cfg = ThinqConfig {
                credentials: creds(),
                beneden: "Airco beneden".into(),
                zolder: "Airco zolder".into(),
                poll_interval_s: 300,
                base_url: None,
            };
            match Thinq::new(cfg) {
                Ok(mut t) => t.states().await.map(|r| print(&t, &CloudReading::Airco(r))),
                Err(e) => Err(e),
            }
        }
        "fluvius" => {
            let cfg = FluviusConfig {
                credentials: creds(),
                state_file: state_dir.join("fluvius-token.json"),
                ean,
                poll_interval_s: 21600,
                endpoints: FluviusEndpoints::default(),
            };
            match Fluvius::new(cfg) {
                Ok(mut f) => {
                    let meter = match f.unavailable() {
                        CloudReading::Fluvius(r) => r.meter,
                        _ => String::new(),
                    };
                    f.peaks_at(Utc::now()).await.map(|p| print(&f, &CloudReading::Fluvius(reading(&meter, &p))))
                }
                Err(e) => Err(e),
            }
        }
        "forecast-solar" => match ForecastSolar::new(ForecastSolarConfig::home()) {
            Ok(mut fs) => fs.forecast_at(Utc::now()).await.map(|r| print(&fs, &CloudReading::Forecast(r))),
            Err(e) => Err(e),
        },
        _ => usage(),
    };
    if let Err(e) = result {
        eprintln!("{kind}: {e}");
        std::process::exit(1);
    }
}
