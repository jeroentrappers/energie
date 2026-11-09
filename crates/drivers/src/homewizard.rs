//! HomeWizard P1 meter, local API v2 (`homewizard` integration, `sensor.p1_meter_*`).
//!
//! `GET https://<meter>/api/measurement` with `Authorization: Bearer <token>` and
//! `X-Api-Version: 2`. The meter serves a certificate issued by HomeWizard's "Appliance Access
//! CA" for the name `<serial>.p1dongle.device.homewizard.energy` (SAN), so the client connects to
//! the meter's IP but verifies the chain against that CA and the certificate against that name.
//! The live meter answers 401 `{"error":"user:unauthorized"}` to a missing or wrong token.

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use energie_core::command::Command;
use energie_core::world::P1;
use serde::Deserialize;

use crate::driver::{Actuator, Applied, Reading, Source};
use crate::error::{DriverError, Result};

/// HomeWizard's published "Appliance Access CA" (the `homewizard_energy` v2 client pins the same
/// PEM). Valid 2021-12-18 .. 2031-12-16.
pub const HOMEWIZARD_CA_PEM: &str = "-----BEGIN CERTIFICATE-----
MIIDITCCAgkCFDn7cwYLioTM3VxdAygLl/Px9ovFMA0GCSqGSIb3DQEBCwUAME0x
CzAJBgNVBAYTAk5MMQswCQYDVQQIDAJaSDETMBEGA1UECgwKSG9tZVdpemFyZDEc
MBoGA1UEAwwTQXBwbGlhbmNlIEFjY2VzcyBDQTAeFw0yMTEyMTgxOTEyMTJaFw0z
MTEyMTYxOTEyMTJaME0xCzAJBgNVBAYTAk5MMQswCQYDVQQIDAJaSDETMBEGA1UE
CgwKSG9tZVdpemFyZDEcMBoGA1UEAwwTQXBwbGlhbmNlIEFjY2VzcyBDQTCCASIw
DQYJKoZIhvcNAQEBBQADggEPADCCAQoCggEBAPBIvW8NRffqdvzHZY0M32fQHiGm
pJgNGhiaQmpJfRDhT9yihM0S/hYcN8IqnfrMqoCQb/56Ub0+dZizmtfcGsE+Lpm1
K1znkWqSDlpnuTNOb70TrsxBmbFuNOZQEi/xOjzT2j98wT0GSfxz1RVq6lZhDRRz
xoe08+Xo4+ttUGanfOggJi0BXygeFEVBpbctVVJ9EgqeEE9itjcMlcxMe1QN14f8
hCcOnId+9PSsdmyUCLrTB0FVYrbNfbJPk/vMU57fu6swBjWhYBxPx9ZhFy+7WnPR
9BFg4seHNVQIqZNrf1YwBXlmZQIL32SRPaiH/+AVNMrYGXBvncY0Km6ZHIMCAwEA
ATANBgkqhkiG9w0BAQsFAAOCAQEA6ybM8xm0PCXg8Rr/q0v1vPxQy44PmwXTDj0e
r2vW4ZMiEwXZCp0Kk2K16KJYz4iJyfiQk8ikAIMiRSbyXzmyQ7XmL1O4l4d8E1Pg
8EImvcyoBxFhd0Lq7VKriLc8Bw8SXbahPMGT+Y8Yz0uIsLAYVwlkLfgppVPmBaLD
QautcQnI8WxPvCIQf5anyzgAyJC5ac6/CkB+iyPcuWcG3RMYvXnC0QoTlRa5YMlE
FweVDlT2C/MdDyOxiAD/H1EP/eaySnU0zsxyD0yNFRKsQfQ+UJEPd2GS1AGA1lTy
CGdyYj/Gghrusw0hM4rYXQSERWGF0mpEnuJ+7bHDolHu0rzgTQ==
-----END CERTIFICATE-----
";

/// How to reach and trust one meter.
#[derive(Debug, Clone)]
pub struct HomeWizardConfig {
    /// IP and port of the meter (`192.168.1.30:443`).
    pub addr: SocketAddr,
    /// The local API v2 bearer token (from the HA config entry; never commit it).
    pub token: String,
    /// Name the certificate is checked against: `<serial>.p1dongle.device.homewizard.energy`.
    pub server_name: String,
    /// PEM of the CA that issued the meter's certificate.
    pub ca_pem: String,
    /// Request timeout. HA's coordinator polls every second.
    pub timeout: Duration,
}

