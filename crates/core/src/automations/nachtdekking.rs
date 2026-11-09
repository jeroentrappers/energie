//! Port of HA automations `nachtdekking_start` and `nachtdekking_eind`.
//!
//! At 22:00 the import, battery-discharge and EV-grid meters are stored as the zero point of the
//! dark period. The period ends when the sun takes over (battery charging > 100 W with a gross
//! PV surplus, for 15 min) or at 13:00 on a grey day. The battery's share of the night's energy
//! is `batt / (batt + imp)`, where `imp` is grid import minus what the cars drew from the grid.
//!
//! The variables are rendered by HA to native types: `round(2)` / `round(1)` values are floats
//! and print like Python (`0.0`, `15.0`), see [`py_float`].

use chrono::{NaiveTime, TimeZone, Timelike};

use super::{Automation, Cx};
use crate::command::Command;
use crate::jinja;
use crate::jinja::py_float;
use crate::time;
use crate::trigger::Held;

/// `ev3_laadenergie_net|float(0) + buzz_laadenergie_net|float(0)`, kWh.
fn ev_net_kwh(cx: &Cx) -> f64 {
    cx.d.ev3_laadenergie_net_kwh.unwrap_or(0.0) + cx.d.buzz_laadenergie_net_kwh.unwrap_or(0.0)
}

#[derive(Debug, Default)]
pub struct NachtdekkingStart;

impl Automation for NachtdekkingStart {
    fn id(&self) -> &'static str {
        "nachtdekking_start"
    }

    fn step(&mut self, cx: &mut Cx) {
        if !cx.daily(22, 0, 0) {
            return;
        }
        cx.mark(self.id());
        // An unavailable import register stores 0, which disables tonight's measurement.
        cx.m.nacht_import_startmeting = cx.w.p1.import_kwh.unwrap_or(0.0);
        cx.m.nacht_ontlaad_startmeting = cx.d.batterij_ontladen_energie_kwh.unwrap_or(0.0);
        cx.m.nacht_ev_net_startmeting = jinja::round(ev_net_kwh(cx), 3);
    }
}

/// The automation `variables:` of `nachtdekking_eind`, in kWh and hours.
#[derive(Debug, Clone, PartialEq)]
struct Vars {
    /// Gross grid import since 22:00 (rendered by HA, used nowhere).
    imp_bruto: f64,
    /// What the cars drew from the grid since 22:00; 0 when no EV zero point was stored.
    imp_ev: f64,
    /// Grid import for the house: gross minus the cars, floored at 0.
    imp: f64,
    batt: f64,
    duur: f64,
    reden: &'static str,
}

fn vars(cx: &Cx, trigger_id: &str) -> Vars {
    let import = cx.w.p1.import_kwh.unwrap_or(0.0);
    let gross = jinja::max2(0.0, import - cx.m.nacht_import_startmeting);
    let ev_start = cx.m.nacht_ev_net_startmeting;
    let ev = if ev_start > 0.0 { jinja::max2(0.0, ev_net_kwh(cx) - ev_start) } else { 0.0 };
    let ontladen = cx.d.batterij_ontladen_energie_kwh.unwrap_or(0.0);
    // `today_at('22:00')`, a day back (wall clock: 22:00 local the day before) unless it is
    // already past 22:00; `(as_timestamp(now()) - as_timestamp(s)) / 3600` is real elapsed
    // time, so a DST night counts its 14 or 16 hours.
    let mut day = cx.now.date_naive();
    if cx.now.hour() < 22 {
        day = day.pred_opt().expect("a previous day");
    }
    let naive = day.and_time(NaiveTime::from_hms_opt(22, 0, 0).expect("valid"));
    let start = time::TZ.from_local_datetime(&naive).earliest().expect("22:00 exists in Brussels");
    let hours = time::secs(start, cx.now) / 3600.0;
    Vars {
        imp_bruto: jinja::round(gross, 2),
        imp_ev: jinja::round(ev, 2),
        imp: jinja::round(jinja::max2(0.0, gross - ev), 2),
        batt: jinja::round(jinja::max2(0.0, ontladen - cx.m.nacht_ontlaad_startmeting), 2),
        duur: jinja::round(hours, 1),
        reden: if trigger_id == "zon" { "zon neemt over" } else { "grijze dag (terugval)" },
    }
}

