//! The `energie` application: runtime loop, Home Assistant bridge, shadow comparison,
//! dashboard, notifications, the simulated house and the history replay. `main.rs` only calls [`cli::main`].

pub mod app;
pub mod bridge;
pub mod cli;
pub mod config;
pub mod control;
pub mod credentials;
pub mod drivers;
pub mod entities;
pub mod error;
pub mod ha;
pub mod history;
pub mod import;
pub mod legacy;
pub mod mqtt;
pub mod notify;
pub mod persist;
pub mod plan;
pub mod plug;
pub mod registry;
pub mod replay;
pub mod runtime;
pub mod shadow;
pub mod sim;
pub mod web;

/// A path as text inside TOML in a test: a Windows backslash would start an escape.
#[cfg(test)]
pub(crate) fn toml_path(p: impl AsRef<std::path::Path>) -> String {
    p.as_ref().display().to_string().replace('\\', "/")
}
