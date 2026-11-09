//! Native clients for the cloud integrations that today only reach the stack through Home
//! Assistant, so the binary can run without HA.
//!
//! Each client reads one service into a typed reading (a `World` sub-struct plus what the
//! dashboard shows), lists the HA entities it provides, and carries out the commands meant for
//! it. Polling limits and backoff are built in: [`CloudSource::read`] answers from the cache
//! inside the poll interval and returns [`CloudError::Backoff`] while backing off after a
//! failure, so a caller can call it every engine tick. See `README.md` for the protocols and
//! where each fact comes from.
//!
//! | Module | Service | Entities | Writes |
//! | --- | --- | --- | --- |
//! | [`kia`] | Kia Connect EU, cached vehicle state | `sensor.ev3_*`, `binary_sensor.ev3_*` | `number.ev3_ac_charging_limit` |
//! | [`cupra`] | VW Group EU Data Act portal (ID.Buzz) | `sensor.de_buzz_*` | - |
//! | [`lgthinq`] | LG ThinQ Connect (PAT) | `switch.airco_*_power` | both switches |
//! | [`fluvius`] | Mijn Fluvius, capacity peak | `sensor.fluvius_meter_*_peak_power` | - |
//! | [`forecast_solar`] | forecast.solar public API | `sensor.power_production_now`, `sensor.energy_*` | - |
//!
//! Credentials are the HA config entries' `data` as JSON files (read-only); tokens a service
//! rotates go to state files ([`store::StateFile`], never under `/etc`). The Kia and VW clients
//! log in with the password by themselves, through [`login::LoginGate`], and take new
//! credentials at run time ([`login::CredentialStore`], `energie credentials set`).
//!
//! [`kinds`] makes each client a driver kind of `energie-drivers`' plug-in contract (`kia_connect`,
//! `cupra_data_act`, `lg_thinq`, `fluvius`, `forecast_solar`), registered by the app.

pub mod cupra;
pub mod error;
pub mod fluvius;
pub mod forecast_solar;
pub mod html;
pub mod http;
pub mod kia;
pub mod kinds;
pub mod lgthinq;
pub mod login;
pub mod poll;
pub mod source;
pub mod store;
pub mod zip;

pub use error::{CloudError, Result};
pub use kinds::kinds;
pub use source::{
    Applied, CloudActuator, CloudCommand, CloudReading, CloudSource, EntityDescriptor, EntityKind, EntityState,
    EntityValue, read_or_unavailable,
};
