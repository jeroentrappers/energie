//! Not a port: a notification the HA setup never had.
//!
//! The battery is full, there is solar surplus, and the Buzz is plugged in and below its target,
//! yet it draws nothing for [`STALL_S`]. Usually the cascade starts it within a few minutes; when
//! it doesn't (a car that won't wake up, a charger stuck in Pause, a cloud integration that stopped
//! reporting), the surplus goes to the grid. It tells the user once, with the readings, and arms
//! again after the car has charged for [`REARM_S`] or was unplugged.
//!
//! [`ev_hoger_laadstil`](super::ev_hoger_laadstil) covers a car ranked *above* the battery; this
//! one covers the usual order, battery first, where the car is next in line once the battery is
//! full.
use super::{Automation, Cx};
use crate::cascade;
use crate::command::Command;
use crate::time::{self, Ts};
use crate::world::EvseMode;

/// How long the car must be idle with a full battery and surplus before the notification.
pub const STALL_S: f64 = 600.0;
/// How long the car must charge before another stall is reported.
pub const REARM_S: f64 = 900.0;
/// Battery state of charge counted as full, %.
pub const FULL_SOC: f64 = 98.0;
/// Gross solar surplus that is going unused, W (the Buzz needs about 1.4 kW at 6 A).
pub const SURPLUS_W: f64 = 1000.0;

pub const TITLE: &str = "🔌 Batterij vol — Buzz laadt niet";

#[derive(Debug, Default)]
pub struct EvVolLaadtNiet {
    /// Since when the stall holds.
    since: Option<Ts>,
    /// Since when the car has been charging.
    charging_since: Option<Ts>,
    /// Reported, and not re-armed yet.
    notified: bool,
}

/// The Buzz should be charging from the surplus but isn't.
pub fn stalled(cx: &Cx) -> bool {
    let (w, d, m) = (cx.w, cx.d, &*cx.m);
    let full = w.solis.soc.is_some_and(|s| s >= FULL_SOC);
    let below_target = match (w.buzz.soc, w.buzz.target_soc) {
        (Some(soc), Some(target)) => soc < target,
        _ => true,
    };
    // Off is the user's choice; any other mode is the controller's to manage.
    let switched_off = w.evse.mode == Some(EvseMode::Off);
    full && d.pv_bruto_overschot_w > SURPLUS_W
        && cascade::buzz_wants(w, m)
        && below_target
        && !switched_off
        && !cascade::buzz_drawing(w)
        && d.ev_laadvermogen_smartevse_w <= 200.0
}

fn message(cx: &Cx, minutes: f64) -> String {
    let (w, d) = (cx.w, cx.d);
    let pct = |v: Option<f64>| v.map_or("?".to_string(), |v| format!("{v:.0} %"));
    format!(
        "De batterij is vol ({}) en er is {:.1} kW zonoverschot, maar de Buzz laadt al {minutes:.0} min niet. \
         Buzz {} (doel {}), lader: {} / {} / modus {}. Het overschot gaat naar het net.",
        pct(w.solis.soc),
        d.pv_bruto_overschot_w / 1000.0,
        pct(w.buzz.soc),
        pct(w.buzz.target_soc),
        w.evse.plug.as_deref().unwrap_or("?"),
        w.evse.state.as_deref().unwrap_or("?"),
        w.evse.mode.map_or("?".to_string(), |m| format!("{m:?}")),
    )
}

