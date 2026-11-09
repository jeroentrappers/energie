//! `kind = "http_json"`: a power value from any HTTP endpoint that answers JSON (a Shelly, an
//! ESPHome web server, a second inverter's local API). The smallest complete driver, and the
//! example `docs/PLUGINS.md` walks through.
//!
//! ```toml
//! [[device]]
//! id = "pv_garage"
//! kind = "http_json"
//! role = "pv"
//! url = "http://192.168.1.70/status"
//! pointer = "/meters/0/power"   # RFC 6901 JSON pointer to a number
//! scale = 1.0                   # multiply into W
//! ```
//!
//! As `pv` the value is `Omnik.power_w` and is summed with the other PV instances; as
//! `grid_meter` it is `P1.power_w` (+ import / − export). A missing or non-numeric value at the
//! pointer is a failed read (the instance goes unavailable after its grace polls).

use std::time::Duration;

use async_trait::async_trait;
use energie_core::command::Command;
use energie_core::world::{Omnik, P1};
use serde::Deserialize;

use crate::driver::{Actuator, Applied, Reading, Source};
use crate::entity::{Entity, Value};
use crate::error::{DriverError, Result};
use crate::plugin::{BuildCx, Driver, DriverKind, Slot};

pub struct HttpJsonKind;

fn unit_scale() -> f64 {
    1.0
}
fn timeout_ms() -> u64 {
    5000
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpJsonTable {
    pub url: String,
    /// JSON pointer to the value, `/power`.
    pub pointer: String,
    /// Factor into W (`1000.0` for a kW value, `-1.0` to flip the sign).
    #[serde(default = "unit_scale")]
    pub scale: f64,
    #[serde(default = "timeout_ms")]
    pub timeout_ms: u64,
}

/// The polling client.
pub struct HttpJson {
    client: reqwest::Client,
    table: HttpJsonTable,
    slot: Slot,
    last_raw: Option<f64>,
}

impl HttpJson {
    /// One GET: the scaled value at the pointer, W.
    pub async fn power(&mut self) -> Result<f64> {
        let resp = self.client.get(&self.table.url).send().await?;
        if !resp.status().is_success() {
            return Err(DriverError::Status(resp.status().as_u16()));
        }
        let body: serde_json::Value = serde_json::from_slice(&resp.bytes().await?)?;
        let raw = body
            .pointer(&self.table.pointer)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| DriverError::Malformed(format!("no number at {}", self.table.pointer)))?;
        self.last_raw = Some(raw);
        Ok(raw * self.table.scale)
    }
}

#[async_trait]
impl Source for HttpJson {
    fn name(&self) -> &'static str {
        "http_json"
    }

    async fn read(&mut self) -> Result<Reading> {
        let w = Some(self.power().await?);
        Ok(match self.slot {
            Slot::GridMeter => Reading::P1(P1 { power_w: w, ..P1::default() }),
            _ => Reading::Omnik(Omnik { power_w: w }),
        })
    }

    fn unavailable(&self) -> Reading {
        self.slot.unavailable()
    }

    /// The unscaled value, as the endpoint sent it.
    fn entities(&self) -> Vec<Entity> {
        vec![Entity::sensor("raw", "Ruwe waarde", Value::from(self.last_raw))]
    }
}

/// Read only.
#[async_trait]
impl Actuator for HttpJson {
    async fn apply(&mut self, _cmd: &Command) -> Result<Applied> {
        Ok(Applied::NotMine)
    }
}

impl DriverKind for HttpJsonKind {
    type Config = HttpJsonTable;
    const NAME: &'static str = "http_json";
    const SLOTS: &'static [Slot] = &[Slot::Pv, Slot::GridMeter];
    const POLL_MS: u64 = 5000;

    fn check(t: &HttpJsonTable) -> Result<()> {
        if !t.pointer.is_empty() && !t.pointer.starts_with('/') {
            return Err(DriverError::Config(format!("pointer {:?} must start with /", t.pointer)));
        }
        Ok(())
    }

    fn build(table: HttpJsonTable, cx: &BuildCx) -> Result<Box<dyn Driver>> {
        let client = reqwest::Client::builder().timeout(Duration::from_millis(table.timeout_ms)).build()?;
        let slot = cx.slots.first().copied().unwrap_or(Slot::Pv);
        Ok(Box::new(HttpJson { client, table, slot, last_raw: None }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::factory;
    use crate::sim::http::serve;
    use axum::Router;
    use axum::routing::get;
    use serde_json::json;

    async fn endpoint() -> crate::sim::http::HttpServer {
        let app = Router::new()
            .route("/status", get(|| async { axum::Json(json!({"meters": [{"power": 1.25}], "name": "x"})) }))
            .route("/broken", get(|| async { (axum::http::StatusCode::BAD_GATEWAY, "nee") }));
        serve(app).await
    }

    /// The value at the pointer, scaled, fills `Omnik.power_w` as PV and `P1.power_w` as grid
    /// meter; the raw value is an entity.
    #[tokio::test]
    async fn reads_the_pointer_into_its_slot() {
        let srv = endpoint().await;
        let f = factory::<HttpJsonKind>();
        let t = json!({"url": format!("http://{}/status", srv.addr), "pointer": "/meters/0/power", "scale": 1000.0});
        let mut pv = f.build(&t, &BuildCx { id: "pv2", slots: &[Slot::Pv], state_dir: None }).unwrap();
        assert_eq!(pv.name(), "http_json");
        assert_eq!(pv.entities()[0].value, Value::Null, "nothing read yet");
        assert_eq!(pv.read().await.unwrap(), Reading::Omnik(Omnik { power_w: Some(1250.0) }));
        assert_eq!(pv.entities()[0].value, Value::Number(1.25));
        assert_eq!(pv.unavailable(), Slot::Pv.unavailable());
        assert_eq!(pv.apply(&Command::RefreshPrices).await.unwrap(), Applied::NotMine);
        let mut grid = f.build(&t, &BuildCx { id: "g", slots: &[Slot::GridMeter], state_dir: None }).unwrap();
        assert_eq!(grid.read().await.unwrap(), Reading::P1(P1 { power_w: Some(1250.0), ..P1::default() }));
        let mut unbound = f.build(&t, &BuildCx { id: "u", slots: &[], state_dir: None }).unwrap();
        assert!(matches!(unbound.read().await.unwrap(), Reading::Omnik(_)), "defaults to pv");
    }

    /// A non-number at the pointer, an error status, and a body that is not JSON fail the read;
    /// a pointer must start with `/`.
    #[tokio::test]
    async fn bad_answers_fail_the_read() {
        let srv = endpoint().await;
        let f = factory::<HttpJsonKind>();
        let cx = BuildCx { id: "x", slots: &[Slot::Pv], state_dir: None };
        let mk = |path: &str, ptr: &str| json!({"url": format!("http://{}{path}", srv.addr), "pointer": ptr});
        let mut text = f.build(&mk("/status", "/name"), &cx).unwrap();
        assert!(text.read().await.unwrap_err().to_string().contains("no number at /name"));
        let mut status = f.build(&mk("/broken", "/x"), &cx).unwrap();
        assert!(matches!(status.read().await, Err(DriverError::Status(502))));
        let srv2 = serve(Router::new().route("/", get(|| async { "geen json" }))).await;
        let t = json!({"url": format!("http://{}/", srv2.addr), "pointer": ""});
        assert!(matches!(f.build(&t, &cx).unwrap().read().await, Err(DriverError::Malformed(_))));
        assert!(
            f.check(&json!({"url": "u", "pointer": "power"})).unwrap_err().to_string().contains("must start with /")
        );
    }
}