impl HomeWizardConfig {
    /// A P1 meter on the LAN, trusted through HomeWizard's CA. `serial` is the meter's serial
    /// (`GET /api` → `serial`, e.g. `aabbccddeeff`).
    pub fn p1(addr: SocketAddr, token: impl Into<String>, serial: &str) -> Self {
        HomeWizardConfig {
            addr,
            token: token.into(),
            server_name: format!("{serial}.p1dongle.device.homewizard.energy"),
            ca_pem: HOMEWIZARD_CA_PEM.to_string(),
            timeout: Duration::from_secs(5),
        }
    }
}

/// The fields of `/api/measurement` the stack reads. Field names are the v2 API's; a meter that
/// lacks a field (single phase, no export) leaves it `None`, as HA does not create the entity.
#[derive(Debug, Deserialize)]
struct Measurement {
    power_w: Option<f64>,
    current_l1_a: Option<f64>,
    current_l2_a: Option<f64>,
    current_l3_a: Option<f64>,
    energy_import_kwh: Option<f64>,
    energy_export_kwh: Option<f64>,
    monthly_power_peak_w: Option<f64>,
}

/// Parse an `/api/measurement` body into the P1 reading.
pub fn parse_measurement(body: &str) -> Result<P1> {
    let m: Measurement = serde_json::from_str(body)?;
    Ok(P1 {
        power_w: m.power_w,
        phase_a: [m.current_l1_a, m.current_l2_a, m.current_l3_a],
        import_kwh: m.energy_import_kwh,
        export_kwh: m.energy_export_kwh,
        month_peak_w: m.monthly_power_peak_w,
    })
}

/// Polling client for one meter.
pub struct HomeWizard {
    client: reqwest::Client,
    url: String,
    token: String,
}

impl HomeWizard {
    pub fn new(cfg: HomeWizardConfig) -> Result<Self> {
        let ca = reqwest::Certificate::from_pem(cfg.ca_pem.as_bytes())?;
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .tls_built_in_root_certs(false)
            .add_root_certificate(ca)
            .resolve(&cfg.server_name, cfg.addr)
            .https_only(true)
            .timeout(cfg.timeout)
            .build()?;
        let url = format!("https://{}:{}/api/measurement", cfg.server_name, cfg.addr.port());
        Ok(HomeWizard { client, url, token: cfg.token })
    }

    /// One `GET /api/measurement`.
    pub async fn measurement(&self) -> Result<P1> {
        let resp = self.client.get(&self.url).bearer_auth(&self.token).header("X-Api-Version", "2").send().await?;
        match resp.status().as_u16() {
            200 => parse_measurement(&resp.text().await?),
            401 | 403 => Err(DriverError::Unauthorized),
            s => Err(DriverError::Status(s)),
        }
    }
}

#[async_trait]
impl Source for HomeWizard {
    fn name(&self) -> &'static str {
        "homewizard"
    }

    async fn read(&mut self) -> Result<Reading> {
        Ok(Reading::P1(self.measurement().await?))
    }

    fn unavailable(&self) -> Reading {
        Reading::P1(P1::default())
    }
}