impl Automation for EvVolLaadtNiet {
    fn id(&self) -> &'static str {
        "ev_vol_laadt_niet"
    }

    fn step(&mut self, cx: &mut Cx) {
        if !(cx.started || cx.every(60)) {
            return;
        }
        let now = cx.now;
        let secs = |t: Ts| time::epoch(now) - time::epoch(t);

        // Re-arm: unplugged, or charged for a while.
        if cascade::buzz_drawing(cx.w) {
            let since = *self.charging_since.get_or_insert(now);
            if secs(since) >= REARM_S {
                self.notified = false;
            }
        } else {
            self.charging_since = None;
        }
        if !cascade::buzz_wants(cx.w, cx.m) {
            self.notified = false;
        }

        if !stalled(cx) {
            self.since = None;
            return;
        }
        let since = *self.since.get_or_insert(now);
        if secs(since) >= STALL_S && !self.notified {
            self.notified = true;
            cx.mark(self.id());
            let text = message(cx, secs(since) / 60.0);
            cx.send(Command::notify(TITLE, text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::ev_modulator::rig::Rig;
    use chrono::Duration;

    /// Battery at 99 %, 2 kW surplus, the Buzz plugged in at 79 % of 100 %, not charging.
    fn rig() -> Rig {
        let mut r = Rig::new();
        r.w.solis.soc = Some(99.0);
        r.d.pv_bruto_overschot_w = 2000.0;
        r.w.buzz.soc = Some(79.0);
        r.w.buzz.target_soc = Some(100.0);
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.state = Some("Connected to EV".into());
        r.w.evse.state_id = Some("B".into());
        r.w.evse.mode = Some(EvseMode::Normal);
        r
    }

    fn minutes(r: &mut Rig, a: &mut EvVolLaadtNiet, n: i64) -> Vec<Command> {
        let mut out = Vec::new();
        for _ in 0..n {
            r.now += Duration::seconds(60);
            out.extend(r.run(a));
        }
        out
    }

    fn titles(out: &[Command]) -> Vec<&str> {
        out.iter()
            .filter_map(|c| match c {
                Command::Notify { title, .. } => Some(title.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Ten minutes of stall: one notification with the readings, and no repeat.
    #[test]
    fn notifies_once_after_ten_minutes() {
        let mut r = rig();
        let mut a = EvVolLaadtNiet::default();
        assert!(r.run(&mut a).is_empty());
        assert!(titles(&minutes(&mut r, &mut a, 9)).is_empty(), "not before ten minutes");
        let out = minutes(&mut r, &mut a, 1);
        assert_eq!(titles(&out), vec![TITLE]);
        let Command::Notify { message, high_priority, .. } = &out[0] else { unreachable!() };
        assert!(!high_priority);
        assert!(message.contains("99 %") && message.contains("2.0 kW") && message.contains("79 %"), "{message}");
        assert!(titles(&minutes(&mut r, &mut a, 60)).is_empty(), "once per stall");
    }

    /// A car that starts within the window, or no reason to charge, never notifies.
    #[test]
    fn quiet_when_it_charges_or_has_no_reason_to() {
        let mut r = rig();
        let mut a = EvVolLaadtNiet::default();
        minutes(&mut r, &mut a, 5);
        r.w.evse.state = Some("Charging".into());
        r.w.evse.state_id = Some("C".into());
        assert!(titles(&minutes(&mut r, &mut a, 30)).is_empty());

        for change in [
            |r: &mut Rig| r.w.solis.soc = Some(95.0),
            |r: &mut Rig| r.d.pv_bruto_overschot_w = 600.0,
            |r: &mut Rig| r.w.buzz.soc = Some(100.0),
            |r: &mut Rig| r.w.evse.mode = Some(EvseMode::Off),
            |r: &mut Rig| {
                r.w.evse.plug = Some("Disconnected".into());
                r.w.evse.state_id = Some("A".into());
            },
        ] {
            let mut r = rig();
            change(&mut r);
            let mut a = EvVolLaadtNiet::default();
            assert!(titles(&minutes(&mut r, &mut a, 30)).is_empty());
        }
    }

    /// After a report, a short charge doesn't re-arm it; fifteen minutes of charging does.
    #[test]
    fn rearms_after_charging_for_a_while() {
        let mut r = rig();
        let mut a = EvVolLaadtNiet::default();
        assert_eq!(titles(&minutes(&mut r, &mut a, 11)).len(), 1);
        let charge = |r: &mut Rig, on: bool| {
            r.w.evse.state = Some(if on { "Charging" } else { "Connected to EV" }.into());
            r.w.evse.state_id = Some(if on { "C" } else { "B" }.into());
        };
        charge(&mut r, true);
        minutes(&mut r, &mut a, 5);
        charge(&mut r, false);
        assert!(titles(&minutes(&mut r, &mut a, 20)).is_empty(), "five minutes of charging is not enough");
        charge(&mut r, true);
        minutes(&mut r, &mut a, 16);
        charge(&mut r, false);
        assert_eq!(titles(&minutes(&mut r, &mut a, 11)), vec![TITLE]);
    }
}
