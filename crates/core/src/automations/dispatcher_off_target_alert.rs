//! Port of HA automation `dispatcher_off_target_alert`.
//!
//! Warns when `binary_sensor.dispatcher_off_target` has been on for 10 min: the battery has
//! not followed its own setpoint (> 800 W away) while the dispatcher is on.

use super::{Automation, Cx};
use crate::battery::{fmt_float_state, fmt_num_state, fmt_rc_mode};
use crate::command::Command;
use crate::jinja::round;
use crate::trigger::Held;

#[derive(Debug, Default)]
pub struct DispatcherOffTargetAlert {
    /// `state to: 'on' for: 00:10:00`.
    held: Held,
}

impl Automation for DispatcherOffTargetAlert {
    fn id(&self) -> &'static str {
        "dispatcher_off_target_alert"
    }

    fn step(&mut self, cx: &mut Cx) {
        if !self.held.update(cx.d.dispatcher_off_target, cx.now, 600.0) {
            return;
        }
        let sp = fmt_num_state(Some(cx.d.batterij_setpoint_w));
        let batt = fmt_num_state(cx.w.solis.battery_power_w);
        let afwijking = round((cx.w.solis.battery_power_w.unwrap_or(0.0) - cx.d.batterij_setpoint_w).abs(), 0);
        let soc = fmt_num_state(cx.w.solis.soc);
        cx.send(Command::notify(
            "⚠️ Dispatcher off-target",
            format!(
                "De batterij volgt het setpoint al 10 min niet: setpoint {sp} W, batterij {batt} W (afwijking {afwijking:.0} W), SOC {soc}%. Controleer de Solis RC-sturing."
            ),
        ));
        cx.send(Command::persistent(
            "Dispatcher off-target",
            format!(
                "setpoint={sp}W, batterij={batt}W, P1={}W, commando={} om {}",
                fmt_float_state(cx.w.p1.power_w),
                fmt_rc_mode(cx.w.solis.rc_mode),
                cx.now.format("%H:%M"),
            ),
        ));
        let id = self.id();
        cx.mark(id);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::battery::rig::Rig;
    use crate::time::{Ts, at};
    use crate::world::RcMode;

    fn t0() -> Ts {
        at(2026, 11, 7, 14, 3, 0)
    }

    /// `for: 00:10:00`: notify and log once after ten minutes on, with the raw states.
    #[test]
    fn alerts_after_ten_minutes_off_target() {
        let mut r = Rig::new(DispatcherOffTargetAlert::default());
        r.w.solis.battery_power_w = Some(0.0);
        r.w.solis.soc = Some(60.0);
        r.w.p1.power_w = Some(3066.0);
        r.w.solis.rc_mode = Some(RcMode::ForceDischarge);
        r.d.batterij_setpoint_w = 1500.0;
        r.tick(t0());
        r.d.dispatcher_off_target = true;
        assert!(r.tick(t0() + Duration::seconds(1)).is_empty());
        assert!(r.tick(t0() + Duration::seconds(600)).is_empty(), "9:59");
        let out = r.tick(t0() + Duration::seconds(601));
        assert_eq!(
            out,
            vec![
                Command::notify(
                    "⚠️ Dispatcher off-target",
                    "De batterij volgt het setpoint al 10 min niet: setpoint 1500 W, batterij 0 W (afwijking 1500 W), SOC 60%. Controleer de Solis RC-sturing."
                ),
                Command::persistent(
                    "Dispatcher off-target",
                    "setpoint=1500W, batterij=0W, P1=3066.0W, commando=Solis RC Force Battery Discharge om 14:13"
                ),
            ]
        );
        assert!(r.tick(t0() + Duration::seconds(700)).is_empty(), "once per run");
    }

    /// Already on at start: a `state to:` trigger needs a change, so no alert (`trigger::Held`).
    /// Unavailable battery power prints `unavailable` and counts 0 in the deviation.
    #[test]
    fn needs_a_transition_and_prints_unavailable() {
        let mut r = Rig::new(DispatcherOffTargetAlert::default());
        r.d.dispatcher_off_target = true;
        r.tick(t0());
        assert!(r.tick(t0() + Duration::seconds(700)).is_empty());
        r.d.dispatcher_off_target = false;
        r.tick(t0() + Duration::seconds(701));
        r.d.dispatcher_off_target = true;
        r.d.batterij_setpoint_w = -900.0;
        r.tick(t0() + Duration::seconds(702));
        let out = r.tick(t0() + Duration::seconds(1302));
        let Command::Notify { message, .. } = &out[0] else { panic!("{out:?}") };
        assert!(
            message.contains("setpoint -900 W, batterij unavailable W (afwijking 900 W), SOC unavailable%"),
            "{message}"
        );
    }

    /// `dispatcher_off_target_alert` message: `afwijking` is `|batt − setpoint|`. Battery
    /// charging 400 W (−400) against a 1500 W discharge setpoint is 1900 W off.
    #[test]
    fn the_deviation_is_battery_minus_setpoint() {
        let mut r = Rig::new(DispatcherOffTargetAlert::default());
        r.w.solis.battery_power_w = Some(-400.0);
        r.w.solis.soc = Some(60.0);
        r.d.batterij_setpoint_w = 1500.0;
        r.tick(t0());
        r.d.dispatcher_off_target = true;
        r.tick(t0() + Duration::seconds(1));
        let out = r.tick(t0() + Duration::seconds(601));
        let Command::Notify { message, .. } = &out[0] else { panic!("{out:?}") };
        assert!(message.contains("setpoint 1500 W, batterij -400 W (afwijking 1900 W)"), "{message}");
    }
}
