//! Omnik string inverter, `javascript` source type (`omnik_inverter` integration).
//!
//! `GET http://<host>/js/status.js` returns a script with
//! `var webData="<serial>,<fw>,<fw slave>,<model>,<rated W>,<current W>,<today 10 Wh>,<total 100 Wh>,<alarm>,..";`.
//! Field 5 is `sensor.omnik_hoofddak_inverter_omnik_hoofddak_current_power_production` in W.
//! Older firmware uses `myDeviceArray[0]="..."` with the same layout. Parsing follows
//! `omnikinverter.models.Inverter.from_js`: an empty field is `None`, a missing field or a
//! missing `webData` string is an error, and so is a non-integer power. HA polls this every
//! 4 minutes (`scan_interval: 4` in the config entry options).

use std::time::Duration;

use async_trait::async_trait;
use energie_core::command::Command;
use energie_core::world::Omnik as OmnikReading;

use crate::driver::{Actuator, Applied, Reading, Source};
use crate::error::{DriverError, Result};

/// Content types the library accepts (`omnikinverter.OmnikInverter.request`).
const CONTENT_TYPES: [&str; 3] = ["application/json", "application/x-javascript", "text/html"];

/// The comma-separated fields of the status string.
fn status_fields(js: &str) -> Result<Vec<&str>> {
    let marker = if js.contains("webData") { "webData=\"" } else { "myDeviceArray[0]=\"" };
    let start =
        js.find(marker).ok_or_else(|| DriverError::Malformed("no data source in status.js".into()))? + marker.len();
    // `.*?(?=";)`: the shortest run up to `";`, not crossing a newline.
    let rest = &js[start..];
    let end = rest.find("\";").filter(|&e| !rest[..e].contains('\n'));
    let end = end.ok_or_else(|| DriverError::Malformed("unterminated status string".into()))?;
    Ok(rest[..end].split(',').collect())
}

/// Parse `status.js` into the current AC power, W.
pub fn parse_status_js(js: &str) -> Result<Option<f64>> {
    let fields = status_fields(js)?;
    let raw = fields.get(5).ok_or_else(|| DriverError::Malformed("status string too short".into()))?;
    if raw.is_empty() {
        return Ok(None);
    }
    let w: i64 = raw.replace(' ', "").parse().map_err(|_| DriverError::Malformed(format!("power {raw:?}")))?;
    Ok(Some(w as f64))
}

/// Polling client for one Omnik inverter.
pub struct Omnik {
    client: reqwest::Client,
    url: String,
}

impl Omnik {
    /// `base` is `http://192.168.1.40` (no trailing slash).
    pub fn new(base: &str, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        Ok(Omnik { client, url: format!("{base}/js/status.js") })
    }

    /// One `GET /js/status.js`.
    pub async fn power(&self) -> Result<Option<f64>> {
        let resp = self
            .client
            .get(&self.url)
            .header("Accept", "text/html,application/xhtml+xml,application/xml")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(DriverError::Status(status.as_u16()));
        }
        let ct = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        if !CONTENT_TYPES.iter().any(|t| ct.contains(t)) {
            return Err(DriverError::Malformed(format!("content type {ct:?}")));
        }
        let bytes = resp.bytes().await?;
        // `decode("ascii", "ignore")`
        let text: String = bytes.iter().filter(|b| b.is_ascii()).map(|&b| b as char).collect();
        parse_status_js(&text)
    }
}

#[async_trait]
impl Source for Omnik {
    fn name(&self) -> &'static str {
        "omnik"
    }

    async fn read(&mut self) -> Result<Reading> {
        Ok(Reading::Omnik(OmnikReading { power_w: self.power().await? }))
    }

    fn unavailable(&self) -> Reading {
        Reading::Omnik(OmnikReading::default())
    }
}

