//! Port of HA automation `batterij_naar_auto_beheer` ("Batterij-dump: kWh-budget beheer"),
//! LOGIC.md "EV charging — two orthogonal levels" (battery level `Ontlaad-budget`).
//!
//! On *choosing* `input_select.ev_batterij_niveau = Ontlaad-budget` it snapshots the discharge
//! counter (`sensor.batterij_ontladen_energie` into `input_number.batterij_naar_auto_start_kwh`).
//! Once as many kWh as `input_number.batterij_naar_auto_budget` are delivered — or the SoC reaches
//! the reserve floor — it sets the battery level back to `Uit` ([`Settings::ev_batterij_niveau`])
//! and notifies. The grid level and the car are left alone.
//!
//! Triggers: the select to `Ontlaad-budget` (`enter`); state changes of the discharge counter,
//! the SoC, the budget and the reserve SoC, and `time_pattern minutes: /1` (`check`).
//! When both fire in one tick, `enter` runs first and `check` after it (HA would queue them as two
//! separate runs of instant actions).
//!
//! [`Settings::ev_batterij_niveau`]: crate::settings::Settings
use super::{Automation, Cx};
use crate::command::Command;
use crate::jinja::{py_float, round};
use crate::settings::BatterijNiveau;
use crate::time;

#[derive(Debug, Default)]
pub struct BatterijNaarAuto;