#[derive(Debug, Default)]
pub struct NachtdekkingEind {
    /// `zon`: battery charging > 100 W and gross PV surplus > 0, for 15 min.
    zon: Held,
}

impl Automation for NachtdekkingEind {
    fn id(&self) -> &'static str {
        "nachtdekking_eind"
    }

    fn step(&mut self, cx: &mut Cx) {
        let sun = cx.d.batterij_laadvermogen_w > 100.0 && cx.d.pv_bruto_overschot_w > 0.0;
        let trigger_id = if self.zon.update(sun, cx.now, 900.0) {
            "zon"
        } else if cx.daily(13, 0, 0) {
            "fallback"
        } else {
            return;
        };
        // Conditions: a measurement is running, and it is not the evening sun before 05:00.
        if !(cx.m.nacht_import_startmeting > 0.0 && cx.now.hour() >= 5) {
            return;
        }
        let v = vars(cx, trigger_id);
        cx.mark(self.id());
        cx.m.nacht_import_kwh = v.imp;
        cx.m.nacht_batterij_kwh = v.batt;
        cx.m.nacht_duur_uren = v.duur;
        let share = if v.batt + v.imp > 0.1 { v.batt / (v.batt + v.imp) * 100.0 } else { 0.0 };
        cx.m.nachtdekking = jinja::round(share, 0);
        cx.m.nacht_import_startmeting = 0.0;
        let ev_note = if v.imp_ev > 0.0 {
            format!(" (+{} kWh auto van net, niet meegerekend)", py_float(v.imp_ev))
        } else {
            String::new()
        };
        let msg = format!(
            "Batterij dekte {}% van de donkere periode ({} u, tot {} — {}): {} kWh batterij, {} kWh net{}.",
            jinja::int(cx.m.nachtdekking),
            py_float(v.duur),
            cx.now.format("%H:%M"),
            v.reden,
            py_float(v.batt),
            py_float(v.imp),
            ev_note,
        );
        cx.send(Command::notify("🌙 Nachtdekking", msg));
    }
}

#[cfg(test)]
mod tests {
    use super::super::dagbalans::rig::Rig;
    use super::*;
    use crate::time::at;

    /// `nachtdekking_start` at 22:00: the three zero points; the EV sum is rounded to 3
    /// decimals (600.4119 + 1030.5872 = 1630.9991…, → 1630.999).
    #[test]
    fn the_night_starts_at_22_with_the_meter_readings() {
        let mut r = Rig::new(at(2026, 11, 6, 21, 59, 59));
        r.w.p1.import_kwh = Some(11885.388);
        r.d.batterij_ontladen_energie_kwh = Some(750.288);
        r.d.ev3_laadenergie_net_kwh = Some(600.41195);
        r.d.buzz_laadenergie_net_kwh = Some(1030.58719);
        let mut a = NachtdekkingStart;
        assert!(r.step(&mut a).is_empty());
        assert_eq!(r.m.nacht_import_startmeting, 0.0, "not yet");
        assert!(r.tick(&mut a, 1).is_empty());
        assert_eq!(r.m.nacht_import_startmeting, 11885.388);
        assert_eq!(r.m.nacht_ontlaad_startmeting, 750.288);
        assert_eq!(r.m.nacht_ev_net_startmeting, 1630.999);
        assert_eq!(r.m.last_triggered("nachtdekking_start"), Some(r.now));
    }

    /// With the meters unavailable at 22:00 the zero points are 0, which disables the end.
    #[test]
    fn unavailable_meters_at_22_store_zero() {
        let mut r = Rig::new(at(2026, 11, 6, 21, 59, 59));
        r.m.nacht_import_startmeting = 5.0;
        r.tick(&mut NachtdekkingStart, 1);
        assert_eq!(r.m.nacht_import_startmeting, 0.0);
        assert_eq!(r.m.nacht_ev_net_startmeting, 0.0);
    }

