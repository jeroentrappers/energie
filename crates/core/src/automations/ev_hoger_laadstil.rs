//! Port of HA automation `ev_hoger_laadstil_beheer` ("EV-cascade: stilstand van hoger
//! geprioriteerde auto"), LOGIC.md "Cascade-yield liveness".
//!
//! The battery yields to a car ranked above it on *eligibility*, so the car can bootstrap. A car
//! that cannot charge would then starve the battery forever. This latches
//! `input_boolean.ev_hoger_laadstil` ([`Memory::ev_hoger_laadstil`](crate::memory::Memory)) on
//! once a car ranked above the battery has been eligible, with an opportunity to charge, but not
//! drawing for 6 minutes, and clears it (and the start stamp
//! `input_number.ev_hoger_laadstil_sinds`, epoch seconds) as soon as that no longer holds.
//! The car is never blocked: the modulators keep commanding it, so the latch self-clears.
//!
//! Triggers: `time_pattern minutes: /1`; state changes of `sensor.ev_laadvermogen_smartevse_w`,
//! `sensor.ev_charging_cable_vermogen`, `sensor.smartevse_1234_ev_plug_state`,
//! `sensor.smartevse_1234_stateid`, `input_select.zonprioriteit`; `homeassistant: start`.
//! No conditions.
use super::{Automation, Cx};
use crate::cascade;
use crate::jinja::round;
use crate::settings::Consumer;
use crate::time;

/// The stall must last this long before the latch goes on.
pub const STALL_S: f64 = 360.0;

#[derive(Debug, Default)]
pub struct EvHogerLaadstil;

/// `eligible and opportunity and not drawing`: a car ranked above the battery is plugged, there is
/// gross surplus (> 200 W) or cheap Max-tot-piek grid, and neither car draws (cable > 0.2 kW,
/// SmartEVSE > 200 W).
pub fn stalled(cx: &Cx) -> bool {
    let (w, d, s, m) = (cx.w, cx.d, &*cx.s, &*cx.m);
    let zp = s.zonprioriteit;
    let ev3_above = zp.above(Consumer::Ev3, Consumer::Batterij);
    let buzz_above = zp.above(Consumer::Buzz, Consumer::Batterij);
    let eligible = (ev3_above && cascade::ev3_wants(w)) || (buzz_above && cascade::buzz_wants(w, m));
    let drawing = cascade::ev3_power_above(w, 0.2) || d.ev_laadvermogen_smartevse_w > 200.0;
    let surplus = d.pv_bruto_overschot_w > 200.0;
    let opportunity = surplus || cascade::grid_max(w, d, s);
    eligible && opportunity && !drawing
}

