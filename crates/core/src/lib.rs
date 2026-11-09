//! Pure control logic of the home energy stack, ported from the live Home Assistant config.
//!
//! No I/O: drivers produce a [`world::World`], the [`engine::Engine`] turns it into
//! [`derived::Derived`] values and [`command::Command`]s, and drivers carry those out.

pub mod automations;
pub mod battery;
pub mod cascade;
pub mod command;
pub mod derived;
pub mod engine;
pub mod jinja;
pub mod memory;
pub mod meters;
pub mod render;
pub mod settings;
pub mod sun;
pub mod time;
pub mod trigger;
pub mod world;
