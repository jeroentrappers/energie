//! Port of HA automations `airco_dump_start` and `airco_dump_stop`. See
//! LOGIC.md "The surplus cascade", item 4.
//!
//! The airco dump sits below both cars and the battery: in summer, 10:00–18:30, with the battery
//! ~full and a surplus left over, both units cool on solar. It waits while a car will still take
//! the surplus, unless the cascade has latched that car as stalled (`ev_hoger_laadstil`).
//!
//! The `numeric_state` thresholds are helpers (`above: input_number...`). HA reads the
//! threshold when the *source* changes; here it is read every tick, so a threshold edit alone
//! can start a hold. Nobody edits these mid-afternoon, and the conditions re-check both values.

use chrono::{Datelike, NaiveTime};

use super::{Automation, Cx};
use crate::cascade;
use crate::command::{AircoUnit, Command};
use crate::jinja;
use crate::trigger::Held;

const TITLE: &str = "❄️ Airco-dump";

fn both(on: bool) -> [Command; 2] {
    [Command::Airco { unit: AircoUnit::Beneden, on }, Command::Airco { unit: AircoUnit::Zolder, on }]
}

/// `sensor.solis_s6_eh1p_battery_soc_2` above `input_number.airco_dump_soc_drempel`; an
/// unavailable SoC does not match.
fn soc_above(cx: &Cx) -> bool {
    cx.w.solis.soc.is_some_and(|soc| soc > cx.s.airco_dump_soc_drempel)
}

/// `sensor.pv_overschot_beschikbaar` above `input_number.airco_dump_overschot_drempel`.
fn surplus_above(cx: &Cx) -> bool {
    cx.d.pv_overschot_beschikbaar_w > cx.s.airco_dump_overschot_drempel
}

/// The start's cascade template: `stalled or (not (ev3_wants and not ev3_maxed) and not
/// (buzz_wants and not buzz_maxed))`. A car that is plugged and not yet at its maximum will
/// take the surplus, so the dump waits for it.
fn no_car_takes_the_surplus(cx: &Cx) -> bool {
    let w = cx.w;
    // `number.ev_charging_cable_charge_current|float(0) >= 16`: the fixed cable maximum.
    let ev3_maxed = w.cable.set_current_a.unwrap_or(0.0) >= 16.0;
    // `override|float(0) >= max|float(32) - 1.5`.
    let buzz_maxed = w.evse.override_a.unwrap_or(0.0) >= cascade::buzz_max_a(w) - 1.5;
    let ev3_hungry = cascade::ev3_wants(w) && !ev3_maxed;
    let buzz_hungry = cascade::buzz_wants(w, cx.m) && !buzz_maxed;
    cx.m.ev_hoger_laadstil || (!ev3_hungry && !buzz_hungry)
}

#[derive(Debug, Default)]
pub struct AircoDumpStart {
    /// Surplus above its threshold for 15 min.
    surplus: Held,
    /// SoC above its threshold for 2 min.
    soc: Held,
}

impl Automation for AircoDumpStart {
    fn id(&self) -> &'static str {
        "airco_dump_start"
    }

    fn step(&mut self, cx: &mut Cx) {
        let by_surplus = self.surplus.update(surplus_above(cx), cx.now, 900.0);
        let by_soc = self.soc.update(soc_above(cx), cx.now, 120.0);
        if !(by_surplus || by_soc || cx.every(600)) {
            return;
        }
        let t = cx.now.time();
        // `condition: time after 10:00 before 18:30`: after is inclusive, before exclusive.
        let in_window = NaiveTime::from_hms_opt(10, 0, 0).expect("valid") <= t
            && t < NaiveTime::from_hms_opt(18, 30, 0).expect("valid");
        let ok = cx.s.airco_overschot_koeling_actief
            && !cx.d.capaciteitspiek_risico
            && soc_above(cx)
            && surplus_above(cx)
            && no_car_takes_the_surplus(cx)
            && (5..=9).contains(&cx.now.month())
            && in_window
            && cx.w.airco.beneden_on == Some(false)
            && cx.w.airco.zolder_on == Some(false);
        if !ok {
            return;
        }
        cx.mark(self.id());
        cx.m.airco_dump_sessie_bezig = true;
        for c in both(true) {
            cx.send(c);
        }
        // The surplus state is a `round(0)` template: HA stores an integer string.
        let surplus = jinja::round(cx.d.pv_overschot_beschikbaar_w, 0) as i64;
        let msg = format!("Batterij vol + overschot {surplus} W — aircos koelen op zonnestroom.");
        cx.send(Command::notify(TITLE, msg));
    }
}

#[derive(Debug, Default)]
pub struct AircoDumpStop {
    /// `overschot_weg`: surplus below −300 W for 10 min.
    gone: Held,
}

