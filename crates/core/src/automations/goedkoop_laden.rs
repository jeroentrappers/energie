//! Port of HA automation `goedkoop_laden`.
//!
//! Decides `input_boolean.batterij_net_laden`: grid-charge the battery when `Max tot piek` is
//! cheap, the battery is below 99 %, it is not dumping into a car, and it is the battery's turn
//! (ranked before both cars, or no car wants charge). Then it refreshes the SmartEVSE plug latch
//! exactly like `smartevse_plug_latch`.
//!
//! Triggers: `/5 min`, HA start, and a state change of any listed entity. A state trigger
//! without `to:` also fires on attribute changes; attributes are not modelled, which only moves
//! *when* this idempotent decision is re-taken, never *what* it decides. Inputs that are read
//! but not listed (the EV3 cable, `ev_hoger_laadstil`) take effect at the next trigger, as live.

use super::{Automation, Cx};
use crate::cascade;
use crate::settings::{BatterijNiveau, Consumer};
use crate::trigger::changed;

/// `soc < 99`: the battery wants grid charge below this, as in HA.
pub const NET_LADEN_VOL_SOC: f64 = 99.0;
/// Not in HA: once the battery reached [`NET_LADEN_VOL_SOC`] in a cheap window, grid-charging
/// restarts only below this. HA's single threshold on an integer SoC made the battery flip
/// between charging (98 %) and discharging into the house (99 %) every minute or two for the
/// rest of the night (135 RC mode switches between 02:40 and 06:30 on 2026-11-10).
pub const NET_LADEN_HERSTART_SOC: f64 = 95.0;

#[derive(Debug, Default)]
pub struct GoedkoopLaden;

impl GoedkoopLaden {
    /// The `trigger: state` list.
    fn state_changed(cx: &Cx) -> bool {
        let (w, pw, s, ps) = (cx.w, cx.pw, &*cx.s, cx.ps);
        changed(&cx.pd.stroomprijs_nu, &cx.d.stroomprijs_nu)
            || changed(&pw.prices.afname_now, &w.prices.afname_now)
            || changed(&pw.solis.soc, &w.solis.soc)
            || changed(&pw.buzz.soc, &w.buzz.soc)
            || changed(&pw.evse.plug, &w.evse.plug)
            || changed(&ps.goedkoop_laden_prijs_drempel, &s.goedkoop_laden_prijs_drempel)
            || changed(&ps.buzz_laaddoel_soc, &s.buzz_laaddoel_soc)
            || changed(&ps.zonprioriteit, &s.zonprioriteit)
            || changed(&ps.ev_net_niveau, &s.ev_net_niveau)
            || changed(&ps.ev_batterij_niveau, &s.ev_batterij_niveau)
    }

    /// `want_netladen`.
    fn want_netladen(cx: &Cx) -> bool {
        let (w, s, m, d) = (cx.w, &*cx.s, &*cx.m, cx.d);
        let grid_max = cascade::grid_max(w, d, s);
        let batt_wants = w.solis.soc.unwrap_or(0.0) < NET_LADEN_VOL_SOC && !m.batterij_net_laden_vol;
        let batt_dump = s.ev_batterij_niveau == BatterijNiveau::OntlaadBudget;
        // `plug_known == 'Connected' or stateid in ['B', 'C']`, as in the setpoint template.
        let buzz_wants = cascade::buzz_wants(w, m);
        let car_wants = (buzz_wants || cascade::ev3_wants(w)) && !m.ev_hoger_laadstil;
        let zp = s.zonprioriteit;
        let batt_first = zp.above(Consumer::Batterij, Consumer::Buzz) && zp.above(Consumer::Batterij, Consumer::Ev3);
        grid_max && batt_wants && !batt_dump && (batt_first || !car_wants)
    }

    /// [`Memory::batterij_net_laden_vol`]: set when the battery is full in a cheap window, kept
    /// while it stays cheap and the SoC at or above the restart level. An unknown SoC clears it.
    ///
    /// [`Memory::batterij_net_laden_vol`]: crate::memory::Memory::batterij_net_laden_vol
    fn full(cx: &Cx) -> bool {
        let Some(soc) = cx.w.solis.soc else { return false };
        let level = if cx.m.batterij_net_laden_vol { NET_LADEN_HERSTART_SOC } else { NET_LADEN_VOL_SOC };
        cascade::grid_max(cx.w, cx.d, cx.s) && soc >= level
    }
}

