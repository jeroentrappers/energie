//! In-process simulators that speak each device's real protocol on `127.0.0.1:0`.
//!
//! They share mutable device state with the test (or the app's demo mode), so a test can change
//! what the "device" reports and inspect what the driver wrote. None of them talks to real
//! hardware.

pub mod homewizard;
pub mod http;
pub mod mqtt;
pub mod omnik;
pub mod prices;
pub mod proxy;
pub mod solis;
pub mod tuya;