/// The meter takes no commands from the stack.
#[async_trait]
impl Actuator for HomeWizard {
    async fn apply(&mut self, _cmd: &Command) -> Result<Applied> {
        Ok(Applied::NotMine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::homewizard::HomeWizardSim;

    /// Real `/api/measurement` response of the P1 meter (HWE-P1, firmware 6.0503, API 2.3.0),
    /// captured 2026-11-07 14:03.
    pub(crate) const FIXTURE: &str = include_str!("../fixtures/homewizard_measurement.json");

    fn client(sim: &HomeWizardSim, token: &str) -> HomeWizard {
        HomeWizard::new(sim.config(token)).unwrap()
    }

    /// The captured response maps onto `sensor.p1_meter_vermogen`, `_stroom_fase_1..3` (signed),
    /// `_energie_import/export` and `_piekverbruik_huidige_maand`.
    #[test]
    fn real_fixture_maps_to_p1_entities() {
        let p1 = parse_measurement(FIXTURE).unwrap();
        assert_eq!(p1.power_w, Some(3145.0));
        assert_eq!(p1.phase_a, [Some(-1.29), Some(15.15), Some(14.3)]);
        assert_eq!(p1.import_kwh, Some(11885.493));
        assert_eq!(p1.export_kwh, Some(8042.631));
        assert_eq!(p1.month_peak_w, Some(3892.0));
    }

    /// Fields a meter does not report stay `None` (HA creates no entity for them).
    #[test]
    fn missing_fields_are_none() {
        let p1 = parse_measurement(r#"{"power_w": 12}"#).unwrap();
        assert_eq!(p1, P1 { power_w: Some(12.0), ..P1::default() });
    }

    /// A body that is not the measurement object is a parse error, not a zero reading.
    #[test]
    fn malformed_body_is_an_error() {
        assert!(matches!(parse_measurement("<html>"), Err(DriverError::Malformed(_))));
        assert!(matches!(parse_measurement(r#"{"power_w":"high"}"#), Err(DriverError::Malformed(_))));
    }

    /// End to end over HTTPS: the client trusts the simulator's CA, sends the bearer token and
    /// the API version header, and reads the live-shaped measurement.
    #[tokio::test]
    async fn reads_measurement_from_simulator() {
        let sim = HomeWizardSim::start("secret-token").await;
        let mut hw = client(&sim, "secret-token");
        assert_eq!(hw.name(), "homewizard");
        let Reading::P1(p1) = hw.read().await.unwrap() else { panic!() };
        assert_eq!(p1.power_w, Some(3145.0));
        sim.set_measurement(serde_json::json!({"power_w": -2500, "current_l1_a": -11.0}));
        let Reading::P1(p1) = hw.read().await.unwrap() else { panic!() };
        assert_eq!(p1.power_w, Some(-2500.0));
        assert_eq!(p1.phase_a[0], Some(-11.0));
        assert_eq!(sim.last_api_version().as_deref(), Some("2"));
        assert_eq!(hw.unavailable(), Reading::P1(P1::default()));
        assert_eq!(hw.apply(&Command::RefreshPrices).await.unwrap(), Applied::NotMine);
    }

    /// A wrong token gets the meter's 401 and surfaces as `Unauthorized`.
    #[tokio::test]
    async fn bad_token_is_unauthorized() {
        let sim = HomeWizardSim::start("secret-token").await;
        let err = client(&sim, "wrong").measurement().await.unwrap_err();
        assert!(matches!(err, DriverError::Unauthorized), "{err}");
    }

    /// Other HTTP errors carry their status; a broken body is a parse error.
    #[tokio::test]
    async fn server_errors_and_garbage() {
        let sim = HomeWizardSim::start("t").await;
        let hw = client(&sim, "t");
        sim.set_status(503);
        assert!(matches!(hw.measurement().await, Err(DriverError::Status(503))));
        sim.set_status(200);
        sim.set_raw_body(Some("not json".into()));
        assert!(matches!(hw.measurement().await, Err(DriverError::Malformed(_))));
    }

    /// The certificate must chain to the configured CA: HomeWizard's real CA does not vouch for
    /// the simulator, so the TLS handshake fails.
    #[tokio::test]
    async fn untrusted_certificate_is_rejected() {
        let sim = HomeWizardSim::start("t").await;
        let mut cfg = sim.config("t");
        cfg.ca_pem = HOMEWIZARD_CA_PEM.into();
        let err = HomeWizard::new(cfg).unwrap().measurement().await.unwrap_err();
        assert!(matches!(err, DriverError::Http(_)), "{err}");
    }

    /// The certificate must also be issued for the configured device name.
    #[tokio::test]
    async fn wrong_device_name_is_rejected() {
        let sim = HomeWizardSim::start("t").await;
        let mut cfg = sim.config("t");
        cfg.server_name = "other.p1dongle.device.homewizard.energy".into();
        assert!(HomeWizard::new(cfg).unwrap().measurement().await.is_err());
    }

    /// The production constructor targets the meter's SAN name and HomeWizard's CA.
    #[test]
    fn p1_config_uses_serial_name_and_homewizard_ca() {
        let cfg = HomeWizardConfig::p1("192.168.1.30:443".parse().unwrap(), "tok", "aabbccddeeff");
        assert_eq!(cfg.server_name, "aabbccddeeff.p1dongle.device.homewizard.energy");
        assert_eq!(cfg.ca_pem, HOMEWIZARD_CA_PEM);
        assert!(HomeWizard::new(cfg).is_ok());
    }

    /// Garbage instead of a PEM certificate fails at construction.
    #[test]
    fn bad_ca_pem_is_rejected() {
        let mut cfg = HomeWizardConfig::p1("127.0.0.1:443".parse().unwrap(), "tok", "x");
        cfg.ca_pem = "-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n".into();
        assert!(HomeWizard::new(cfg).is_err());
    }
}
