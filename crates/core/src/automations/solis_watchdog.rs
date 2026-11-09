//! Port of HA automation `solis_watchdog`.
//!
//! When `binary_sensor.solis_offline` has been on for 10 min, at most once an hour: stamp
//! `input_datetime.watchdog_laatste_herstart`, notify (high priority), leave a dashboard note
//! and restart. HA restarts itself because the `solis_modbus` integration cannot be reloaded;
//! here the restart is [`Command::SolisReconnect`], re-opening the Modbus session.
//!
//! Choices:
//! - The 5 s `delay` before `homeassistant.restart` (to let the notifications go out) is
//!   dropped: the commands are emitted in order in one tick and the driver sends the
//!   notifications before it reconnects.
//! - HA's restart also resets this trigger. Not modelled: after a restart that does not cure
//!   the outage HA's trigger would fire again 10 min later, inside the hour, so the rate limit
//!   would block it anyway; the net behaviour (one reconnect per outage, per hour) is the same.

use super::{Automation, Cx};
use crate::command::Command;
use crate::time::{self, Ts};
use crate::trigger::Held;

#[derive(Debug, Default)]
pub struct SolisWatchdog {
    /// `state binary_sensor.solis_offline to: 'on' for: 00:10:00`.
    held: Held,
}

/// `now().strftime('%Y-%m-%d %H:%M:%S')`: the helper keeps whole seconds.
fn whole_seconds(t: Ts) -> Ts {
    t - chrono::Duration::nanoseconds(i64::from(t.timestamp_subsec_nanos()))
}

impl Automation for SolisWatchdog {
    fn id(&self) -> &'static str {
        "solis_watchdog"
    }

    fn step(&mut self, cx: &mut Cx) {
        if !self.held.update(cx.d.solis_offline, cx.now, 600.0) {
            return;
        }
        // `as_timestamp(states('input_datetime.watchdog_laatste_herstart'), 0)`: never → 0.
        let last = cx.m.watchdog_laatste_herstart.map_or(0.0, time::epoch);
        if time::epoch(cx.now) - last <= 3600.0 {
            return;
        }
        cx.m.watchdog_laatste_herstart = Some(whole_seconds(cx.now));
        cx.send(Command::notify_high(
            "⚠️ Solis-watchdog",
            "solis_modbus 10 min offline — HA wordt automatisch herstart. (Blijft dit terugkomen, dan is de integratie structureel stuk.)",
        ));
        cx.send(Command::persistent(
            "Solis-watchdog herstart",
            format!("Auto-herstart om {} wegens solis-uitval.", cx.now.format("%H:%M %d-%m")),
        ));
        cx.send(Command::SolisReconnect);
        let id = self.id();
        cx.mark(id);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::battery::rig::Rig;
    use crate::time::at;

    fn t0() -> Ts {
        at(2026, 10, 30, 13, 22, 52)
    }

    fn secs(s: i64) -> Ts {
        t0() + Duration::seconds(s)
    }

    fn offline_rig() -> Rig<SolisWatchdog> {
        let mut r = Rig::new(SolisWatchdog::default());
        r.tick(t0());
        r.d.solis_offline = true;
        r.tick(secs(1));
        r
    }

    /// `for: 00:10:00`, then the stamp, a high-priority push, the dashboard note and the
    /// reconnect, in HA's order.
    #[test]
    fn reconnects_after_ten_minutes_offline() {
        let mut r = offline_rig();
        assert!(r.tick(secs(600)).is_empty(), "9:59");
        let now = secs(601) + Duration::milliseconds(250);
        let out = r.tick(now);
        assert_eq!(
            out,
            vec![
                Command::notify_high(
                    "⚠️ Solis-watchdog",
                    "solis_modbus 10 min offline — HA wordt automatisch herstart. (Blijft dit terugkomen, dan is de integratie structureel stuk.)"
                ),
                Command::persistent("Solis-watchdog herstart", "Auto-herstart om 13:32 30-10 wegens solis-uitval."),
                Command::SolisReconnect,
            ]
        );
        assert_eq!(r.m.watchdog_laatste_herstart, Some(secs(601)), "whole seconds");
        assert_eq!(r.m.last_triggered("solis_watchdog"), Some(now));
    }

    /// `(now − laatste_herstart) > 3600`: a second outage within the hour does nothing (and the
    /// run is not marked); after more than an hour it reconnects again.
    #[test]
    fn at_most_one_reconnect_per_hour() {
        let mut r = offline_rig();
        r.m.watchdog_laatste_herstart = Some(secs(601) - Duration::seconds(3600));
        assert!(r.tick(secs(601)).is_empty(), "exactly 3600 s");
        assert_eq!(r.m.last_triggered("solis_watchdog"), None);

        let mut r = offline_rig();
        r.m.watchdog_laatste_herstart = Some(secs(600) - Duration::seconds(3600));
        assert_eq!(r.tick(secs(601)).len(), 3, "3601 s");
    }

    /// The trigger fires once per outage; it re-arms when the Solis comes back.
    #[test]
    fn fires_once_per_outage() {
        let mut r = offline_rig();
        r.tick(secs(601));
        assert!(r.tick(secs(5000)).is_empty(), "still the same outage");
        r.d.solis_offline = false;
        r.tick(secs(5001));
        r.d.solis_offline = true;
        r.tick(secs(5002));
        assert_eq!(r.tick(secs(5602)).len(), 3);
    }
}
