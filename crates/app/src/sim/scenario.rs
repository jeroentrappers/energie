//! Fast-forward scenarios: the house model and the engine stepping on a virtual clock, with
//! no sleeps. A day runs in well under a second, so behaviour over hours can be tested.

use chrono::Duration;
use energie_core::command::Command;
use energie_core::derived::Derived;
use energie_core::engine::Engine;
use energie_core::memory::Memory;
use energie_core::settings::Settings;
use energie_core::time::Ts;
use energie_core::world::World;

use super::House;
use crate::config::SimConfig;

/// One step's outcome.
#[derive(Debug, Clone)]
pub struct Frame {
    pub t: Ts,
    pub world: World,
    pub derived: Derived,
    pub commands: Vec<Command>,
}

pub struct Scenario {
    pub house: House,
    pub engine: Engine,
    pub t: Ts,
    pub step: Duration,
}

impl Scenario {
    pub fn new(cfg: SimConfig, start: Ts, settings: Settings, memory: Memory) -> Self {
        Scenario {
            house: House::new(cfg, start),
            engine: Engine::new(settings, memory),
            t: start,
            step: Duration::seconds(1),
        }
    }

    /// Advance one step: physics to `t`, read, decide, carry out the commands.
    pub fn step(&mut self) -> Frame {
        self.t += self.step;
        self.house.step(self.t);
        let world = self.house.world();
        let out = self.engine.tick(self.t, &world);
        for c in &out.commands {
            self.house.apply(self.t, c);
        }
        Frame { t: self.t, world, derived: out.derived, commands: out.commands }
    }

    /// Run for `d` of virtual time, calling `each` on every frame.
    pub fn run_for(&mut self, d: Duration, mut each: impl FnMut(&Frame)) {
        let end = self.t + d;
        while self.t < end {
            let f = self.step();
            each(&f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use energie_core::settings::NetNiveau;
    use energie_core::time::TZ;

    /// A whole June day runs closed-loop on the virtual clock: the sun comes up and goes down,
    /// the battery is fuller in the late afternoon than at dawn, and the meters only move
    /// forward. Grid level `Uit`, so only the sun can fill the battery (with the live default
    /// `Max tot piek` and a cheap night it grid-charges to full before dawn, as HA does).
    #[test]
    fn a_june_day_fast_forwards() {
        let start = TZ.with_ymd_and_hms(2026, 6, 21, 0, 0, 0).unwrap();
        let cfg = SimConfig { buzz_plugged: false, ev3_plugged: false, battery_soc: 30.0, ..Default::default() };
        let settings = Settings { ev_net_niveau: NetNiveau::Uit, ..Settings::default() };
        let mut s = Scenario::new(cfg, start, settings, Memory::default());
        s.step = Duration::seconds(5);
        let mut max_pv = 0.0f64;
        let mut last_import = 0.0;
        let mut soc_at_dawn = None;
        let mut soc_afternoon = None;
        let mut frames = 0;
        s.run_for(Duration::hours(24), |f| {
            frames += 1;
            let pv = f.world.solis.pv_power_w.unwrap() + f.world.omnik.power_w.unwrap();
            max_pv = max_pv.max(pv);
            let imp = f.world.p1.import_kwh.unwrap();
            assert!(imp >= last_import);
            last_import = imp;
            if f.t == start + Duration::hours(5) {
                soc_at_dawn = f.world.solis.soc;
            }
            if f.t == start + Duration::hours(17) {
                soc_afternoon = f.world.solis.soc;
            }
        });
        assert_eq!(frames, 24 * 3600 / 5);
        assert!(max_pv > 4000.0, "{max_pv}");
        assert!(soc_afternoon.unwrap() > soc_at_dawn.unwrap(), "{soc_afternoon:?} vs {soc_at_dawn:?}");
        assert_eq!(s.t, start + Duration::hours(24));
    }

    /// Commands from the engine reach the house in the same step.
    #[test]
    fn engine_commands_reach_the_house() {
        use energie_core::automations::{Automation, Cx};
        struct SetOverride;
        impl Automation for SetOverride {
            fn id(&self) -> &'static str {
                "test_override"
            }
            fn step(&mut self, cx: &mut Cx) {
                cx.send(Command::EvseCurrentDeciA(120));
            }
        }
        let start = TZ.with_ymd_and_hms(2026, 11, 7, 2, 0, 0).unwrap();
        let mut s = Scenario::new(SimConfig::default(), start, Settings::default(), Memory::default());
        s.engine = Engine::with_automations(Settings::default(), Memory::default(), vec![Box::new(SetOverride)]);
        assert_eq!(s.engine.automation_ids(), vec!["test_override"]);
        let f = s.step();
        assert_eq!(f.commands, vec![Command::EvseCurrentDeciA(120)]);
        assert_eq!(s.house.override_a, 12.0);
        s.run_for(Duration::seconds(30), |_| {});
        assert_eq!(s.house.evse_a, 12.0);
    }
}
