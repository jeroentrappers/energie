//! Port of HA automation `dagbalans_rapport`.
//!
//! At 23:55 a push message sums up the day from the daily utility meters. The numbers are
//! automation `variables:`, which HA renders to native types, so `round(1)` values print as
//! Python floats (`20.0`, `3.1`) and `|int(0)` values as integers.
//!
//! This module also holds [`py_float`], the one place that knows how HA prints a float, used by
//! the other notification texts of the safety and reporting automations.

use super::{Automation, Cx};
use crate::command::Command;
use crate::jinja;

#[derive(Debug, Default)]
pub struct DagbalansRapport;

impl Automation for DagbalansRapport {
    fn id(&self) -> &'static str {
        "dagbalans_rapport"
    }

    fn step(&mut self, cx: &mut Cx) {
        if !cx.daily(23, 55, 0) {
            return;
        }
        let r1 = |v: Option<f64>| jinja::py_float(jinja::round(v.unwrap_or(0.0), 1));
        let pv = r1(cx.d.pv_energie_dag_kwh);
        let verbruik = r1(cx.d.huisverbruik_dag_kwh);
        let imp = r1(cx.d.netimport_dag_kwh);
        let inj = r1(cx.d.injectie_dag_kwh);
        // `states(..)|int(0)`: the sensors render whole percentages; `|int` truncates anyway.
        let zv = jinja::int(cx.d.zelfverbruik_vandaag_pct);
        let zvz = jinja::int(cx.d.zelfvoorziening_vandaag_pct);
        // The YAML single-quoted scalar folds its blank line into one `\n`.
        let message = format!(
            "Verbruik {verbruik} kWh · PV {pv} kWh · injectie {inj} kWh · net {imp} kWh\n\
             Zelfverbruik {zv}% · zelfvoorziening {zvz}%"
        );
        cx.mark(self.id());
        cx.send(Command::notify("⚡ Dagbalans", message));
    }
}

/// A hand-cranked [`Cx`] for the automation tests of this group: two snapshots of everything
/// (previous and current tick), stepped explicitly.
#[cfg(test)]
pub(crate) mod rig {
    use chrono::Duration;

    use crate::automations::{Automation, Cx};
    use crate::command::Command;
    use crate::derived::Derived;
    use crate::memory::Memory;
    use crate::settings::Settings;
    use crate::time::Ts;
    use crate::world::World;

    pub struct Rig {
        pub now: Ts,
        pub prev_now: Ts,
        pub started: bool,
        pub w: World,
        pub pw: World,
        pub d: Derived,
        pub pd: Derived,
        pub s: Settings,
        pub ps: Settings,
        pub m: Memory,
    }

    impl Rig {
        /// A rig at `now`, not the start tick, whose previous tick was one second earlier.
        pub fn new(now: Ts) -> Self {
            Rig {
                now,
                prev_now: now - Duration::seconds(1),
                started: false,
                w: World::default(),
                pw: World::default(),
                d: Derived::default(),
                pd: Derived::default(),
                s: Settings::default(),
                ps: Settings::default(),
                m: Memory::default(),
            }
        }

        /// Run `a` once on the current snapshots and return what it sent.
        pub fn step(&mut self, a: &mut dyn Automation) -> Vec<Command> {
            self.with_cx(|cx| a.step(cx)).1
        }

        /// Call `f` with a [`Cx`] over the current snapshots; returns its result and what was
        /// sent. For testing an automation's helpers directly.
        pub fn with_cx<R>(&mut self, f: impl FnOnce(&mut Cx) -> R) -> (R, Vec<Command>) {
            let mut out = Vec::new();
            let mut cx = Cx {
                now: self.now,
                prev_now: self.prev_now,
                started: self.started,
                w: &self.w,
                pw: &self.pw,
                d: &self.d,
                pd: &self.pd,
                s: &mut self.s,
                ps: &self.ps,
                m: &mut self.m,
                out: &mut out,
            };
            let r = f(&mut cx);
            (r, out)
        }

        /// Move time on by `secs`, carrying the current snapshots into the previous ones.
        pub fn advance(&mut self, secs: i64) {
            self.pw = self.w.clone();
            self.pd = self.d.clone();
            self.ps = self.s.clone();
            self.prev_now = self.now;
            self.now += Duration::seconds(secs);
            self.started = false;
        }

        /// [`Rig::advance`] then [`Rig::step`].
        pub fn tick(&mut self, a: &mut dyn Automation, secs: i64) -> Vec<Command> {
            self.advance(secs);
            self.step(a)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::rig::Rig;
    use super::*;
    use crate::time::at;

    /// `dagbalans_rapport`: at 23:55 the daily meters are rounded to 0.1 kWh (ties to even,
    /// printed as floats) and the percentages truncated to integers; two lines.
    #[test]
    fn the_day_report_rounds_meters_to_tenths_and_truncates_percentages() {
        let mut r = Rig::new(at(2026, 11, 7, 23, 54, 59));
        r.d.pv_energie_dag_kwh = Some(3.108); // → 3.1
        r.d.huisverbruik_dag_kwh = Some(20.0); // → 20.0, printed with its decimal
        r.d.netimport_dag_kwh = Some(16.25); // 16.25 is exact in binary: ties to even → 16.2
        r.d.injectie_dag_kwh = None; // float(0) → 0.0
        r.d.zelfverbruik_vandaag_pct = 99.7; // |int → 99
        r.d.zelfvoorziening_vandaag_pct = 0.0;
        let mut a = DagbalansRapport;
        assert!(r.step(&mut a).is_empty(), "not yet 23:55");
        let out = r.tick(&mut a, 1);
        assert_eq!(
            out,
            vec![Command::notify(
                "⚡ Dagbalans",
                "Verbruik 20.0 kWh · PV 3.1 kWh · injectie 0.0 kWh · net 16.2 kWh\n\
                 Zelfverbruik 99% · zelfvoorziening 0%"
            )]
        );
        assert_eq!(r.m.last_triggered("dagbalans_rapport"), Some(r.now));
        assert!(r.tick(&mut a, 1).is_empty(), "once a day");
    }
}
