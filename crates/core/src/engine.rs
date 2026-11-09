//! One control step: world in, derived values and commands out.

use serde::{Deserialize, Serialize};

use crate::automations::{self, Automation, Cx};
use crate::command::Command;
use crate::derived::{self, Derived};
use crate::memory::Memory;
use crate::settings::Settings;
use crate::time::Ts;
use crate::world::World;
use crate::{battery, meters};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TickOutput {
    pub derived: Derived,
    pub commands: Vec<Command>,
}

struct Prev {
    now: Ts,
    world: World,
    derived: Derived,
    settings: Settings,
}

pub struct Engine {
    pub settings: Settings,
    pub memory: Memory,
    automations: Vec<Box<dyn Automation>>,
    prev: Option<Prev>,
}

impl Engine {
    pub fn new(settings: Settings, memory: Memory) -> Self {
        Self::with_automations(settings, memory, automations::all())
    }

    pub fn with_automations(settings: Settings, memory: Memory, automations: Vec<Box<dyn Automation>>) -> Self {
        Engine { settings, memory, automations, prev: None }
    }

    /// The ids of the automations this engine runs, in evaluation order.
    pub fn automation_ids(&self) -> Vec<&'static str> {
        self.automations.iter().map(|a| a.id()).collect()
    }

    /// Run one step at `now` against the latest readings.
    pub fn tick(&mut self, now: Ts, w: &World) -> TickOutput {
        meters::update_sources(now, w, &mut self.memory.meters);
        let (mut d, renders) = derived::compute_with(now, w, &self.settings, &self.memory);
        self.memory.template_renders = renders;
        battery::fill(now, w, &self.settings, &self.memory, &mut d);

        let started = self.prev.is_none();
        let prev = self.prev.take().unwrap_or_else(|| Prev {
            now,
            world: w.clone(),
            derived: d.clone(),
            settings: self.settings.clone(),
        });

        let mut out = Vec::new();
        for a in &mut self.automations {
            let mut cx = Cx {
                now,
                prev_now: prev.now,
                started,
                w,
                pw: &prev.world,
                d: &d,
                pd: &prev.derived,
                s: &mut self.settings,
                ps: &prev.settings,
                m: &mut self.memory,
                out: &mut out,
            };
            a.step(&mut cx);
        }

        meters::update_integrals(now, &d, &mut self.memory.meters);
        self.prev = Some(Prev { now, world: w.clone(), derived: d.clone(), settings: self.settings.clone() });
        TickOutput { derived: d, commands: out }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::at;
    use chrono::Duration;

    /// Records what it saw, to check the engine's plumbing.
    struct Probe {
        seen: std::sync::Arc<std::sync::Mutex<Vec<(bool, i64, bool)>>>,
    }

    impl Automation for Probe {
        fn id(&self) -> &'static str {
            "probe"
        }
        fn step(&mut self, cx: &mut Cx) {
            let dt = (cx.now - cx.prev_now).num_seconds();
            let changed = cx.s.peak_guard_actief != cx.ps.peak_guard_actief;
            self.seen.lock().unwrap().push((cx.started, dt, changed));
            cx.send(Command::RefreshPrices);
            cx.mark("probe");
        }
    }

    #[test]
    fn first_tick_is_start_then_prev_is_carried() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut e = Engine::with_automations(
            Settings::default(),
            Memory::default(),
            vec![Box::new(Probe { seen: seen.clone() })],
        );
        assert_eq!(e.automation_ids(), vec!["probe"]);
        let t = at(2026, 11, 7, 12, 0, 0);
        let w = World::default();
        let out = e.tick(t, &w);
        assert_eq!(out.commands, vec![Command::RefreshPrices]);
        e.settings.peak_guard_actief = false;
        e.tick(t + Duration::seconds(1), &w);
        e.tick(t + Duration::seconds(3), &w);
        assert_eq!(*seen.lock().unwrap(), vec![(true, 0, false), (false, 1, true), (false, 2, false)]);
        assert_eq!(e.memory.last_triggered("probe"), Some(t + Duration::seconds(3)));
    }

    #[test]
    fn the_real_registry_runs() {
        let mut e = Engine::new(Settings::default(), Memory::default());
        assert!(e.automation_ids().len() >= 29);
        let t = at(2026, 11, 7, 12, 0, 0);
        e.tick(t, &World::default());
        e.tick(t + Duration::seconds(1), &World::default());
    }

    /// HA's quarter meter updates on the P1 event itself, before the templates that read it
    /// re-render; the engine updates the P1-fed meters before computing the templates, so the
    /// tick that brings a new import reading already sees it (0.01 kWh), not one tick later.
    #[test]
    fn p1_meters_update_before_the_templates() {
        let mut e = Engine::new(Settings::default(), Memory::default());
        let t = at(2026, 11, 7, 12, 1, 0);
        let mut w = World::default();
        w.p1.import_kwh = Some(100.0);
        assert_eq!(e.tick(t, &w).derived.kwartier_import_kwh, Some(0.0));
        w.p1.import_kwh = Some(100.01);
        let d = e.tick(t + Duration::seconds(1), &w).derived;
        assert!((d.kwartier_import_kwh.unwrap() - 0.01).abs() < 1e-9);
        assert!((d.kwartiervermogen_vastgelegd_kw - 0.04).abs() < 1e-9);
    }
}