impl Automation for GoedkoopLaden {
    fn id(&self) -> &'static str {
        "goedkoop_laden"
    }

    fn step(&mut self, cx: &mut Cx) {
        if !(cx.every(300) || cx.started || Self::state_changed(cx)) {
            return;
        }
        cx.m.batterij_net_laden_vol = Self::full(cx);
        cx.m.batterij_net_laden = Self::want_netladen(cx);
        if let Some(p) = cx.w.evse.plug.as_deref()
            && !cascade::is_missing(Some(p))
            && p != cx.m.smartevse_plug_laatst_bekend
        {
            cx.m.smartevse_plug_laatst_bekend = p.to_string();
        }
        let id = self.id();
        cx.mark(id);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::battery::rig::Rig;
    use crate::settings::{NetNiveau, Zonprioriteit};
    use crate::time::{Ts, at};

    fn t0() -> Ts {
        at(2026, 11, 7, 2, 1, 0)
    }

    fn secs(s: i64) -> Ts {
        t0() + Duration::seconds(s)
    }

    /// Cheap `Max tot piek`, battery 50 %, no car: wants grid-charging. Not yet started.
    fn rig() -> Rig<GoedkoopLaden> {
        let mut r = Rig::new(GoedkoopLaden);
        r.s.ev_net_niveau = NetNiveau::MaxTotPiek;
        r.d.stroomprijs_nu = Some(0.20);
        r.w.solis.soc = Some(50.0);
        r
    }

    /// `trigger: homeassistant start`: decides on the first tick.
    #[test]
    fn decides_at_start() {
        let mut r = rig();
        assert!(r.tick(t0()).is_empty(), "no commands, only the helper");
        assert!(r.m.batterij_net_laden);
        assert_eq!(r.m.last_triggered("goedkoop_laden"), Some(t0()));
    }

    /// `time_pattern minutes: /5`: a change of an unlisted input waits for the next five.
    #[test]
    fn rechecks_every_five_minutes() {
        let mut r = rig();
        r.tick(t0());
        r.tick(secs(1));
        r.w.cable.status = Some("plugged_in".into()); // EV3 wants: not a listed trigger either
        r.tick(secs(2));
        assert!(r.m.batterij_net_laden, "02:01:02, no trigger yet");
        r.tick(at(2026, 11, 7, 2, 5, 0));
        assert!(!r.m.batterij_net_laden, "02:05 re-decides: battery not first, the EV3 wants");
    }

    /// Each listed entity re-triggers on a state change.
    #[test]
    fn listed_state_changes_trigger() {
        type Poke = fn(&mut Rig<GoedkoopLaden>);
        let pokes: [(&str, Poke); 10] = [
            ("stroomprijs_nu", |r| r.d.stroomprijs_nu = Some(0.21)),
            ("market price", |r| r.w.prices.afname_now = Some(0.3)),
            ("soc", |r| r.w.solis.soc = Some(51.0)),
            ("buzz accu", |r| r.w.buzz.soc = Some(40.0)),
            ("plug", |r| r.w.evse.plug = Some("Disconnected".into())),
            ("drempel", |r| r.s.goedkoop_laden_prijs_drempel = 0.39),
            ("laaddoel", |r| r.s.buzz_laaddoel_soc = 80.0),
            ("zonprioriteit", |r| {
                r.s.zonprioriteit = Zonprioriteit([Consumer::Ev3, Consumer::Buzz, Consumer::Batterij])
            }),
            ("net niveau", |r| r.s.ev_net_niveau = NetNiveau::Brug6A),
            ("batterij niveau", |r| r.s.ev_batterij_niveau = BatterijNiveau::Brug6A),
        ];
        for (name, poke) in pokes {
            let mut r = rig();
            r.tick(t0());
            r.tick(secs(1));
            poke(&mut r);
            r.tick(secs(2));
            assert_eq!(r.m.last_triggered("goedkoop_laden"), Some(secs(2)), "{name}");
        }
        let mut r = rig();
        r.tick(t0());
        r.tick(secs(1));
        assert_eq!(r.m.last_triggered("goedkoop_laden"), Some(t0()), "nothing changed");
    }

    /// `grid_max`: `Max tot piek` and strictly below the threshold; the fallback price.
    #[test]
    fn needs_cheap_max_tot_piek() {
        let mut r = rig();
        r.d.stroomprijs_nu = Some(0.38);
        r.tick(t0());
        assert!(!r.m.batterij_net_laden, "0.38 is not below 0.38");
        let mut r = rig();
        r.s.ev_net_niveau = NetNiveau::Brug6A;
        r.tick(t0());
        assert!(!r.m.batterij_net_laden);
        let mut r = rig();
        r.d.stroomprijs_nu = None;
        r.w.prices.afname_now = Some(0.10);
        r.tick(t0());
        assert!(r.m.batterij_net_laden, "falls back to the market price");
    }

    /// `batt_wants = soc|float(0) < 99` and `not batt_dump`.
    #[test]
    fn needs_room_and_no_dump() {
        let mut r = rig();
        r.w.solis.soc = Some(99.0);
        r.tick(t0());
        assert!(!r.m.batterij_net_laden);
        let mut r = rig();
        r.w.solis.soc = None;
        r.tick(t0());
        assert!(r.m.batterij_net_laden, "unavailable SoC reads 0");
        let mut r = rig();
        r.s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        r.tick(t0());
        assert!(!r.m.batterij_net_laden);
    }

    /// Not in HA: full at 99 % in a cheap window latches until the SoC drops below 95 %, so the
    /// integer SoC no longer flips grid-charging on at 98 % and off at 99 % (live 2026-11-10).
    #[test]
    fn full_latches_until_the_restart_level() {
        let mut r = rig();
        let mut t = t0();
        let mut at_soc = |r: &mut Rig<GoedkoopLaden>, soc: f64| {
            r.w.solis.soc = Some(soc);
            t += Duration::seconds(1);
            r.tick(t);
            (r.m.batterij_net_laden, r.m.batterij_net_laden_vol)
        };
        assert_eq!(at_soc(&mut r, 98.0), (true, false), "charging");
        assert_eq!(at_soc(&mut r, 99.0), (false, true), "full: stop and latch");
        assert_eq!(at_soc(&mut r, 98.0), (false, true), "98 % does not restart");
        assert_eq!(at_soc(&mut r, 95.0), (false, true), "95 % is not below 95");
        assert_eq!(at_soc(&mut r, 94.0), (true, false), "below 95: charge again");
        assert_eq!(at_soc(&mut r, 98.0), (true, false), "and on to full");
        assert_eq!(at_soc(&mut r, 99.0), (false, true));
    }

    /// The latch belongs to one cheap window: when the price rises above the threshold it
    /// clears, and the next window tops up from 97 % as HA does.
    #[test]
    fn the_latch_clears_when_the_window_ends() {
        let mut r = rig();
        r.w.solis.soc = Some(99.0);
        r.tick(t0());
        assert!(r.m.batterij_net_laden_vol);
        r.d.stroomprijs_nu = Some(0.45);
        r.tick(secs(1));
        assert!(!r.m.batterij_net_laden_vol, "not cheap: cleared");
        r.w.solis.soc = Some(97.0);
        r.d.stroomprijs_nu = Some(0.20);
        r.tick(secs(2));
        assert_eq!((r.m.batterij_net_laden, r.m.batterij_net_laden_vol), (true, false), "next window tops up");
    }

    /// An unknown SoC never latches (HA's `|float(0)` still wants charge).
    #[test]
    fn an_unknown_soc_clears_the_latch() {
        let mut r = rig();
        r.w.solis.soc = Some(99.0);
        r.tick(t0());
        r.w.solis.soc = None;
        r.tick(secs(1));
        assert_eq!((r.m.batterij_net_laden, r.m.batterij_net_laden_vol), (true, false));
    }

    /// `batt_first or not car_wants`: a wanting car ranked above the battery takes the grid
    /// turn, unless it is stalled (`ev_hoger_laadstil`); a battery ranked first always charges.
    /// `buzz_wants` is `plug_known == 'Connected' or stateid in ['B', 'C']`: the pilot at B or C
    /// with the plug sensor saying Disconnected still counts; pilot A does not.
    #[test]
    fn yields_the_grid_turn_to_a_wanting_car() {
        let mut r = rig();
        r.w.evse.plug = Some("Connected".into());
        r.tick(t0());
        assert!(!r.m.batterij_net_laden, "Buzz → Batterij → EV3 with the Buzz plugged");
        let mut r = rig();
        r.w.evse.plug = Some("Connected".into());
        r.m.ev_hoger_laadstil = true;
        r.tick(t0());
        assert!(r.m.batterij_net_laden, "stalled car");
        let mut r = rig();
        r.w.ev3.plugged = Some(true);
        r.tick(t0());
        assert!(!r.m.batterij_net_laden, "EV3 wants (cable offline, car plugged)");
        let mut r = rig();
        r.s.zonprioriteit = Zonprioriteit([Consumer::Batterij, Consumer::Buzz, Consumer::Ev3]);
        r.w.evse.plug = Some("Connected".into());
        r.tick(t0());
        assert!(r.m.batterij_net_laden, "battery first");
        for (pilot, net_laden) in [("B", false), ("C", false), ("A", true)] {
            let mut r = rig();
            r.s.zonprioriteit = Zonprioriteit([Consumer::Buzz, Consumer::Batterij, Consumer::Ev3]);
            r.w.evse.plug = Some("Disconnected".into());
            r.w.evse.state_id = Some(pilot.into());
            r.tick(t0());
            assert_eq!(r.m.batterij_net_laden, net_laden, "pilot {pilot} with the plug Disconnected");
        }
    }

    /// Plug latch: a known plug state is copied into `input_text.smartevse_plug_laatst_bekend`;
    /// `unavailable` keeps the latch, and the latch is read when the plug blips.
    #[test]
    fn refreshes_the_plug_latch() {
        let mut r = rig();
        r.w.evse.plug = Some("Connected".into());
        r.tick(t0());
        assert_eq!(r.m.smartevse_plug_laatst_bekend, "Connected");
        r.w.evse.plug = Some("unavailable".into());
        r.tick(secs(1));
        assert_eq!(r.m.smartevse_plug_laatst_bekend, "Connected");
        assert!(!r.m.batterij_net_laden, "latched Connected still wants");
        r.w.evse.plug = None;
        r.tick(secs(2));
        assert_eq!(r.m.smartevse_plug_laatst_bekend, "Connected");
        r.w.evse.plug = Some("Connected".into());
        r.tick(at(2026, 11, 7, 2, 5, 0));
        assert_eq!(r.m.smartevse_plug_laatst_bekend, "Connected", "same value: unchanged");
    }
}
