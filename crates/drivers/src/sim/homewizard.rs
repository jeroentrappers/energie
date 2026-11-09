//! HomeWizard P1 meter, local API v2, over HTTPS with a bearer-token check.
//!
//! Serves `GET /api/measurement` (the captured live response until a test changes it) and
//! `GET /api` (device info). A missing or wrong token gets the meter's
//! `401 {"error":"user:unauthorized"}`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value, json};

use super::http::{HttpServer, TestPki, serve_tls};
use crate::homewizard::HomeWizardConfig;

/// The live meter's measurement, captured 2026-11-07.
pub const MEASUREMENT_FIXTURE: &str = include_str!("../../fixtures/homewizard_measurement.json");

/// Name the simulator's certificate is issued for (same shape as a real meter's SAN).
pub const SIM_SERVER_NAME: &str = "aabbccddeeff.p1dongle.device.homewizard.energy";

struct Inner {
    token: String,
    measurement: Value,
    status: u16,
    raw_body: Option<String>,
    last_api_version: Option<String>,
}

type Shared = Arc<Mutex<Inner>>;

/// A running simulated meter.
pub struct HomeWizardSim {
    state: Shared,
    pki: TestPki,
    server: HttpServer,
}

async fn measurement(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let mut s = st.lock().unwrap();
    s.last_api_version = headers.get("x-api-version").and_then(|v| v.to_str().ok()).map(str::to_string);
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
    if auth != format!("Bearer {}", s.token) {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "user:unauthorized"}))).into_response();
    }
    let status = StatusCode::from_u16(s.status).expect("status");
    let body = s.raw_body.clone().unwrap_or_else(|| s.measurement.to_string());
    (status, [("content-type", "application/json")], body).into_response()
}

async fn device() -> axum::Json<Value> {
    axum::Json(json!({"product_name": "P1 Meter", "product_type": "HWE-P1", "serial": "aabbccddeeff",
        "firmware_version": "6.0503", "api_version": "2.3.0"}))
}

impl HomeWizardSim {
    /// Start a meter that accepts `token`.
    pub async fn start(token: &str) -> HomeWizardSim {
        let state = Arc::new(Mutex::new(Inner {
            token: token.to_string(),
            measurement: serde_json::from_str(MEASUREMENT_FIXTURE).expect("fixture"),
            status: 200,
            raw_body: None,
            last_api_version: None,
        }));
        let router = Router::new()
            .route("/api/measurement", get(measurement))
            .route("/api", get(device))
            .with_state(state.clone());
        let pki = TestPki::new(SIM_SERVER_NAME);
        let server = serve_tls(router, &pki).await;
        HomeWizardSim { state, pki, server }
    }

    /// A client config that reaches and trusts this simulator.
    pub fn config(&self, token: &str) -> HomeWizardConfig {
        HomeWizardConfig {
            addr: self.server.addr,
            token: token.to_string(),
            server_name: SIM_SERVER_NAME.to_string(),
            ca_pem: self.pki.ca_pem.clone(),
            timeout: Duration::from_secs(5),
        }
    }

    /// Replace the measurement object.
    pub fn set_measurement(&self, v: Value) {
        self.state.lock().unwrap().measurement = v;
    }

    /// Answer authorised requests with this status (200 restores normal service).
    pub fn set_status(&self, status: u16) {
        self.state.lock().unwrap().status = status;
    }

    /// Answer authorised requests with this exact body instead of the measurement.
    pub fn set_raw_body(&self, body: Option<String>) {
        self.state.lock().unwrap().raw_body = body;
    }

    /// The `X-Api-Version` header of the last request.
    pub fn last_api_version(&self) -> Option<String> {
        self.state.lock().unwrap().last_api_version.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GET /api` answers like the live meter, so a client can discover the serial.
    #[tokio::test]
    async fn device_info_endpoint() {
        let sim = HomeWizardSim::start("t").await;
        let cfg = sim.config("t");
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .add_root_certificate(reqwest::Certificate::from_pem(cfg.ca_pem.as_bytes()).unwrap())
            .resolve(SIM_SERVER_NAME, cfg.addr)
            .build()
            .unwrap();
        let v: Value = client
            .get(format!("https://{SIM_SERVER_NAME}:{}/api", cfg.addr.port()))
            .header("connection", "close")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(v["serial"], "aabbccddeeff");
        // `Connection: close` lets the server finish the connection cleanly.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