/// The Omnik is AC-coupled and not controllable.
#[async_trait]
impl Actuator for Omnik {
    async fn apply(&mut self, _cmd: &Command) -> Result<Applied> {
        Ok(Applied::NotMine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::omnik::{OmnikSim, STATUS_FIXTURE};

    /// The captured live `status.js` (2026-11-07, omnik5000tl2): field 5 is 489 W.
    #[test]
    fn real_fixture_current_power() {
        assert_eq!(parse_status_js(STATUS_FIXTURE).unwrap(), Some(489.0));
    }

    /// Older firmware's `myDeviceArray[0]` carries the same fields; spaces inside are dropped.
    #[test]
    fn my_device_array_variant() {
        let js = r#"var myDeviceArray=new Array(); myDeviceArray[0]="SN,a,b,m,3000, 1 234,1,2,,";"#;
        assert_eq!(parse_status_js(js).unwrap(), Some(1234.0));
    }

    /// An empty power field (inverter asleep at dusk) is `None`, not 0.
    #[test]
    fn empty_power_is_none() {
        assert_eq!(parse_status_js(r#"var webData="SN,,,m,5000,,0,1,,";"#).unwrap(), None);
    }

    /// Missing data source, a string without its terminator, a short list and a non-number are
    /// all errors (the library raises; HA marks the sensor unavailable).
    #[test]
    fn malformed_status_is_an_error() {
        for js in [
            "var nothing=1;",
            "var webData=1;",
            "var webData=\"SN,a\nb,c\";",
            r#"var webData="SN,a,b";"#,
            r#"var webData="SN,,,m,5000,abc,0,1,,";"#,
        ] {
            assert!(matches!(parse_status_js(js), Err(DriverError::Malformed(_))), "{js}");
        }
    }

    /// End to end against the simulator: the live script shape, then a changed power.
    #[tokio::test]
    async fn reads_power_from_simulator() {
        let sim = OmnikSim::start().await;
        let mut omnik = Omnik::new(&sim.base_url(), Duration::from_secs(5)).unwrap();
        assert_eq!(omnik.name(), "omnik");
        assert_eq!(omnik.read().await.unwrap(), Reading::Omnik(OmnikReading { power_w: Some(489.0) }));
        sim.set_power(Some(1520));
        assert_eq!(omnik.power().await.unwrap(), Some(1520.0));
        sim.set_power(None);
        assert_eq!(omnik.power().await.unwrap(), None);
        assert_eq!(omnik.unavailable(), Reading::Omnik(OmnikReading::default()));
        assert_eq!(omnik.apply(&Command::RefreshPrices).await.unwrap(), Applied::NotMine);
    }

    /// HTTP errors and unexpected content types fail the read.
    #[tokio::test]
    async fn http_error_and_wrong_content_type() {
        let sim = OmnikSim::start().await;
        let omnik = Omnik::new(&sim.base_url(), Duration::from_secs(5)).unwrap();
        sim.set_status(500);
        assert!(matches!(omnik.power().await, Err(DriverError::Status(500))));
        sim.set_status(200);
        sim.set_content_type("image/png");
        assert!(matches!(omnik.power().await, Err(DriverError::Malformed(_))));
    }

    /// Non-ASCII bytes are dropped before parsing, like `decode("ascii", "ignore")`.
    #[tokio::test]
    async fn non_ascii_is_ignored() {
        let sim = OmnikSim::start().await;
        sim.set_body("var webData=\"SN,,,m\u{e9},5000,77,0,1,,\";".to_string());
        let omnik = Omnik::new(&sim.base_url(), Duration::from_secs(5)).unwrap();
        assert_eq!(omnik.power().await.unwrap(), Some(77.0));
    }

    /// A dead inverter (nothing listening) is a transport error.
    #[tokio::test]
    async fn unreachable_inverter() {
        let omnik = Omnik::new("http://127.0.0.1:9", Duration::from_secs(2)).unwrap();
        assert!(matches!(omnik.power().await, Err(DriverError::Http(_))));
    }
}
