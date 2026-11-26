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
//! Unlike HA's single latch, the stall is kept per car ([`Memory::laadstil`]), and the
//! consumers below a car read that car's stall: the battery, the other car, the airco dump and
//! cheap grid charging. A car that draws clears only its own stall. With one latch for both,
//! EV3 → Buzz → Batterij and a full EV3 looped: the EV3 stalled, so the Buzz started; the Buzz
//! drawing cleared the latch, the full EV3 blocked the Buzz again, it was paused 5 minutes
//! later, and 6 minutes after that the EV3 stalled anew: a Buzz session teardown every 11
//! minutes. The HA helpers stay: `ev_hoger_laadstil` is either car stalled.
//!
//! [`Memory::laadstil`]: crate::memory::Memory::laadstil
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

/// A car is drawing: the cable above 0.2 kW, the SmartEVSE above 200 W.
fn drawing(cx: &Cx, car: Consumer) -> bool {
    match car {
        Consumer::Ev3 => cascade::ev3_power_above(cx.w, 0.2),
        Consumer::Buzz => cx.d.ev_laadvermogen_smartevse_w > 200.0,
        Consumer::Batterij => false,
    }
}

/// `car` wants to charge and its charger is reachable.
fn plugged(cx: &Cx, car: Consumer) -> bool {
    match car {
        Consumer::Ev3 => cascade::ev3_wants(cx.w) && cascade::ev3_ctrl(cx.w),
        Consumer::Buzz => cascade::buzz_wants(cx.w, cx.m) && cascade::buzz_ctrl(cx.w),
        Consumer::Batterij => false,
    }
}

/// `car` is stalling: it is ranked above the battery and plugged, there is gross surplus
/// (> 200 W) or cheap Max-tot-piek grid, it does not draw, and no car ranked above it holds the
/// cascade (draws, or is plugged and not stalled itself). A car held back that way is waiting,
/// not stalled. A car ranked *below* that draws does not count: it charges *because* this car
/// stalled, and counting it cleared the stall, blocked that car again and paused it, then
/// stalled anew. Reads the other car's latch, so run the cars in ranking order.
pub fn stalling(cx: &Cx, car: Consumer) -> bool {
    let (w, d, s, m) = (cx.w, cx.d, &*cx.s, &*cx.m);
    let zp = s.zonprioriteit;
    let other = if car == Consumer::Ev3 { Consumer::Buzz } else { Consumer::Ev3 };
    let wants = match car {
        Consumer::Ev3 => cascade::ev3_wants(w),
        Consumer::Buzz => cascade::buzz_wants(w, m),
        Consumer::Batterij => return false,
    };
    let opportunity = d.pv_bruto_overschot_w > 200.0 || cascade::grid_max(w, d, s);
    let held = zp.above(other, car) && (drawing(cx, other) || (plugged(cx, other) && !m.stalled(other)));
    zp.above(car, Consumer::Batterij) && wants && opportunity && !drawing(cx, car) && !held
}