impl Automation for AircoDumpStop {
    fn id(&self) -> &'static str {
        "airco_dump_stop"
    }

    fn step(&mut self, cx: &mut Cx) {
        let gone = self.gone.update(cx.d.pv_overschot_beschikbaar_w < -300.0, cx.now, 600.0);
        let trigger_id = if gone {
            "overschot_weg"
        } else if cx.daily(18, 30, 0) {
            "einde_venster"
        } else {
            return;
        };
        if !cx.m.airco_dump_sessie_bezig {
            return;
        }
        cx.mark(self.id());
        for c in both(false) {
            cx.send(c);
        }
        cx.m.airco_dump_sessie_bezig = false;
        cx.send(Command::notify(TITLE, format!("Overschot-koeling gestopt ({trigger_id}).")));
    }
}

#[cfg(test)]
mod tests {
    use super::super::dagbalans::rig::Rig;
    use super::*;
    use crate::time::at;

    /// A July afternoon, dump enabled, battery at 99 %, 1200 W surplus, aircos off, no car:
    /// every start condition passes.
    fn summer() -> Rig {
        let mut r = Rig::new(at(2026, 7, 1, 14, 0, 30));
        r.s.airco_overschot_koeling_actief = true;
        r.w.solis.soc = Some(99.0);
        r.d.pv_overschot_beschikbaar_w = 1200.0;
        r.w.airco.beneden_on = Some(false);
        r.w.airco.zolder_on = Some(false);
        r.w.cable.status = Some("available".into());
        r.w.evse.plug = Some("Disconnected".into());
        r
    }

    /// Step to the next `/10` minute boundary (14:10:00) and run the start.
    fn recheck(r: &mut Rig) -> Vec<Command> {
        let secs = (r.now.timestamp().div_euclid(600) + 1) * 600 - r.now.timestamp();
        r.tick(&mut AircoDumpStart::default(), secs)
    }

    fn started(surplus: &str) -> Vec<Command> {
        let mut v = both(true).to_vec();
        v.push(Command::notify(TITLE, format!("Batterij vol + overschot {surplus} W — aircos koelen op zonnestroom.")));
        v
    }

    /// `airco_dump_start` via the `/10` re-check: both units on, the session latched, and the
    /// surplus printed as HA's integer state.
    #[test]
    fn the_dump_starts_on_the_ten_minute_recheck() {
        let mut r = summer();
        assert!(r.step(&mut AircoDumpStart::default()).is_empty(), "14:00:30 is not a trigger");
        r.d.pv_overschot_beschikbaar_w = 1234.4;
        assert_eq!(recheck(&mut r), started("1234"));
        assert!(r.m.airco_dump_sessie_bezig);
        assert_eq!(r.m.last_triggered("airco_dump_start"), Some(r.now));
    }

    /// LOGIC.md cascade item 4: "a car that is plugged, below its target and not yet maxed
    /// WILL take it, so the dump waits." EV3 plugged at 10 A waits; at the fixed 16 A maximum
    /// it no longer does. Same for the Buzz: override 27.4 A against max 29 waits, 27.5 A
    /// (max − 1.5) is maxed.
    #[test]
    fn the_airco_dump_waits_for_a_car_that_will_still_take_the_surplus() {
        let mut r = summer();
        r.w.cable.status = Some("charging".into());
        r.w.cable.set_current_a = Some(10.0);
        assert!(recheck(&mut r).is_empty(), "EV3 at 10 A will take it");
        r.w.cable.set_current_a = Some(16.0);
        assert_eq!(recheck(&mut r).len(), 3, "EV3 maxed");

        let mut r = summer();
        r.w.evse.plug = Some("Connected".into());
        r.w.evse.override_max_a = Some(29.0);
        r.w.evse.override_a = Some(27.4);
        assert!(recheck(&mut r).is_empty(), "Buzz below 27.5 A will take it");
        r.w.evse.override_a = Some(27.5);
        assert_eq!(recheck(&mut r).len(), 3, "Buzz maxed");
    }

    /// LOGIC.md cascade item 4: "`ev_hoger_laadstil` is the escape hatch: a car that has been
    /// eligible but not drawing for 6 min has demonstrably given up, so the dump proceeds."
    #[test]
    fn a_stalled_car_does_not_block_the_dump() {
        let mut r = summer();
        r.w.ev3.plugged = Some(true);
        r.w.cable.status = Some("unavailable".into()); // falls back to the car's plug sensor
        assert!(recheck(&mut r).is_empty());
        r.m.ev_hoger_laadstil = true;
        assert_eq!(recheck(&mut r).len(), 3);
    }

