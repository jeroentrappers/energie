//! Read-only probe of a real HomeWizard P1 meter: one `GET /api/measurement`.
//!
//! `HW_TOKEN=... cargo run -p energie-drivers --example probe_homewizard -- 192.168.1.30:443 aabbccddeeff`

use energie_drivers::homewizard::{HomeWizard, HomeWizardConfig};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().expect("addr").parse().expect("ip:port");
    let serial = args.next().expect("serial");
    let token = std::env::var("HW_TOKEN").expect("HW_TOKEN");
    let hw = HomeWizard::new(HomeWizardConfig::p1(addr, token, &serial)).expect("client");
    println!("{:?}", hw.measurement().await);
}