    /// A night that started at 22:00 with the given zero points, now at `now`.
    fn night(now: chrono::DateTime<chrono_tz::Tz>) -> Rig {
        let mut r = Rig::new(now);
        r.m.nacht_import_startmeting = 11885.0;
        r.m.nacht_ontlaad_startmeting = 750.0;
        r.m.nacht_ev_net_startmeting = 1630.0;
        r.w.p1.import_kwh = Some(11885.0);
        r.d.batterij_ontladen_energie_kwh = Some(750.0);
        r.d.ev3_laadenergie_net_kwh = Some(600.0);
        r.d.buzz_laadenergie_net_kwh = Some(1030.0);
        r
    }

    /// `nachtdekking_eind` imp/imp_ev: grid energy the cars drew overnight is not house
    /// demand. Gross import 16.56 kWh, Buzz drew 12.5 kWh of it: imp = 4.06, batt = 3.2,
    /// share 3.2 / 7.26 = 44.07 % → 44. Fallback at 13:00 after a 15.0 h night.
    #[test]
    fn nachtdekking_excludes_ev_grid_energy() {
        let mut r = night(at(2026, 11, 7, 12, 59, 59));
        r.w.p1.import_kwh = Some(11901.56);
        r.d.batterij_ontladen_energie_kwh = Some(753.2);
        r.d.buzz_laadenergie_net_kwh = Some(1042.5);
        let mut a = NachtdekkingEind::default();
        assert!(r.step(&mut a).is_empty());
        let out = r.tick(&mut a, 1);
        assert_eq!(
            out,
            vec![Command::notify(
                "🌙 Nachtdekking",
                "Batterij dekte 44% van de donkere periode (15.0 u, tot 13:00 — grijze dag (terugval)): \
                 3.2 kWh batterij, 4.06 kWh net (+12.5 kWh auto van net, niet meegerekend)."
            )]
        );
        assert_eq!(r.m.nacht_import_kwh, 4.06);
        assert_eq!(r.m.nacht_batterij_kwh, 3.2);
        assert_eq!(r.m.nacht_duur_uren, 15.0);
        assert_eq!(r.m.nachtdekking, 44.0);
        assert_eq!(r.m.nacht_import_startmeting, 0.0, "the measurement is closed");
        assert!(r.tick(&mut a, 3600).is_empty(), "closed: no second report");
    }

    /// The variables on their own: gross 16.56, cars 12.5, imp 4.06; the cars can never push
    /// the house import below 0; without an EV zero point the cars count as 0.
    #[test]
    fn the_variables_floor_at_zero_and_ignore_a_missing_ev_zero_point() {
        let mut r = night(at(2026, 11, 7, 9, 0, 0));
        r.w.p1.import_kwh = Some(11886.0);
        r.d.buzz_laadenergie_net_kwh = Some(1032.0);
        let (v, _) = r.with_cx(|cx| vars(cx, "zon"));
        assert_eq!(v, Vars { imp_bruto: 1.0, imp_ev: 2.0, imp: 0.0, batt: 0.0, duur: 11.0, reden: "zon neemt over" });
        r.m.nacht_ev_net_startmeting = 0.0;
        let (v, _) = r.with_cx(|cx| vars(cx, "fallback"));
        assert_eq!((v.imp_ev, v.imp, v.reden), (0.0, 1.0, "grijze dag (terugval)"));
    }

    /// `zon` trigger: battery charging > 100 W with a gross surplus for 15 min. A tiny night
    /// (batt + imp ≤ 0.1 kWh) reads 0 %, and without car energy the note is left out.
    #[test]
    fn the_sun_taking_over_for_15_min_ends_the_night() {
        let mut r = night(at(2026, 11, 7, 9, 0, 0));
        r.w.p1.import_kwh = Some(11885.05);
        r.d.batterij_ontladen_energie_kwh = Some(750.04);
        let mut a = NachtdekkingEind::default();
        r.step(&mut a);
        r.d.batterij_laadvermogen_w = 101.0;
        r.d.pv_bruto_overschot_w = 1.0;
        assert!(r.tick(&mut a, 1).is_empty());
        assert!(r.tick(&mut a, 899).is_empty());
        assert_eq!(
            r.tick(&mut a, 1),
            vec![Command::notify(
                "🌙 Nachtdekking",
                "Batterij dekte 0% van de donkere periode (11.3 u, tot 09:15 — zon neemt over): \
                 0.04 kWh batterij, 0.05 kWh net."
            )]
        );
    }