    /// `airco_dump_start` conditions, each one failing on its own: switch off, peak risk, SoC
    /// not above 98 (or unavailable), surplus not above 800, outside May–September, outside
    /// 10:00–18:30 (18:30 itself excluded, 10:00 included), a unit already on or unavailable.
    #[test]
    fn every_start_condition_blocks_on_its_own() {
        type Tweak = fn(&mut Rig);
        let cases: [(&str, Tweak); 11] = [
            ("switch off", |r| r.s.airco_overschot_koeling_actief = false),
            ("peak risk", |r| r.d.capaciteitspiek_risico = true),
            ("soc at threshold", |r| r.w.solis.soc = Some(98.0)),
            ("soc unavailable", |r| r.w.solis.soc = None),
            ("surplus at threshold", |r| r.d.pv_overschot_beschikbaar_w = 800.0),
            ("october", |r| r.now = at(2026, 10, 1, 13, 59, 30)),
            ("april", |r| r.now = at(2026, 4, 30, 13, 59, 30)),
            ("18:30", |r| r.now = at(2026, 7, 1, 18, 29, 30)),
            ("09:50", |r| r.now = at(2026, 7, 1, 9, 49, 30)),
            ("beneden on", |r| r.w.airco.beneden_on = Some(true)),
            ("zolder unavailable", |r| r.w.airco.zolder_on = None),
        ];
        for (name, tweak) in cases {
            let mut r = summer();
            tweak(&mut r);
            assert!(recheck(&mut r).is_empty(), "{name}");
        }
        let mut r = summer();
        r.now = at(2026, 9, 30, 9, 59, 30);
        assert_eq!(recheck(&mut r).len(), 3, "10:00:00 on 30 September passes");
        let mut r = summer();
        r.now = at(2026, 5, 1, 18, 19, 30);
        assert_eq!(recheck(&mut r).len(), 3, "18:20 on 1 May passes");
    }

    /// `airco_dump_start` trigger 1: surplus above the threshold for 15 min, then the start
    /// runs without waiting for the `/10` pattern. The hold starts at 14:00:31 and ends at
    /// 14:15:31; the 14:10 re-check in between is blocked by a peak risk, which does not
    /// interrupt the hold.
    #[test]
    fn a_surplus_held_for_15_min_starts_the_dump() {
        let mut r = summer();
        let mut a = AircoDumpStart::default();
        r.d.pv_overschot_beschikbaar_w = 500.0;
        assert!(r.step(&mut a).is_empty());
        r.d.pv_overschot_beschikbaar_w = 1000.0;
        assert!(r.tick(&mut a, 1).is_empty(), "14:00:31, hold starts");
        assert!(r.tick(&mut a, 568).is_empty(), "14:09:59");
        r.d.capaciteitspiek_risico = true;
        assert!(r.tick(&mut a, 1).is_empty(), "14:10:00 re-check, blocked by the risk");
        r.d.capaciteitspiek_risico = false;
        assert!(r.tick(&mut a, 330).is_empty(), "14:15:30, 899 s");
        assert_eq!(r.tick(&mut a, 1), started("1000"));
    }

    /// `airco_dump_start` trigger 2: SoC above its threshold for 2 min. The threshold is the
    /// helper's live value (here lowered to 95 %).
    #[test]
    fn a_full_battery_held_for_2_min_starts_the_dump() {
        let mut r = summer();
        r.s.airco_dump_soc_drempel = 95.0;
        let mut a = AircoDumpStart::default();
        r.w.solis.soc = None;
        assert!(r.step(&mut a).is_empty(), "unavailable does not match, but primes");
        r.w.solis.soc = Some(96.0);
        assert!(r.tick(&mut a, 1).is_empty());
        assert!(r.tick(&mut a, 119).is_empty());
        assert_eq!(r.tick(&mut a, 1), started("1200"));
    }

    /// `airco_dump_stop` `overschot_weg`: surplus below −300 W for 10 min ends a running
    /// session; the trigger id is in the message.
    #[test]
    fn the_dump_stops_when_the_surplus_is_gone_for_10_min() {
        let mut r = summer();
        r.m.airco_dump_sessie_bezig = true;
        let mut a = AircoDumpStop::default();
        r.d.pv_overschot_beschikbaar_w = -300.0;
        assert!(r.step(&mut a).is_empty(), "−300 is not below −300");
        r.d.pv_overschot_beschikbaar_w = -301.0;
        assert!(r.tick(&mut a, 1).is_empty());
        assert!(r.tick(&mut a, 599).is_empty());
        let mut want = both(false).to_vec();
        want.push(Command::notify(TITLE, "Overschot-koeling gestopt (overschot_weg)."));
        assert_eq!(r.tick(&mut a, 1), want);
        assert!(!r.m.airco_dump_sessie_bezig);
        assert_eq!(r.m.last_triggered("airco_dump_stop"), Some(r.now));
    }

    /// `airco_dump_stop` `einde_venster` at 18:30, and only for a dump session: aircos a person
    /// switched on by hand are left alone.
    #[test]
    fn the_dump_stops_at_18_30_but_leaves_manual_aircos_alone() {
        let mut r = Rig::new(at(2026, 7, 1, 18, 30, 0));
        let mut a = AircoDumpStop::default();
        assert!(r.step(&mut a).is_empty(), "no session");
        r.m.airco_dump_sessie_bezig = true;
        let mut want = both(false).to_vec();
        want.push(Command::notify(TITLE, "Overschot-koeling gestopt (einde_venster)."));
        assert_eq!(r.step(&mut a), want);
        assert!(r.tick(&mut a, 1).is_empty(), "18:30:01");
    }
}
