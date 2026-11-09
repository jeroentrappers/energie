//! Omnik Wi-Fi module: `GET /js/status.js`, served as `application/x-javascript` like the live
//! Ralink HTTPD.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;

use super::http::{HttpServer, serve};

/// The live `status.js`, captured 2026-11-07.
pub const STATUS_FIXTURE: &str = include_str!("../../fixtures/omnik_status.js");

struct Inner {
    body: String,
    status: u16,
    content_type: String,
}

type Shared = Arc<Mutex<Inner>>;

/// A running simulated Omnik.
pub struct OmnikSim {
    state: Shared,
    server: HttpServer,
}

async fn status_js(State(st): State<Shared>) -> impl IntoResponse {
    let s = st.lock().unwrap();
    (StatusCode::from_u16(s.status).expect("status"), [("content-type", s.content_type.clone())], s.body.clone())
}

impl OmnikSim {
    pub async fn start() -> OmnikSim {
        let state = Arc::new(Mutex::new(Inner {
            body: STATUS_FIXTURE.to_string(),
            status: 200,
            content_type: "application/x-javascript".into(),
        }));
        let server = serve(Router::new().route("/js/status.js", get(status_js)).with_state(state.clone())).await;
        OmnikSim { state, server }
    }

    /// `http://127.0.0.1:<port>`
    pub fn base_url(&self) -> String {
        format!("http://{}", self.server.addr)
    }

    /// Set the current-power field of `webData` (`None` empties it, as at dusk).
    pub fn set_power(&self, w: Option<i64>) {
        let mut s = self.state.lock().unwrap();
        let start = s.body.find("webData=\"").expect("webData") + 9;
        let end = start + s.body[start..].find("\";").expect("end");
        let mut fields: Vec<String> = s.body[start..end].split(',').map(str::to_string).collect();
        fields[5] = w.map(|w| w.to_string()).unwrap_or_default();
        s.body.replace_range(start..end, &fields.join(","));
    }

    /// Serve this exact body.
    pub fn set_body(&self, body: String) {
        self.state.lock().unwrap().body = body;
    }

    pub fn set_status(&self, status: u16) {
        self.state.lock().unwrap().status = status;
    }

    pub fn set_content_type(&self, ct: &str) {
        self.state.lock().unwrap().content_type = ct.to_string();
    }
}