impl Automation for EvHogerLaadstil {
    fn id(&self) -> &'static str {
        "ev_hoger_laadstil_beheer"
    }

    fn step(&mut self, cx: &mut Cx) {
        let fired = cx.started
            || cx.every(60)
            || cx.d.ev_laadvermogen_smartevse_w != cx.pd.ev_laadvermogen_smartevse_w
            || cx.w.cable.power_kw != cx.pw.cable.power_kw
            || cx.w.evse.plug != cx.pw.evse.plug
            || cx.w.evse.state_id != cx.pw.evse.state_id
            || cx.s.zonprioriteit != cx.ps.zonprioriteit;
        if !fired {
            return;
        }
        cx.mark(self.id());
        let sinds = cx.m.ev_hoger_laadstil_sinds;
        let now_s = time::epoch(cx.now);
        if !stalled(cx) {
            if sinds != 0.0 {
                cx.m.ev_hoger_laadstil_sinds = 0.0;
            }
            cx.m.ev_hoger_laadstil = false;
        } else if sinds == 0.0 {
            cx.m.ev_hoger_laadstil_sinds = round(now_s, 0);
        } else if sinds > 0.0 && now_s - sinds >= STALL_S && !cx.m.ev_hoger_laadstil {
            cx.m.ev_hoger_laadstil = true;
            cx.send(crate::command::Command::notify(
                "🔌 Auto laadt niet — batterij neemt over",
                "De auto vóór de batterij komt al 6 min in aanmerking voor het zonoverschot maar laadt niet. \
                 De batterij stopt met wijken en neemt het overschot (anders injecteren we het). \
                 Gaat automatisch terug zodra de auto wél laadt.",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;
    use crate::command::Command;
    use crate::settings::Consumer::*;
    use crate::settings::{NetNiveau, Zonprioriteit};
    use chrono::Duration;

    /// Buzz → Batterij → EV3, the Buzz plugged but not drawing, 1000 W gross surplus.
    fn rig() -> Rig {
        let mut r = Rig::new();
        r.s.zonprioriteit = Zonprioriteit([Buzz, Batterij, Ev3]);
        r.s.ev_net_niveau = NetNiveau::Uit;
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.state_id = Some("B".into());
        r.d.pv_bruto_overschot_w = 1000.0;
        r
    }

    /// Stamp the stall on the start tick, latch after 360 s with one notification, clear and
    /// unstamp the moment the car draws.
    #[test]
    fn latches_after_six_minutes_and_clears_when_drawing() {
        let mut r = rig();
        let mut a = EvHogerLaadstil;
        let t0 = r.now;
        assert!(r.run(&mut a).is_empty());
        assert_eq!(r.m.ev_hoger_laadstil_sinds, time::epoch(t0));
        assert_eq!(r.m.last_triggered("ev_hoger_laadstil_beheer"), Some(t0));
        r.wait(50); // 12:01:00
        assert!(r.run(&mut a).is_empty());
        r.wait(300); // 12:06:00, 350 s
        assert!(r.run(&mut a).is_empty());
        assert!(!r.m.ev_hoger_laadstil);
        r.wait(60); // 12:07:00, 410 s
        let out = r.run(&mut a);
        assert!(r.m.ev_hoger_laadstil);
        assert!(
            matches!(&out[..], [Command::Notify { title, .. }] if title == "🔌 Auto laadt niet — batterij neemt over")
        );
        r.wait(60);
        assert!(r.run(&mut a).is_empty(), "notified once");
        r.wait(5);
        r.d.ev_laadvermogen_smartevse_w = 1380.0; // state change trigger
        r.run(&mut a);
        assert!(!r.m.ev_hoger_laadstil);
        assert_eq!(r.m.ev_hoger_laadstil_sinds, 0.0);
    }

    /// 360 s exactly is enough (`>=`).
    #[test]
    fn exactly_360_s_latches() {
        let mut r = rig();
        let mut a = EvHogerLaadstil;
        r.run(&mut a);
        r.m.ev_hoger_laadstil_sinds = time::epoch(r.now) + 50.0 - 360.0;
        r.wait(50);
        r.run(&mut a);
        assert!(r.m.ev_hoger_laadstil);
    }

    /// The stamp is `as_timestamp(now())|round(0)`: 12:00:10.6 → …11.
    #[test]
    fn the_stamp_is_rounded_epoch_seconds() {
        let mut r = rig();
        r.now += Duration::milliseconds(600);
        r.run(&mut EvHogerLaadstil);
        assert_eq!(r.m.ev_hoger_laadstil_sinds, (r.now.timestamp() + 1) as f64);
    }

    /// Not eligible: car not ranked above the battery, or not plugged.
    #[test]
    fn eligibility_needs_a_plugged_car_above_the_battery() {
        let mut r = rig();
        r.s.zonprioriteit = Zonprioriteit([Batterij, Buzz, Ev3]);
        r.run(&mut EvHogerLaadstil);
        assert_eq!(r.m.ev_hoger_laadstil_sinds, 0.0);
        // The EV3 above the battery and plugged (cable offline, car says plugged).
        let mut r = rig();
        r.s.zonprioriteit = Zonprioriteit([Ev3, Batterij, Buzz]);
        r.w.ev3.plugged = Some(true);
        r.run(&mut EvHogerLaadstil);
        assert!(r.m.ev_hoger_laadstil_sinds > 0.0);
        r.w.ev3.plugged = Some(false);
        r.wait(50);
        r.run(&mut EvHogerLaadstil);
        assert_eq!(r.m.ev_hoger_laadstil_sinds, 0.0);
    }

    /// Opportunity: gross surplus > 200 W, or cheap Max tot piek.
    #[test]
    fn opportunity_is_surplus_or_cheap_grid() {
        let mut r = rig();
        r.d.pv_bruto_overschot_w = 200.0;
        r.run(&mut EvHogerLaadstil);
        assert_eq!(r.m.ev_hoger_laadstil_sinds, 0.0, "200 W is not > 200");
        let mut r = rig();
        r.d.pv_bruto_overschot_w = 0.0;
        r.s.ev_net_niveau = NetNiveau::MaxTotPiek;
        r.d.stroomprijs_nu = Some(0.10);
        r.run(&mut EvHogerLaadstil);
        assert!(r.m.ev_hoger_laadstil_sinds > 0.0);
    }

    /// Drawing on either charger is not a stall: cable > 0.2 kW or SmartEVSE > 200 W.
    #[test]
    fn either_car_drawing_is_not_a_stall() {
        let mut r = rig();
        r.w.cable.power_kw = Some(0.21);
        r.run(&mut EvHogerLaadstil);
        assert_eq!(r.m.ev_hoger_laadstil_sinds, 0.0);
        let mut r = rig();
        r.d.ev_laadvermogen_smartevse_w = 200.0;
        r.run(&mut EvHogerLaadstil);
        assert!(r.m.ev_hoger_laadstil_sinds > 0.0, "200 W is not > 200");
    }

    /// Each listed state change triggers a run; nothing else between the minute marks.
    #[test]
    fn triggers() {
        for change in 0..6 {
            let mut r = rig();
            let mut a = EvHogerLaadstil;
            r.run(&mut a);
            r.wait(5);
            match change {
                0 => r.d.ev_laadvermogen_smartevse_w = 10.0,
                1 => r.w.cable.power_kw = Some(0.0),
                2 => r.w.evse.plug = None,
                3 => r.w.evse.state_id = Some("C".into()),
                4 => r.s.zonprioriteit = Zonprioriteit([Buzz, Ev3, Batterij]),
                _ => {}
            }
            r.run(&mut a);
            let ran = r.m.last_triggered("ev_hoger_laadstil_beheer") == Some(r.now);
            assert_eq!(ran, change < 5, "change {change}");
        }
    }

    #[test]
    fn id() {
        assert_eq!(EvHogerLaadstil.id(), "ev_hoger_laadstil_beheer");
    }
}