    /// Conditions: no measurement running (zero point 0), or before 05:00 (`hour >= 5`).
    #[test]
    fn the_end_needs_a_running_measurement_and_daylight_hours() {
        let mut r = night(at(2026, 11, 7, 12, 59, 59));
        r.m.nacht_import_startmeting = 0.0;
        assert!(r.tick(&mut NachtdekkingEind::default(), 1).is_empty());

        let mut r = night(at(2026, 11, 7, 4, 44, 58));
        let mut a = NachtdekkingEind::default();
        r.step(&mut a);
        r.d.batterij_laadvermogen_w = 500.0;
        r.d.pv_bruto_overschot_w = 200.0;
        r.tick(&mut a, 1);
        assert!(r.tick(&mut a, 900).is_empty(), "04:59:59 is before 05:00");
        assert_eq!(r.m.nacht_import_startmeting, 11885.0);
    }

    /// `nachtdekking_eind` `duur` = `(as_timestamp(now()) - as_timestamp(s)) / 3600`: real
    /// elapsed hours from 22:00 local. The night into 2026-10-25 (clocks back) reads 22:00 →
    /// 13:00 as 16.0 h, the night into 2026-03-29 (clocks forward) as 14.0 h; after 22:00 it
    /// counts from today's 22:00.
    #[test]
    fn the_duration_is_real_elapsed_time_from_22_00() {
        for (day_of, month, hours) in [(25, 10, 16.0), (29, 3, 14.0)] {
            let mut r = night(at(2026, month, day_of, 12, 59, 59));
            let mut a = NachtdekkingEind::default();
            r.tick(&mut a, 1);
            assert_eq!(r.m.nacht_duur_uren, hours, "2026-{month}-{day_of}");
        }

        let mut r = night(at(2026, 11, 7, 23, 30, 0));
        assert_eq!(r.with_cx(|cx| vars(cx, "zon")).0.duur, 1.5);
    }

    /// `nachtdekking_eind` `zon` trigger: `batterij_laadvermogen > 100 and
    /// pv_bruto_overschot > 0`, both strict and both needed. Exactly 100 W of charge, or a
    /// gross surplus of exactly 0, held for 15 min, ends nothing.
    #[test]
    fn the_sun_trigger_needs_strictly_above_100_w_and_a_positive_surplus() {
        for (laad, pv) in [(100.0, 200.0), (500.0, 0.0)] {
            let mut r = night(at(2026, 11, 7, 9, 0, 0));
            let mut a = NachtdekkingEind::default();
            r.step(&mut a);
            r.d.batterij_laadvermogen_w = laad;
            r.d.pv_bruto_overschot_w = pv;
            r.tick(&mut a, 1);
            assert!(r.tick(&mut a, 1800).is_empty(), "{laad} W, surplus {pv}");
            assert_eq!(r.m.nacht_import_startmeting, 11885.0);
        }
    }

    /// `nachtdekking` share: `batt / (batt + imp) * 100 if (batt + imp) > 0.1 else 0`. A night
    /// of exactly 0.1 kWh (0.05 + 0.05) is still "tiny" and reads 0 %; 1 kWh from the battery
    /// and nothing from the grid reads 100 %.
    #[test]
    fn the_share_needs_more_than_0_1_kwh() {
        for (ontladen, import, share) in [(750.05, 11885.05, 0.0), (751.0, 11885.0, 100.0)] {
            let mut r = night(at(2026, 11, 7, 12, 59, 59));
            r.d.batterij_ontladen_energie_kwh = Some(ontladen);
            r.w.p1.import_kwh = Some(import);
            r.tick(&mut NachtdekkingEind::default(), 1);
            assert_eq!(r.m.nachtdekking, share, "{ontladen} / {import}");
        }
    }

    /// `duur`: `s if now().hour >= 22 else s - timedelta(days=1)`. During the 22:00 hour the
    /// night counts from today's 22:00: 22:30 is 0.5 h.
    #[test]
    fn during_the_22_hour_the_night_is_today_s() {
        let mut r = night(at(2026, 11, 7, 22, 30, 0));
        assert_eq!(r.with_cx(|cx| vars(cx, "zon")).0.duur, 0.5);
    }
}
