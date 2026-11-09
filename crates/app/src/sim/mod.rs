//! The simulated house: the physics model ([`house`]), the `sim` source/actuator around it,
//! and fast-forward scenarios ([`scenario`]) for tests.

pub mod house;
pub mod scenario;

use std::sync::{Arc, Mutex};

use energie_core::command::Command;
use energie_core::time::Ts;
use energie_core::world::World;

use crate::error::Result;
use crate::plug::{Actuator, WorldSource};
pub use house::House;

/// The house model as both a source (it advances to `now` on each read) and an actuator.
#[derive(Clone)]
pub struct SimHandle(pub Arc<Mutex<House>>);

impl SimHandle {
    pub fn new(house: House) -> Self {
        SimHandle(Arc::new(Mutex::new(house)))
    }
}

impl WorldSource for SimHandle {
    fn name(&self) -> &str {
        "sim"
    }
    fn read(&self, now: Ts) -> World {
        let mut h = self.0.lock().expect("house lock");
        h.step(now);
        h.world()
    }
}

impl Actuator for SimHandle {
    fn name(&self) -> &str {
        "sim"
    }
    fn submit(&self, now: Ts, cmd: &Command) -> Result<()> {
        self.0.lock().expect("house lock").apply(now, cmd);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SimConfig;
    use crate::plug::tests::t0;
    use chrono::Duration;

    /// Reading advances the model; submitting a command changes it.
    #[test]
    fn handle_is_a_source_and_an_actuator() {
        let h = SimHandle::new(House::new(SimConfig::default(), t0()));
        assert_eq!(WorldSource::name(&h), "sim");
        assert_eq!(Actuator::name(&h), "sim");
        h.submit(t0(), &Command::EvseCurrentDeciA(100)).unwrap();
        let w = h.read(t0() + Duration::seconds(30));
        assert_eq!(w.evse.override_a, Some(10.0));
        assert_eq!(h.0.lock().unwrap().t, t0() + Duration::seconds(30));
    }
}