/// Either car is stalling (the original single-latch condition, with the ranking rule above).
pub fn stalled(cx: &Cx) -> bool {
    stalling(cx, Consumer::Ev3) || stalling(cx, Consumer::Buzz)
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
        let now_s = time::epoch(cx.now);
        // Ranking order: a car's stall reads the latch of the car above it.
        for car in cx.s.zonprioriteit.0 {
            let stalling = stalling(cx, car);
            let Some(st) = cx.m.laadstil.get_mut(car) else { continue };
            if !stalling {
                *st = Default::default();
            } else if st.sinds == 0.0 {
                st.sinds = round(now_s, 0);
            } else if now_s - st.sinds >= STALL_S && !st.latched {
                st.latched = true;
                let name = if car == Consumer::Ev3 { "De EV3" } else { "De Buzz" };
                cx.send(crate::command::Command::notify(
                    "🔌 Auto laadt niet — batterij neemt over",
                    format!(
                        "{name} komt al 6 min in aanmerking voor het overschot maar laadt niet. \
                         Wat na hem komt in de zonprioriteit (de andere auto, de batterij) neemt het \
                         overschot (anders injecteren we het). Gaat automatisch terug zodra hij wél laadt."
                    ),
                ));
            }
        }
        // The HA helpers: either car stalled, and since the earliest stall still running.
        let l = cx.m.laadstil;
        cx.m.ev_hoger_laadstil = l.ev3.latched || l.buzz.latched;
        cx.m.ev_hoger_laadstil_sinds = [l.ev3.sinds, l.buzz.sinds].into_iter().filter(|x| *x > 0.0).fold(
            0.0,
            |a, x| {
                if a == 0.0 { x } else { a.min(x) }
            },
        );
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
        r.m.laadstil.buzz.sinds = time::epoch(r.now) + 50.0 - 360.0;
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

    /// A car drawing is not stalling (cable > 0.2 kW, SmartEVSE > 200 W), and neither is a car
    /// waiting behind a car ranked above it that draws. A car ranked *below* that draws does not
    /// clear the stall.
    #[test]
    fn drawing_and_waiting_are_not_a_stall() {
        let mut r = rig();
        r.d.ev_laadvermogen_smartevse_w = 210.0;
        r.run(&mut EvHogerLaadstil);
        assert_eq!(r.m.ev_hoger_laadstil_sinds, 0.0, "the Buzz draws");
        let mut r = rig();
        r.d.ev_laadvermogen_smartevse_w = 200.0;
        r.run(&mut EvHogerLaadstil);
        assert!(r.m.ev_hoger_laadstil_sinds > 0.0, "200 W is not > 200");
        // Buzz → Batterij → EV3: the EV3, ranked below, drawing leaves the Buzz stalling.
        let mut r = rig();
        r.w.cable.power_kw = Some(0.21);
        r.run(&mut EvHogerLaadstil);
        assert!(r.m.laadstil.buzz.sinds > 0.0);
        // EV3 → Buzz → Batterij: the Buzz waits while the EV3 draws.
        let mut r = rig();
        r.s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        r.w.cable.status = Some("charging".into());
        r.w.cable.power_kw = Some(3.0);
        r.run(&mut EvHogerLaadstil);
        assert_eq!(r.m.laadstil, Default::default());
    }

    /// EV3 → Buzz → Batterij at night on cheap Max tot piek, the EV3 plugged but full, the Buzz
    /// plugged. The EV3 stalls after 6 min and the Buzz may charge. The Buzz drawing must not
    /// clear the EV3's stall: that blocked the Buzz again, paused it 5 min later and restarted
    /// it after the next 6 min, a session teardown every 11 min. The stall ends when the EV3
    /// draws, and then the Buzz waits again.
    #[test]
    fn a_lower_car_charging_keeps_the_higher_cars_stall() {
        use crate::automations::ev_modulator::{ChargerView, law};
        let mut r = Rig::new();
        r.s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        r.s.ev_net_niveau = NetNiveau::MaxTotPiek;
        r.s.pv_overschot_laden_actief = true;
        r.d.stroomprijs_nu = Some(0.05);
        r.d.capaciteit_maandtarget_kw = 6.0;
        r.d.kwartier_toegestaan_vermogen_w = Some(6000.0);
        r.w.p1.power_w = Some(300.0);
        r.w.solis.soc = Some(99.0);
        r.w.cable.status = Some("charged".into());
        r.w.cable.set_current_a = Some(6.0);
        r.w.cable.set_current_max_a = Some(16.0);
        r.w.cable.power_kw = Some(0.0);
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.state_id = Some("B".into());
        r.w.evse.state = Some("Connected to EV".into());
        r.w.evse.override_a = Some(6.0);
        r.w.evse.override_max_a = Some(29.0);
        let buzz_allowed = |r: &Rig| law(&ChargerView::buzz(&r.w, &r.d, &r.m), r.now, &r.w, &r.d, &r.s, &r.m).allowed;
        let mut a = EvHogerLaadstil;

        r.run(&mut a);
        assert!(!buzz_allowed(&r), "the EV3 is first and plugged");
        r.wait(420);
        let out = r.run(&mut a);
        assert!(r.m.stalled(Ev3) && r.m.ev_hoger_laadstil);
        assert_eq!(out.len(), 1, "one notification");
        assert!(buzz_allowed(&r), "the stalled EV3 lets the Buzz charge");

        // The Buzz starts and draws; the EV3 stays full. Twenty minutes, one run a minute.
        r.w.evse.state = Some("Charging".into());
        r.w.evse.state_id = Some("C".into());
        r.d.ev_laadvermogen_smartevse_w = 3000.0;
        for _ in 0..20 {
            r.wait(60);
            assert!(r.run(&mut a).is_empty(), "no new notification");
            assert!(r.m.stalled(Ev3), "the Buzz drawing keeps the EV3's stall");
            assert!(buzz_allowed(&r), "so the Buzz keeps charging");
        }

        // The EV3 wakes up and draws: its stall ends and the Buzz waits for it again.
        r.w.cable.status = Some("charging".into());
        r.w.cable.power_kw = Some(1.4);
        r.wait(5);
        r.run(&mut a);
        assert!(!r.m.stalled(Ev3) && !r.m.ev_hoger_laadstil);
        assert_eq!(r.m.ev_hoger_laadstil_sinds, 0.0);
        assert!(!buzz_allowed(&r));
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