impl Automation for BatterijNaarAuto {
    fn id(&self) -> &'static str {
        "batterij_naar_auto_beheer"
    }

    fn step(&mut self, cx: &mut Cx) {
        let enter = cx.s.ev_batterij_niveau == BatterijNiveau::OntlaadBudget
            && cx.ps.ev_batterij_niveau != BatterijNiveau::OntlaadBudget;
        let check = cx.every(60)
            || cx.d.batterij_ontladen_energie_kwh != cx.pd.batterij_ontladen_energie_kwh
            || cx.w.solis.soc != cx.pw.solis.soc
            || cx.s.batterij_naar_auto_budget != cx.ps.batterij_naar_auto_budget
            || cx.s.batterij_reserve_soc != cx.ps.batterij_reserve_soc;
        if !(enter || check) {
            return;
        }
        cx.mark(self.id());
        if enter {
            cx.m.batterij_naar_auto_start_kwh = cx.d.batterij_ontladen_energie_kwh.unwrap_or(0.0);
        }
        if !check {
            return;
        }
        let geleverd = cx.d.batterij_ontladen_energie_kwh.unwrap_or(0.0) - cx.m.batterij_naar_auto_start_kwh;
        let budget = cx.s.batterij_naar_auto_budget;
        let soc = cx.w.solis.soc.unwrap_or(100.0);
        let vloer = if time::is_ochtend(cx.now) { 15.0 } else { cx.s.batterij_reserve_soc };
        let meetbaar = cx.d.batterij_ontladen_energie_kwh.is_some();
        if cx.s.ev_batterij_niveau == BatterijNiveau::OntlaadBudget
            && ((meetbaar && geleverd >= budget) || soc <= vloer)
        {
            cx.s.ev_batterij_niveau = BatterijNiveau::Uit;
            let why = if soc <= vloer {
                format!("Reservevloer bereikt (SoC {}%). ", py_float(round(soc, 0)))
            } else {
                format!("Budget geleverd: {} van {} kWh. ", py_float(round(geleverd, 1)), py_float(round(budget, 1)))
            };
            cx.send(Command::notify("Batterij-dump klaar", format!("{why}Batterij-niveau terug op Uit.")));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;
    use crate::time::at;

    /// 14:00 (no morning floor), floor 15 %, budget 5 kWh, SoC 60 %, counter at 549.546 kWh.
    fn rig() -> Rig {
        let mut r = Rig::new();
        r.now = at(2026, 11, 7, 14, 0, 10);
        r.s.batterij_reserve_soc = 15.0;
        r.s.batterij_naar_auto_budget = 5.0;
        r.s.ev_batterij_niveau = BatterijNiveau::Uit;
        r.w.solis.soc = Some(60.0);
        r.d.batterij_ontladen_energie_kwh = Some(549.546);
        r
    }

    fn done(message: &str) -> Vec<Command> {
        vec![Command::notify("Batterij-dump klaar", message)]
    }

    /// Entering snapshots the counter; 5 kWh later the level goes back to Uit.
    #[test]
    fn budget_delivered_sets_the_level_back_to_uit() {
        let mut r = rig();
        let mut a = BatterijNaarAuto;
        r.run(&mut a);
        r.wait(5);
        r.s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        assert!(r.run(&mut a).is_empty());
        assert_eq!(r.m.batterij_naar_auto_start_kwh, 549.546);
        r.wait(5);
        r.d.batterij_ontladen_energie_kwh = Some(554.5);
        assert!(r.run(&mut a).is_empty(), "4.954 < 5");
        r.wait(5);
        r.d.batterij_ontladen_energie_kwh = Some(554.6);
        let out = r.run(&mut a);
        assert_eq!(r.s.ev_batterij_niveau, BatterijNiveau::Uit);
        assert_eq!(out, done("Budget geleverd: 5.1 van 5.0 kWh. Batterij-niveau terug op Uit."));
    }

    /// The SoC reaching the floor ends the dump too (and says so).
    #[test]
    fn the_reserve_floor_ends_the_dump() {
        let mut r = rig();
        let mut a = BatterijNaarAuto;
        r.s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        r.m.batterij_naar_auto_start_kwh = 549.546;
        r.run(&mut a);
        r.wait(5);
        r.w.solis.soc = Some(15.0);
        let out = r.run(&mut a);
        assert_eq!(r.s.ev_batterij_niveau, BatterijNiveau::Uit);
        assert_eq!(out, done("Reservevloer bereikt (SoC 15.0%). Batterij-niveau terug op Uit."));
    }

    /// In the 06–10 h morning window the floor is 15 % whatever the setting.
    #[test]
    fn the_morning_floor_is_15() {
        let mut r = rig();
        r.now = at(2026, 11, 7, 7, 0, 10);
        r.s.batterij_reserve_soc = 40.0;
        r.s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        r.m.batterij_naar_auto_start_kwh = 549.546;
        r.w.solis.soc = Some(30.0);
        let mut a = BatterijNaarAuto;
        r.run(&mut a);
        r.wait(50); // 07:01:00
        assert!(r.run(&mut a).is_empty(), "30 % is above the morning floor");
        r.now = at(2026, 11, 7, 10, 0, 10);
        r.wait(50); // 10:01:00: the 40 % floor applies
        let out = r.run(&mut a);
        assert_eq!(out, done("Reservevloer bereikt (SoC 30.0%). Batterij-niveau terug op Uit."));
    }

    /// Without the counter (`meetbaar` false) the budget never ends the dump, the floor still
    /// does; an unknown SoC reads 100.
    #[test]
    fn an_unmeasurable_counter_only_ends_on_the_floor() {
        let mut r = rig();
        let mut a = BatterijNaarAuto;
        r.s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        r.m.batterij_naar_auto_start_kwh = -10.0; // geleverd would be 10 ≥ 5
        r.d.batterij_ontladen_energie_kwh = None;
        r.run(&mut a);
        r.wait(50);
        r.w.solis.soc = None;
        assert!(r.run(&mut a).is_empty());
        assert_eq!(r.s.ev_batterij_niveau, BatterijNiveau::OntlaadBudget);
    }

    /// Not in Ontlaad-budget: a check does nothing, even past the budget.
    #[test]
    fn only_acts_in_ontlaad_budget() {
        let mut r = rig();
        let mut a = BatterijNaarAuto;
        r.run(&mut a);
        r.wait(50);
        r.w.solis.soc = Some(5.0);
        assert!(r.run(&mut a).is_empty());
        assert_eq!(r.m.last_triggered("batterij_naar_auto_beheer"), Some(r.now), "no conditions: it ran");
    }

    /// Each check trigger runs it; nothing between the minute marks otherwise.
    #[test]
    fn check_triggers() {
        for change in 0..5 {
            let mut r = rig();
            let mut a = BatterijNaarAuto;
            r.run(&mut a);
            r.wait(5);
            match change {
                0 => r.d.batterij_ontladen_energie_kwh = Some(550.0),
                1 => r.w.solis.soc = Some(59.0),
                2 => r.s.batterij_naar_auto_budget = 6.0,
                3 => r.s.batterij_reserve_soc = 20.0,
                _ => {}
            }
            r.run(&mut a);
            let ran = r.m.last_triggered("batterij_naar_auto_beheer") == Some(r.now);
            assert_eq!(ran, change < 4, "change {change}");
        }
    }

    /// Entering at or below the floor: snapshot, then the same tick's check ends it.
    #[test]
    fn enter_and_check_in_one_tick() {
        let mut r = rig();
        let mut a = BatterijNaarAuto;
        r.run(&mut a);
        r.wait(50); // 14:01:00, the minute pattern
        r.s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        r.w.solis.soc = Some(10.0);
        let out = r.run(&mut a);
        assert_eq!(r.m.batterij_naar_auto_start_kwh, 549.546);
        assert_eq!(out, done("Reservevloer bereikt (SoC 10.0%). Batterij-niveau terug op Uit."));
    }

    #[test]
    fn python_float_formatting() {
        assert_eq!(py_float(40.0), "40.0");
        assert_eq!(py_float(5.25), "5.25");
    }
}
