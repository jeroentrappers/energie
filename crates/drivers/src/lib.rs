//! Device drivers and protocol-level simulators.
//!
//! Each driver turns one device's wire protocol into a [`Reading`] (a `World` sub-struct) and
//! carries out the [`energie_core::command::Command`]s meant for it. See `README.md` for the
//! protocols, register/DP/topic maps and where each fact comes from.
//!
//! | Module | Device | Transport |
//! | --- | --- | --- |
//! | [`homewizard`] | HomeWizard P1 meter | HTTPS, local API v2 |
//! | [`solis`] | Solis S6-EH1P hybrid inverter | Modbus TCP via a Waveshare RS485 gateway |
//! | [`smartevse`] | SmartEVSE v3 (Buzz) | MQTT |
//! | [`tuya`] | Kolanky EV cable (EV3) | Tuya local protocol 3.4 |
//! | [`omnik`] | Omnik string inverter | HTTP, `js/status.js` |
//! | [`prices`] | Day-ahead prices | energy-charts JSON, ENTSO-E XML fallback |
//! | [`http_json`] | any JSON endpoint with a power value | HTTP |
//! | [`fixed`] | `static`: fixed values from the config (stand-in, tests) | none |
//!
//! [`plugin`] is the contract a driver kind implements to be configurable as a `[[device]]`
//! (roles, slots, [`plugin::DriverKind`]); [`kinds`] holds the kinds of the drivers above;
//! [`entity`] is the generic entity model. [`sim`] holds a simulator per protocol that speaks
//! the real wire format on `127.0.0.1:0`.

pub mod auth;
pub mod driver;
pub mod entity;
pub mod error;
pub mod fixed;
pub mod homewizard;
pub mod http_json;
pub mod kinds;
pub mod omnik;
pub mod plugin;
pub mod prices;
pub mod sim;
pub mod smartevse;
pub mod solis;
pub mod tuya;

pub use auth::AuthStatus;
pub use driver::{Actuator, Applied, Outcome, Reading, Source, Write, read_or_unavailable};
pub use error::{DriverError, Result};
