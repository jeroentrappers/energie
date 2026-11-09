//! The charge law shared by the two EV modulators, `ev_laden_buzz` (SmartEVSE) and
//! `ev_laden_cable` (Tuya cable on the EV3): the `target` variable of both automations in
//! the original automation (`ev_modulator.py` in the HA build).
//!
//! The two templates are the same law over a different charger. Everything a charger
//! contributes is read once into a [`ChargerView`] ([`ChargerView::buzz`] /
//! [`ChargerView::cable`]); [`law`] is then the template's `{% set %}` chain and branch ladder,
//! line for line. The per-charger differences are only in the view:
//!
//! | term | Buzz (`ev_laden_buzz`) | EV3 cable (`ev_laden_cable`) |
//! | --- | --- | --- |
//! | `plugged` | plug (latched) `Connected` or pilot B/C | cable status, or car plug when the cable is offline |
//! | `cur` / `mx` | override `|float(6)` / its `max` `|float(32)` | set current `|float(6)` / its `max` `|float(16)` |
//! | `other_plugged` | EV3 plugged, not stalled, cable reachable | Buzz plugged, not stalled, EVSE reachable |
//! | `other_maxed` | cable set current `>=` its `max` `|float(16)` | Buzz override `>= max − 0.5` |
//! | `started` | state `Charging` or `Charging Stopped` | cable current `> 0.5 A` |
//! | `drawing` | state `Charging` | cable current `> 0.5 A` |
//! | `act` | `sensor.ev_laadvermogen_smartevse_w / 230` | cable current, A |
//! | own draw in `load` | `sensor.ev_laadvermogen_smartevse_w` | cable current × voltage `|float(230)` |
//!
//! Sections of `spec/LOGIC.md` this implements: "EV charging — two orthogonal levels (v2)",
//! "Charge-current resolution", "Battery reservation", "Quarter-hour budget control",
//! "Charge-session protection — the Buzz's hard constraint", "Cascade-yield liveness" and
//! "The surplus cascade".

use crate::automations::Cx;
use crate::cascade;
use crate::derived::Derived;
use crate::jinja::{max2, min2, round, round_floor};
use crate::memory::Memory;
use crate::settings::{BatterijNiveau, Consumer, Settings};
use crate::time::{self, Ts};
use crate::world::{EvseMode, World};

/// A live session (pilot C, mode Normal) is never paused before this many seconds after the
/// last real mode change (LOGIC.md "Charge-session protection", `MIN_CHARGE_S`).
pub const MIN_CHARGE_S: f64 = 300.0;
/// A paused Buzz is not re-offered `Normal` before this (`MIN_PAUSE_S`).
pub const MIN_PAUSE_S: f64 = 60.0;
/// `capaciteitspiek_risico` throttles at once but may only stop a car after this (`RISK_STOP_S`).
pub const RISK_STOP_S: f64 = 60.0;
/// Each modulator skips a run within this many seconds of its own previous run.
pub const DEBOUNCE_S: f64 = 25.0;
/// The proactive per-conductor cap: `cur + (27 − maxphase)`.
pub const PHASE_CAP_A: f64 = 27.0;
/// The car's minimum charge current (IEC 61851).
pub const FLOOR_A: f64 = 6.0;
/// The templates' W ↔ A conversion.
pub const VOLT: f64 = 230.0;
/// `automation.buzz_modewissel_stempel`: its `last_triggered` is the last real mode change.
pub const MODE_STEMPEL_ID: &str = "buzz_mode_stempel";

/// Keep [`Memory::capaciteitspiek_risico_sinds`] equal to HA's `last_changed` of
/// `binary_sensor.capaciteitspiek_risico`: set on every flip of the derived value, and at start
/// (HA recreates template entities on start, so `last_changed` is the start time).
/// Both modulators call it every tick, before their own triggers; a second call in the same
/// tick writes the same instant.
pub fn observe_risk(cx: &mut Cx) {
    let flipped = cx.d.capaciteitspiek_risico != cx.pd.capaciteitspiek_risico;
    if cx.started || flipped || cx.m.capaciteitspiek_risico_sinds.is_none() {
        cx.m.capaciteitspiek_risico_sinds = Some(cx.now);
    }
}

/// `risk and has_value(risk) and (now() - last_changed).total_seconds() > 60`.
/// The binary sensor always has a value here, so `has_value` is true.
pub fn risk_long(d: &Derived, m: &Memory, now: Ts) -> bool {
    d.capaciteitspiek_risico && m.capaciteitspiek_risico_sinds.is_some_and(|t| time::secs(t, now) > RISK_STOP_S)
}

/// The common triggers: `time_pattern seconds: /30` and a state change of
/// `sensor.pv_overschot_beschikbaar`, `input_select.ev_net_niveau`, `input_select.ev_batterij_niveau`,
/// `input_select.zonprioriteit`, plus the charger's own plug/status sensor (`plug_changed`).
pub fn triggered(cx: &Cx, plug_changed: bool) -> bool {
    cx.every(30)
        || cx.d.pv_overschot_beschikbaar_w != cx.pd.pv_overschot_beschikbaar_w
        || cx.s.ev_net_niveau != cx.ps.ev_net_niveau
        || cx.s.ev_batterij_niveau != cx.ps.ev_batterij_niveau
        || cx.s.zonprioriteit != cx.ps.zonprioriteit
        || plug_changed
}

/// `this.attributes.last_triggered is none or (now() - last_triggered).total_seconds() > 25`.
pub fn debounce_ok(cx: &Cx, id: &str) -> bool {
    cx.since_triggered(id).is_none_or(|s| s > DEBOUNCE_S)
}

/// Seconds since the last real Buzz mode change; HA's `... if last_triggered is not none else 99999`.
pub fn since_mode_change(cx: &Cx) -> f64 {
    cx.since_triggered(MODE_STEMPEL_ID).unwrap_or(99999.0)
}

/// `BUZZ_PAUSE_OK`: pausing now would not abort a live session — the mode is not already
/// `Pause`, and the car is not charging, or the mode is not `Normal`, or the session has had
/// [`MIN_CHARGE_S`] since the last mode change (LOGIC.md "Charge-session protection").
/// `select.smartevse_1234_mode` unavailable counts as "not Pause" and "not Normal", like HA's
/// `is_state`.
pub fn buzz_pause_ok(w: &World, since_mode_change_s: f64) -> bool {
    w.evse.mode != Some(EvseMode::Pause)
        && (!cascade::buzz_drawing(w) || w.evse.mode != Some(EvseMode::Normal) || since_mode_change_s >= MIN_CHARGE_S)
}

/// What one charger contributes to the shared law. See the table in the module docs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChargerView {
    /// This charger's consumer in `input_select.zonprioriteit`.
    pub me: Consumer,
    /// The other car.
    pub other: Consumer,
    /// A car is connected to this charger.
    pub plugged: bool,
    /// The charger's command entity has a value (`has_value`): it is reachable.
    pub ctrl: bool,
    /// The commanded current, A (`|float(6)`).
    pub cur: f64,
    /// The command entity's `max`, A.
    pub mx: f64,
    /// The other car is plugged, not stalled (`ev_hoger_laadstil`), and its charger reachable.
    pub other_plugged: bool,
    /// The other car is at its maximum.
    pub other_maxed: bool,
    /// A session is under way (allows the +3 A ramp step).
    pub started: bool,
    /// The car is drawing current (template-internal `drawing`).
    pub drawing: bool,
    /// The car's actual current, A.
    pub act: f64,
    /// The car's own draw, W, taken out of P1 in the peak formula's `load`.
    pub own_w: f64,
}

impl ChargerView {
    /// The Buzz on the SmartEVSE, as `ev_laden_buzz` reads it.
    pub fn buzz(w: &World, d: &Derived, m: &Memory) -> Self {
        let evse_w = d.ev_laadvermogen_smartevse_w;
        ChargerView {
            me: Consumer::Buzz,
            other: Consumer::Ev3,
            plugged: cascade::buzz_wants(w, m),
            ctrl: cascade::buzz_ctrl(w),
            cur: w.evse.override_a.unwrap_or(6.0),
            mx: cascade::buzz_max_a(w),
            other_plugged: cascade::ev3_wants(w) && !m.ev_hoger_laadstil && cascade::ev3_ctrl(w),
            // `>= state_attr('number.ev_charging_cable_charge_current', 'max')|float(16)`.
            other_maxed: w.cable.set_current_a.unwrap_or(0.0) >= cascade::ev3_max_a(w),
            started: matches!(w.evse.state.as_deref(), Some("Charging" | "Charging Stopped")),
            drawing: cascade::buzz_drawing(w),
            // `sensor.ev_laadvermogen_smartevse_w / 230`: pilot current, there is no CT.
            act: evse_w / VOLT,
            own_w: evse_w,
        }
    }

    /// The EV3 on the Tuya cable, as `ev_laden_cable` reads it.
    pub fn cable(w: &World, m: &Memory) -> Self {
        let amps = w.cable.current_a.unwrap_or(0.0);
        ChargerView {
            me: Consumer::Ev3,
            other: Consumer::Buzz,
            plugged: cascade::ev3_wants(w),
            ctrl: cascade::ev3_ctrl(w),
            cur: w.cable.set_current_a.unwrap_or(6.0),
            mx: cascade::ev3_max_a(w),
            other_plugged: cascade::buzz_wants(w, m) && !m.ev_hoger_laadstil && cascade::buzz_ctrl(w),
            other_maxed: w.evse.override_a.unwrap_or(0.0) >= cascade::buzz_max_a(w) - 0.5,
            started: cascade::ev3_current_flowing(w),
            drawing: cascade::ev3_current_flowing(w),
            act: amps,
            own_w: amps * w.cable.voltage_v.unwrap_or(VOLT),
        }
    }
}

/// Which rung of the template's branch ladder produced the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Branch {
    /// `not ctrl or not plugged or not master or not allowed or risk_long or hard_max < 6` → 0.
    Gate,
    /// `grid_max`: charge up to the quarter-hour allowance.
    GridMax,
    /// `batt_dump` as the primary car: at least 6 A, the battery feeds the floor.
    DumpPrimary,
    /// `batt_dump` as a secondary car: only on a genuine 6 A of `avail_dump`.
    DumpSecondary,
    /// No sun, no grid/battery level that works without it → 0.
    NoSolar,
    /// `grid_bridge or batt_bridge` as the primary car: the 6 A floor is bridged.
    BridgePrimary,
    /// A bridge level, but a secondary car: only on a genuine 6 A surplus.
    BridgeSecondary,
    /// Plain solar: only on a genuine 6 A surplus.
    Solar,
}

/// The template's intermediate values and the resulting target, A (0 = stop).
#[derive(Debug, Clone, PartialEq)]
pub struct Law {
    pub allowed: bool,
    pub primary: bool,
    pub risk_long: bool,
    pub resv_target: f64,
    pub resv_short_a: f64,
    pub hard_max: f64,
    pub k: f64,
    pub avail_raw: f64,
    pub avail: f64,
    pub avail_dump: f64,
    pub solar: bool,
    pub branch: Branch,
    pub target: f64,
}

/// `[hard_max, [6, x]|max]|min |round(1)`: clamp to the floor and the hard cap.
fn clamp_target(hard_max: f64, x: f64) -> f64 {
    round(min2(hard_max, max2(FLOOR_A, x)), 1)
}

/// The `target` variable of `ev_laden_buzz` / `ev_laden_cable`, for the charger in `v`.
pub fn law(v: &ChargerView, now: Ts, w: &World, d: &Derived, s: &Settings, m: &Memory) -> Law {
    // Price gate and the two levels (LOGIC.md "EV charging — two orthogonal levels").
    let grid_max = cascade::grid_max(w, d, s);
    let grid_bridge = cascade::grid_bridge(w, d, s);
    let batt_bridge = !w.no_battery && s.ev_batterij_niveau == BatterijNiveau::Brug6A;
    let batt_dump = !w.no_battery && s.ev_batterij_niveau == BatterijNiveau::OntlaadBudget;
    let master = s.pv_overschot_laden_actief;
    let risk = d.capaciteitspiek_risico;
    let risk_long = risk_long(d, m, now);

    // The cascade (LOGIC.md "The surplus cascade"): wait for a hungry consumer ranked above.
    let zp = s.zonprioriteit;
    let soc = w.solis.soc.unwrap_or(0.0);
    let other_above = zp.above(v.other, v.me);
    let batt_above = zp.above(Consumer::Batterij, v.me);
    let allowed = !(other_above && v.other_plugged && !v.other_maxed) && !(batt_above && soc < 99.0);
    let primary = !(v.other_plugged && other_above);

    // Hard cap: the charger max, the per-phase headroom and a +3 A ramp step (6 A to start a
    // session); then the battery's unrealised reservation, down to the 6 A floor.
    let maxphase = cascade::maxphase(w);
    let ramp_cap = if v.started { v.cur + 3.0 } else { FLOOR_A };
    let resv_on = s.batterij_reservering_actief && soc < 100.0;
    let resv_target = if resv_on { s.batterij_reservering_w } else { 0.0 };
    let batt_charge_raw = cascade::batt_charge_w(w);
    let resv_short_a = max2(0.0, resv_target - batt_charge_raw) / VOLT;
    let mut hard_max = min2(min2(v.mx, v.cur + (PHASE_CAP_A - maxphase)), ramp_cap);
    if risk {
        // Risk throttles to the floor at once; it only stops after RISK_STOP_S (risk_long).
        hard_max = min2(hard_max, FLOOR_A);
    }
    // `[6, hard_max - resv_short_a]|max if hard_max >= 6`: the reservation trims the current
    // but never decides whether we charge (the `hard_max < 6` gate sees the unreserved cap).
    if hard_max >= FLOOR_A {
        hard_max = max2(FLOOR_A, hard_max - resv_short_a);
    }

    // Available current: surplus + own draw + a lower-ranked battery's charge, scaled by k.
    let besch = d.pv_overschot_beschikbaar_w;
    let k = if v.drawing && v.act >= 4.0 && v.cur >= v.act { min2(1.4, v.cur / v.act) } else { 1.0 };
    let batt_charge = if !batt_above { batt_charge_raw } else { 0.0 };
    // `resv_on`: a full battery takes no reservation.
    let resv = if resv_on { s.batterij_reservering_w } else { 0.0 };
    let avail_raw = round_floor(((besch + batt_charge) / VOLT + v.act) * k, 1);
    // The reservation only trims what is above the 6 A floor; whether to charge is judged on
    // the unreserved figure (LOGIC.md "Battery reservation").
    let avail = if avail_raw >= FLOOR_A { max2(FLOOR_A, avail_raw - resv / VOLT) } else { avail_raw };
    let batt_dis = cascade::batt_discharge_w(w);
    let avail_dump = round_floor(((besch + batt_dis) / VOLT + v.act) * k, 1);
    let pv = d.pv_bruto_overschot_w;
    let solar = d.sun_above_horizon && pv > if v.drawing { 150.0 } else { 800.0 };

    let (branch, target) = if !v.ctrl || !v.plugged || !master || !allowed || risk_long || hard_max < FLOOR_A {
        (Branch::Gate, 0.0)
    } else if grid_max {
        // LOGIC.md "Quarter-hour budget control": steer to the allowance directly. `load` is the
        // house without this car and without the battery's charge, so the reservation (manual or
        // the automatic `batterij_reservering_effectief`, whichever is larger) comes off in full.
        let target_w = d.capaciteit_maandtarget_kw * 1000.0;
        let tw = d.kwartier_toegestaan_vermogen_w.unwrap_or(target_w);
        let load = w.p1.power_w.unwrap_or(0.0) - v.own_w - batt_charge_raw + batt_dis;
        let resv_grid = d.batterij_reservering_effectief_w;
        let raw = round_floor((tw - load - max2(resv_grid, resv_target)) / VOLT, 1);
        let need = if v.drawing { FLOOR_A } else { 10.0 };
        (Branch::GridMax, if raw >= need { clamp_target(hard_max, raw) } else { 0.0 })
    } else if batt_dump {
        if primary {
            (Branch::DumpPrimary, clamp_target(hard_max, avail_dump))
        } else {
            (Branch::DumpSecondary, if avail_dump >= FLOOR_A { clamp_target(hard_max, avail_dump) } else { 0.0 })
        }
    } else if !solar {
        (Branch::NoSolar, 0.0)
    } else if grid_bridge || batt_bridge {
        if primary {
            (Branch::BridgePrimary, clamp_target(hard_max, avail))
        } else {
            (Branch::BridgeSecondary, if avail >= FLOOR_A { clamp_target(hard_max, avail) } else { 0.0 })
        }
    } else {
        (Branch::Solar, if avail >= FLOOR_A { clamp_target(hard_max, avail) } else { 0.0 })
    };

    Law {
        allowed,
        primary,
        risk_long,
        resv_target,
        resv_short_a,
        hard_max,
        k,
        avail_raw,
        avail,
        avail_dump,
        solar,
        branch,
        target,
    }
}

/// Shorthand for [`law`] on the tick's context.
pub fn law_cx(v: &ChargerView, cx: &Cx) -> Law {
    law(v, cx.now, cx.w, cx.d, cx.s, cx.m)
}

/// A small harness that runs one automation over successive ticks with hand-set world,
/// derived values, settings and memory (the engine's `derived::compute` is not used).
#[cfg(test)]
pub(crate) mod rig {
    use chrono::Duration;

    use crate::automations::{Automation, Cx};
    use crate::command::Command;
    use crate::derived::Derived;
    use crate::memory::Memory;
    use crate::settings::Settings;
    use crate::time::{Ts, at};
    use crate::world::World;

    pub struct Rig {
        pub now: Ts,
        pub w: World,
        pub d: Derived,
        pub s: Settings,
        pub m: Memory,
        prev: Option<(Ts, World, Derived, Settings)>,
    }

    impl Rig {
        /// 2026-11-07 12:00:10 (not on a /30 boundary), nothing set.
        pub fn new() -> Self {
            Rig {
                now: at(2026, 11, 7, 12, 0, 10),
                w: World::default(),
                d: Derived::default(),
                s: Settings::default(),
                m: Memory::default(),
                prev: None,
            }
        }

        /// Advance the clock by `secs` seconds.
        pub fn wait(&mut self, secs: i64) {
            self.now += Duration::seconds(secs);
        }

        /// Run `a` once at the current time; the first run is the start tick.
        pub fn run(&mut self, a: &mut dyn Automation) -> Vec<Command> {
            let (prev_now, pw, pd, ps) =
                self.prev.clone().unwrap_or_else(|| (self.now, self.w.clone(), self.d.clone(), self.s.clone()));
            let mut out = Vec::new();
            let mut cx = Cx {
                now: self.now,
                prev_now,
                started: self.prev.is_none(),
                w: &self.w,
                pw: &pw,
                d: &self.d,
                pd: &pd,
                s: &mut self.s,
                ps: &ps,
                m: &mut self.m,
                out: &mut out,
            };
            a.step(&mut cx);
            self.prev = Some((self.now, self.w.clone(), self.d.clone(), self.s.clone()));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{NetNiveau, Zonprioriteit};
    use crate::time::at;
    use Consumer::*;
    use chrono::Duration;

    fn now() -> Ts {
        at(2026, 11, 7, 12, 0, 0)
    }

    /// A sunny noon, Buzz → Batterij → EV3, battery full enough (99 %) not to hold the cars,
    /// no reservation, grid level Uit, power dear. The Buzz is plugged, charging at 10 A
    /// commanded and 10 A measured, with 1150 W exported (= 5 A more).
    fn sunny() -> (World, Derived, Settings, Memory) {
        let mut w = World::default();
        w.evse.plug = Some("Connected".into());
        w.evse.state = Some("Charging".into());
        w.evse.state_id = Some("C".into());
        w.evse.mode = Some(EvseMode::Normal);
        w.evse.override_a = Some(10.0);
        w.evse.override_max_a = Some(29.0);
        w.solis.soc = Some(99.0);
        w.solis.battery_power_w = Some(0.0);
        w.cable.status = Some("available".into());
        w.cable.set_current_a = Some(6.0);
        w.cable.set_current_max_a = Some(16.0);
        w.prices.afname_now = Some(0.40);
        let d = Derived {
            sun_above_horizon: true,
            pv_overschot_beschikbaar_w: 1150.0,
            pv_bruto_overschot_w: 3450.0,
            ev_laadvermogen_smartevse_w: 2300.0,
            capaciteit_maandtarget_kw: 3.9,
            stroomprijs_nu: Some(0.40),
            ..Derived::default()
        };
        let s = Settings {
            zonprioriteit: Zonprioriteit([Buzz, Batterij, Ev3]),
            ev_net_niveau: NetNiveau::Uit,
            ev_batterij_niveau: BatterijNiveau::Uit,
            batterij_reservering_actief: false,
            ..Settings::default()
        };
        (w, d, s, Memory::default())
    }

    fn buzz(w: &World, d: &Derived, s: &Settings, m: &Memory) -> Law {
        law(&ChargerView::buzz(w, d, m), now(), w, d, s, m)
    }

    fn cable(w: &World, d: &Derived, s: &Settings, m: &Memory) -> Law {
        law(&ChargerView::cable(w, m), now(), w, d, s, m)
    }

    /// The EV3 plugged in and charging on the cable at 8 A set / 8 A measured, 230 V.
    fn ev3_charging(w: &mut World) {
        w.cable.status = Some("charging".into());
        w.cable.set_current_a = Some(8.0);
        w.cable.current_a = Some(8.0);
        w.cable.voltage_v = Some(230.0);
        w.cable.power_kw = Some(1.84);
    }

    // ---- no home battery --------------------------------------------------------------------

    /// With the battery ranked first and no battery there, the sun still reaches the car. A
    /// battery that merely stopped answering keeps the car waiting, as before.
    #[test]
    fn without_a_battery_the_surplus_goes_to_the_car() {
        let (mut w, d, mut s, m) = sunny();
        s.zonprioriteit = Zonprioriteit([Batterij, Buzz, Ev3]);
        w.solis.soc = None;
        w.solis.battery_power_w = None;
        assert_eq!(buzz(&w, &d, &s, &m).target, 0.0, "battery first and unknown: wait");
        w.without_battery();
        assert!(buzz(&w, &d, &s, &m).target >= 6.0);
    }

    /// The battery levels mean nothing without one.
    #[test]
    fn without_a_battery_its_levels_do_nothing() {
        let (mut w, mut d, mut s, m) = sunny();
        w.without_battery();
        d.pv_overschot_beschikbaar_w = -3000.0;
        d.pv_bruto_overschot_w = 0.0;
        for level in [BatterijNiveau::Brug6A, BatterijNiveau::OntlaadBudget] {
            s.ev_batterij_niveau = level;
            assert_eq!(buzz(&w, &d, &s, &m).target, 0.0, "{level:?}");
        }
    }

    // ---- the views -------------------------------------------------------------------------

    /// The per-charger table in the module docs, Buzz column.
    #[test]
    fn buzz_view_reads_the_smartevse() {
        let (mut w, d, _, mut m) = sunny();
        let v = ChargerView::buzz(&w, &d, &m);
        assert_eq!((v.me, v.other), (Buzz, Ev3));
        assert!(v.plugged && v.ctrl && v.started && v.drawing);
        assert_eq!((v.cur, v.mx, v.act, v.own_w), (10.0, 29.0, 10.0, 2300.0));
        assert!(!v.other_plugged, "cable says available: no EV3");
        // `Charging Stopped` is a started session that is not drawing.
        w.evse.state = Some("Charging Stopped".into());
        let v = ChargerView::buzz(&w, &d, &m);
        assert!(v.started && !v.drawing);
        // Unknown override and max fall back to 6 and 32.
        w.evse.override_a = None;
        w.evse.override_max_a = None;
        let v = ChargerView::buzz(&w, &d, &m);
        assert_eq!((v.cur, v.mx, v.ctrl), (6.0, 32.0, false));
        // Other plugged needs the EV3 plugged, not stalled, and the cable reachable.
        ev3_charging(&mut w);
        assert!(ChargerView::buzz(&w, &d, &m).other_plugged);
        m.ev_hoger_laadstil = true;
        assert!(!ChargerView::buzz(&w, &d, &m).other_plugged, "a stalled car does not hold the cascade");
        m.ev_hoger_laadstil = false;
        w.cable.set_current_a = None;
        assert!(!ChargerView::buzz(&w, &d, &m).other_plugged, "an offline cable is transparent");
    }

    /// `ev_laden_buzz` `other_maxed`: the cable set current `>= state_attr(cable, 'max')|float(16)`.
    /// 15 A with a max of 13 is maxed; without the attribute the max reads 16, so 15 is not and
    /// 16 is; an unavailable set current reads 0.
    #[test]
    fn buzz_view_other_maxed_is_the_cable_at_its_max() {
        let (mut w, d, _, m) = sunny();
        w.cable.set_current_a = Some(15.0);
        w.cable.set_current_max_a = Some(13.0);
        assert!(ChargerView::buzz(&w, &d, &m).other_maxed, "15 >= max 13");
        w.cable.set_current_max_a = None;
        assert!(!ChargerView::buzz(&w, &d, &m).other_maxed, "15 < 16 (no max attribute)");
        w.cable.set_current_a = Some(16.0);
        assert!(ChargerView::buzz(&w, &d, &m).other_maxed);
        w.cable.set_current_a = None;
        assert!(!ChargerView::buzz(&w, &d, &m).other_maxed, "|float(0)");
    }

    /// The per-charger table, cable column: current-based `started`/`drawing`, `act` in A,
    /// own draw = current × voltage.
    #[test]
    fn cable_view_reads_the_tuya_cable() {
        let (mut w, _, _, mut m) = sunny();
        ev3_charging(&mut w);
        w.cable.voltage_v = Some(240.0);
        let v = ChargerView::cable(&w, &m);
        assert_eq!((v.me, v.other), (Ev3, Buzz));
        assert!(v.plugged && v.ctrl && v.started && v.drawing);
        assert_eq!((v.cur, v.mx, v.act, v.own_w), (8.0, 16.0, 8.0, 1920.0));
        assert!(v.other_plugged, "Buzz connected, EVSE reachable");
        // 0.5 A exactly is not drawing; the power sensor is not what the law reads.
        w.cable.current_a = Some(0.5);
        w.cable.power_kw = Some(5.0);
        let v = ChargerView::cable(&w, &m);
        assert!(!v.started && !v.drawing);
        // Missing readings: set current 6, max 16, voltage 230, current 0.
        w.cable = Default::default();
        w.ev3.plugged = Some(true);
        let v = ChargerView::cable(&w, &m);
        assert_eq!((v.cur, v.mx, v.act, v.own_w, v.ctrl), (6.0, 16.0, 0.0, 0.0, false));
        assert!(v.plugged, "cable offline: the car's own plug sensor");
        w.cable.current_a = Some(10.0);
        assert_eq!(ChargerView::cable(&w, &m).own_w, 2300.0, "voltage |float(230)");
        m.ev_hoger_laadstil = true;
        assert!(!ChargerView::cable(&w, &m).other_plugged);
        m.ev_hoger_laadstil = false;
        w.evse.override_a = None;
        assert!(!ChargerView::cable(&w, &m).other_plugged, "an offline EVSE is transparent");
    }

    /// The cable sees the Buzz maxed at `override >= max − 0.5` (0.1 A resolution).
    #[test]
    fn cable_view_other_maxed_is_buzz_max_minus_half() {
        let (mut w, _, _, m) = sunny();
        w.evse.override_a = Some(28.4);
        assert!(!ChargerView::cable(&w, &m).other_maxed, "28.4 < 29 − 0.5");
        w.evse.override_a = Some(28.5);
        assert!(ChargerView::cable(&w, &m).other_maxed);
        w.evse.override_max_a = None;
        w.evse.override_a = Some(31.4);
        assert!(!ChargerView::cable(&w, &m).other_maxed, "max |float(32)");
        w.evse.override_a = Some(31.5);
        assert!(ChargerView::cable(&w, &m).other_maxed);
    }

    // ---- plain solar -----------------------------------------------------------------------

    /// Solar: avail = (besch/230 + act) floored to 0.1 A, capped at cur + 3 while ramping.
    /// 1150/230 + 10 = 15.0 → capped to 13.0 (ramp step).
    #[test]
    fn plain_solar_ramps_at_most_3_a_per_run() {
        let (w, d, s, m) = sunny();
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.avail_raw, 15.0);
        assert_eq!(l.hard_max, 13.0);
        assert_eq!((l.branch, l.target), (Branch::Solar, 13.0));
    }

    /// The absolute signal: avail adds the car's own draw back. besch −230 W with act 10 A
    /// → 9.0 A, whatever the stale command says.
    #[test]
    fn available_adds_the_cars_own_draw_back() {
        let (w, mut d, s, m) = sunny();
        d.pv_overschot_beschikbaar_w = -230.0;
        assert_eq!(buzz(&w, &d, &s, &m).target, 9.0);
    }

    /// `round(1, 'floor')`: 10 A + 100 W/230 = 10.43 → 10.4.
    #[test]
    fn available_floors_to_a_tenth() {
        let (w, mut d, s, m) = sunny();
        d.pv_overschot_beschikbaar_w = 100.0;
        assert_eq!(buzz(&w, &d, &s, &m).target, 10.4);
    }

    /// A plain-solar car (any rank) needs a genuine 6 A: 5.9 A → 0.
    #[test]
    fn plain_solar_needs_six_amps() {
        let (w, mut d, s, m) = sunny();
        d.pv_overschot_beschikbaar_w = -943.0; // 10 − 4.1 = 5.9
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.avail, 5.9);
        assert_eq!((l.branch, l.target), (Branch::Solar, 0.0));
        d.pv_overschot_beschikbaar_w = -920.0; // exactly 6.0
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0);
    }

    /// `solar` needs the sun above the horizon and pv_bruto > 150 W while drawing,
    /// > 800 W to start.
    #[test]
    fn solar_needs_sun_and_gross_surplus_with_start_hysteresis() {
        let (mut w, mut d, s, m) = sunny();
        d.pv_bruto_overschot_w = 151.0;
        assert!(buzz(&w, &d, &s, &m).solar);
        d.pv_bruto_overschot_w = 150.0;
        let l = buzz(&w, &d, &s, &m);
        assert!(!l.solar);
        assert_eq!((l.branch, l.target), (Branch::NoSolar, 0.0));
        // Not drawing: 800 W is not enough, 801 W is.
        w.evse.state = Some("Connected to EV".into());
        d.pv_bruto_overschot_w = 800.0;
        assert!(!buzz(&w, &d, &s, &m).solar);
        d.pv_bruto_overschot_w = 801.0;
        assert!(buzz(&w, &d, &s, &m).solar);
        d.sun_above_horizon = false;
        assert_eq!(buzz(&w, &d, &s, &m).branch, Branch::NoSolar);
    }

    /// A session that has not started gets 6 A, not more (ramp_cap = 6).
    #[test]
    fn a_new_session_starts_at_six() {
        let (mut w, mut d, s, m) = sunny();
        w.evse.state = Some("Connected to EV".into());
        d.ev_laadvermogen_smartevse_w = 0.0;
        d.pv_overschot_beschikbaar_w = 3000.0; // 13.0 A available
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.avail, 13.0);
        assert_eq!((l.hard_max, l.target), (6.0, 6.0));
    }

    /// The charger max caps the target: Buzz max 11 A.
    #[test]
    fn the_charger_max_caps_the_target() {
        let (mut w, d, s, m) = sunny();
        w.evse.override_max_a = Some(11.0);
        assert_eq!(buzz(&w, &d, &s, &m).target, 11.0);
    }

    /// Per-phase cap: `cur + (27 − maxphase)`, import direction only. 10 + 27 − 25 = 12;
    /// a 30 A *export* phase does not eat headroom.
    #[test]
    fn the_phase_cap_counts_import_only() {
        let (mut w, d, s, m) = sunny();
        w.p1.phase_a = [Some(25.0), Some(-30.0), Some(3.0)];
        assert_eq!(buzz(&w, &d, &s, &m).target, 12.0);
        w.p1.phase_a = [Some(-30.0), Some(0.0), None];
        assert_eq!(buzz(&w, &d, &s, &m).target, 13.0);
        // Phase cap below 6 A disarms: 10 + 27 − 31.5 = 5.5.
        w.p1.phase_a = [Some(31.5), None, None];
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.branch, l.target), (Branch::Gate, 0.0));
    }

    // ---- k factor --------------------------------------------------------------------------

    /// A car that under-draws its command is scaled up by cur/act: cur 10, act 8 (1840 W),
    /// besch 0 → 8 × 1.25 = 10.0, so it keeps its 10 A instead of decaying to 8.
    #[test]
    fn k_factor_scales_a_car_that_under_draws() {
        let (w, mut d, s, m) = sunny();
        d.ev_laadvermogen_smartevse_w = 1840.0;
        d.pv_overschot_beschikbaar_w = 0.0;
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.k, 1.25);
        assert_eq!(l.target, 10.0);
    }

    /// k is capped at 1.4: cur 10, act 5 → k 1.4, avail = 5 × 1.4 = 7.0.
    #[test]
    fn k_factor_is_capped_at_1_4() {
        let (w, mut d, s, m) = sunny();
        d.ev_laadvermogen_smartevse_w = 1150.0;
        d.pv_overschot_beschikbaar_w = 0.0;
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.k, 1.4);
        assert_eq!(l.target, 7.0);
    }

    /// k is 1 when act < 4 A, when not drawing, or when the car over-draws (cur < act).
    #[test]
    fn k_factor_needs_drawing_at_least_4_a_and_cur_at_least_act() {
        let (mut w, mut d, s, m) = sunny();
        d.ev_laadvermogen_smartevse_w = 897.0; // 3.9 A
        assert_eq!(buzz(&w, &d, &s, &m).k, 1.0);
        d.ev_laadvermogen_smartevse_w = 920.0; // 4.0 A
        assert_eq!(buzz(&w, &d, &s, &m).k, 1.4);
        d.ev_laadvermogen_smartevse_w = 2530.0; // 11 A > cur 10
        assert_eq!(buzz(&w, &d, &s, &m).k, 1.0);
        d.ev_laadvermogen_smartevse_w = 1840.0;
        w.evse.state = Some("Charging Stopped".into());
        assert_eq!(buzz(&w, &d, &s, &m).k, 1.0);
    }

    // ---- battery add-back and the reservation ----------------------------------------------

    /// A lower-ranked battery's charge is added back (it mops up behind the car): battery
    /// charging 690 W adds 3 A. A higher-ranked battery's charge is not added.
    #[test]
    fn a_lower_ranked_batterys_charge_is_added_back() {
        let (mut w, mut d, mut s, m) = sunny();
        w.solis.battery_power_w = Some(-690.0);
        d.pv_overschot_beschikbaar_w = 0.0;
        assert_eq!(buzz(&w, &d, &s, &m).avail_raw, 13.0);
        s.zonprioriteit = Zonprioriteit([Batterij, Buzz, Ev3]);
        assert_eq!(buzz(&w, &d, &s, &m).avail_raw, 10.0, "battery above the Buzz");
    }

    /// The reservation trims avail above 6 A: 15.0 − 700/230 (3.04) = 11.96 → 11.96 (rounded
    /// at the end: 12.0).
    #[test]
    fn the_reservation_trims_above_six() {
        let (mut w, d, mut s, m) = sunny();
        s.batterij_reservering_actief = true;
        s.batterij_reservering_w = 700.0;
        w.solis.battery_power_w = Some(-700.0); // battery already holds it: no phase shortfall
        let mut d = d;
        d.pv_overschot_beschikbaar_w = 450.0; // (450 + 700)/230 + 10 = 15.0
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.avail_raw, 15.0);
        assert!((l.avail - 11.956_521_739_130_435).abs() < 1e-12);
        assert_eq!(l.resv_short_a, 0.0);
        assert_eq!(l.target, 12.0);
    }

    /// The reservation never stops a car at the 6 A floor: avail_raw 6.5 − 3.04 = 3.46 is
    /// lifted to 6, and avail_raw 5.9 stays 5.9 (judged unreserved) → stop.
    #[test]
    fn the_reservation_never_stops_a_car_at_the_floor() {
        let (mut w, mut d, mut s, m) = sunny();
        s.batterij_reservering_actief = true;
        s.batterij_reservering_w = 700.0;
        w.solis.battery_power_w = Some(-700.0);
        d.pv_overschot_beschikbaar_w = -805.0; // (−805 + 700)/230 + 10 = 9.54 → 9.5
        assert_eq!(buzz(&w, &d, &s, &m).avail_raw, 9.5);
        d.pv_overschot_beschikbaar_w = -1585.0; // 6.15 → 6.1
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.avail_raw, l.avail, l.target), (6.1, 6.0, 6.0));
        d.pv_overschot_beschikbaar_w = -1643.0; // 5.9
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.avail_raw, l.avail, l.target), (5.9, 5.9, 0.0));
    }

    /// `resv_on` (toggle and soc < 100) gates both `resv_target` (phase shortfall and peak
    /// formula) and `resv` (solar trim): at SoC 100 nothing is held back (avail 10.0); at 99.5
    /// the 460 W (2 A) trims avail to 8.0.
    #[test]
    fn reservation_terms_and_soc_100() {
        let (mut w, mut d, mut s, m) = sunny();
        s.batterij_reservering_actief = true;
        s.batterij_reservering_w = 460.0;
        w.solis.soc = Some(100.0);
        d.pv_overschot_beschikbaar_w = 0.0; // avail_raw 10.0
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.resv_target, 0.0);
        assert_eq!(l.resv_short_a, 0.0);
        assert_eq!(l.avail, 10.0, "a full battery reserves nothing");
        w.solis.soc = Some(99.5);
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.avail, 8.0);
        assert_eq!(l.resv_target, 460.0);
        assert_eq!(l.resv_short_a, 2.0, "nothing charging yet: all of it is short");
        w.solis.battery_power_w = Some(-230.0);
        assert_eq!(buzz(&w, &d, &s, &m).resv_short_a, 1.0, "half realised");
    }

    /// `hard_max = [6, hard_max - resv_short_a]|max if hard_max >= 6`, after the gate's cap:
    /// the reservation shortfall never stops a car at the floor on a phase-limited import.
    /// cur 6, maxphase 24.5 → cap 6 + 2.5 = 8.5; 8.5 − 700/230 = 5.46 → lifted to 6 → the car
    /// keeps 6 A (the old order gated on 5.46 < 6 and stopped it).
    #[test]
    fn the_reservation_shortfall_never_disarms_a_floor_car_on_the_phase_cap() {
        let (mut w, mut d, mut s, m) = sunny();
        w.evse.override_a = Some(6.0);
        d.ev_laadvermogen_smartevse_w = 1380.0;
        w.p1.phase_a = [Some(24.5), None, None];
        d.pv_overschot_beschikbaar_w = 0.0;
        s.ev_batterij_niveau = BatterijNiveau::Brug6A;
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0, "6A-brug, no reservation");
        s.batterij_reservering_actief = true;
        s.batterij_reservering_w = 700.0;
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.hard_max, 6.0);
        assert_eq!((l.branch, l.target), (Branch::BridgePrimary, 6.0));
    }

    /// Above the floor the shortfall trims the cap in full: cur 10, ramp cap 13, a 690 W
    /// reservation with the battery idle is 3.0 A short → hard_max 10.0; a cap already under 6
    /// (cur 6, maxphase 27.5 → 5.5) is left alone and gates the car.
    #[test]
    fn the_reservation_shortfall_trims_the_cap_above_the_floor() {
        let (w, d, mut s, m) = sunny();
        s.batterij_reservering_actief = true;
        s.batterij_reservering_w = 690.0;
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.resv_short_a, l.hard_max), (3.0, 10.0));
        let mut w = w;
        w.evse.override_a = Some(6.0);
        w.p1.phase_a = [Some(27.5), None, None];
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.hard_max, l.branch), (5.5, Branch::Gate));
    }

    // ---- gates -----------------------------------------------------------------------------

    /// Each gate alone forces 0: not reachable, not plugged, master off, not allowed.
    #[test]
    fn the_gates_stop_the_car() {
        let (w, d, s, m) = sunny();
        assert_eq!(buzz(&w, &d, &s, &m).target, 13.0);
        let mut w2 = w.clone();
        w2.evse.override_a = None;
        assert_eq!(buzz(&w2, &d, &s, &m).branch, Branch::Gate, "charger unreachable");
        let mut w2 = w.clone();
        w2.evse.plug = Some("Disconnected".into());
        w2.evse.state_id = Some("A".into());
        assert_eq!(buzz(&w2, &d, &s, &m).branch, Branch::Gate, "not plugged");
        let mut s2 = s.clone();
        s2.pv_overschot_laden_actief = false;
        assert_eq!(buzz(&w, &d, &s2, &m).branch, Branch::Gate, "master off");
    }

    /// A plug blip falls back to the latch: plug unavailable but latch Connected still charges.
    #[test]
    fn a_plug_blip_reads_the_latch() {
        let (mut w, d, s, mut m) = sunny();
        w.evse.plug = None;
        w.evse.state_id = None;
        m.smartevse_plug_laatst_bekend = "Connected".into();
        assert_eq!(buzz(&w, &d, &s, &m).target, 13.0);
        m.smartevse_plug_laatst_bekend = "Disconnected".into();
        assert_eq!(buzz(&w, &d, &s, &m).target, 0.0);
    }

    /// A battery ranked above the car holds it until SoC 99.
    #[test]
    fn a_higher_ranked_battery_holds_the_car_below_99() {
        let (mut w, d, mut s, m) = sunny();
        s.zonprioriteit = Zonprioriteit([Batterij, Buzz, Ev3]);
        w.solis.soc = Some(98.9);
        assert!(!buzz(&w, &d, &s, &m).allowed);
        w.solis.soc = Some(99.0);
        assert!(buzz(&w, &d, &s, &m).allowed);
        w.solis.soc = None;
        assert!(!buzz(&w, &d, &s, &m).allowed, "soc |float(0)");
    }

    /// A higher-ranked car that is plugged and not maxed holds the lower one; once maxed the
    /// lower car may join (as a secondary car).
    #[test]
    fn a_higher_ranked_hungry_car_holds_the_lower_one() {
        let (mut w, d, mut s, m) = sunny();
        ev3_charging(&mut w);
        s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        let l = buzz(&w, &d, &s, &m);
        assert!(!l.allowed && !l.primary);
        assert_eq!(l.target, 0.0);
        w.cable.set_current_a = Some(16.0);
        let l = buzz(&w, &d, &s, &m);
        assert!(l.allowed && !l.primary);
        assert_eq!(l.target, 13.0);
    }

    /// An offline higher-priority charger is transparent: the EV3 is plugged (car telemetry)
    /// but the cable is unreachable, so the Buzz is allowed and primary.
    #[test]
    fn an_offline_higher_priority_charger_is_transparent() {
        let (mut w, d, mut s, m) = sunny();
        s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        w.cable = Default::default();
        w.ev3.plugged = Some(true);
        let l = buzz(&w, &d, &s, &m);
        assert!(l.allowed && l.primary);
        assert_eq!(l.target, 13.0);
    }

    /// A stalled higher-ranked car (`ev_hoger_laadstil`) does not hold the lower one.
    #[test]
    fn a_stalled_higher_car_releases_the_cascade() {
        let (mut w, d, mut s, mut m) = sunny();
        ev3_charging(&mut w);
        s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        m.ev_hoger_laadstil = true;
        let l = buzz(&w, &d, &s, &m);
        assert!(l.allowed && l.primary);
    }

    /// A lower-ranked plugged car does not make this one secondary.
    #[test]
    fn a_lower_ranked_car_does_not_hold_or_demote() {
        let (mut w, d, s, m) = sunny();
        ev3_charging(&mut w);
        let l = buzz(&w, &d, &s, &m);
        assert!(l.allowed && l.primary);
    }

    // ---- risk ------------------------------------------------------------------------------

    /// Risk clamps to 6 A immediately but only stops after 60 s (strictly more).
    #[test]
    fn risk_clamps_to_six_at_once_and_stops_only_after_60_s() {
        let (w, mut d, s, mut m) = sunny();
        d.capaciteitspiek_risico = true;
        m.capaciteitspiek_risico_sinds = Some(now() - Duration::seconds(10));
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.hard_max, l.target), (6.0, 6.0));
        m.capaciteitspiek_risico_sinds = Some(now() - Duration::seconds(60));
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0, "60 s is not > 60");
        m.capaciteitspiek_risico_sinds = Some(now() - Duration::seconds(61));
        let l = buzz(&w, &d, &s, &m);
        assert!(l.risk_long);
        assert_eq!((l.branch, l.target), (Branch::Gate, 0.0));
        // The timestamp alone (risk off) does nothing.
        d.capaciteitspiek_risico = false;
        assert_eq!(buzz(&w, &d, &s, &m).target, 13.0);
    }

    /// Risk without a known `last_changed` never counts as long.
    #[test]
    fn risk_without_a_timestamp_is_not_long() {
        let (w, mut d, s, m) = sunny();
        d.capaciteitspiek_risico = true;
        assert!(!risk_long(&d, &m, now()));
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0);
    }

    // ---- grid: Max tot piek ----------------------------------------------------------------

    /// Night, Max tot piek, cheap. Allowance 3837 W, P1 4000 W with the Buzz drawing 2300 W
    /// → load 1700 W; raw = (3837 − 1700)/230 = 9.29 → 9.2 A.
    fn night_grid() -> (World, Derived, Settings, Memory) {
        let (mut w, mut d, mut s, m) = sunny();
        d.sun_above_horizon = false;
        d.pv_bruto_overschot_w = 0.0;
        d.pv_overschot_beschikbaar_w = -4000.0;
        d.kwartier_toegestaan_vermogen_w = Some(3837.0);
        d.stroomprijs_nu = Some(0.30);
        w.p1.power_w = Some(4000.0);
        s.ev_net_niveau = NetNiveau::MaxTotPiek;
        (w, d, s, m)
    }

    #[test]
    fn grid_max_steers_to_the_quarter_hour_allowance() {
        let (w, d, s, m) = night_grid();
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.branch, l.target), (Branch::GridMax, 9.2));
    }

    /// Without an allowance the flat month target is used: 3.9 kW → (3900 − 1700)/230 = 9.56 → 9.5.
    #[test]
    fn grid_max_falls_back_to_the_month_target() {
        let (w, mut d, s, m) = night_grid();
        d.kwartier_toegestaan_vermogen_w = None;
        assert_eq!(buzz(&w, &d, &s, &m).target, 9.5);
    }

    /// The price gate: at the threshold (not strictly below) grid_max is off and, without
    /// sun, the car stops.
    #[test]
    fn grid_max_is_price_gated() {
        let (w, mut d, s, m) = night_grid();
        d.stroomprijs_nu = Some(0.38);
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.branch, l.target), (Branch::NoSolar, 0.0));
    }

    /// `load` takes the battery out: discharging 460 W adds to the house load (−2 A),
    /// charging 460 W is not house load (+2 A).
    #[test]
    fn grid_max_load_excludes_the_battery() {
        let (mut w, d, s, m) = night_grid();
        w.solis.battery_power_w = Some(460.0);
        assert_eq!(buzz(&w, &d, &s, &m).target, 7.2);
        w.solis.battery_power_w = Some(-460.0);
        assert_eq!(buzz(&w, &d, &s, &m).target, 11.2);
    }

    /// The reservation under the peak: the larger of the manual target and the automatic
    /// `batterij_reservering_effectief` comes off. 460 W manual → −2 A; effective 690 W → −3 A.
    #[test]
    fn grid_max_yields_the_larger_reservation() {
        let (mut w, mut d, mut s, m) = night_grid();
        w.solis.battery_power_w = Some(-460.0); // load 1240; resv fully realised: no shortfall
        s.batterij_reservering_actief = true;
        s.batterij_reservering_w = 460.0;
        // (3837 − 1240 − 460)/230 = 9.29 → 9.2
        assert_eq!(buzz(&w, &d, &s, &m).target, 9.2);
        d.batterij_reservering_effectief_w = 690.0;
        // (3837 − 1240 − 690)/230 = 8.29 → 8.2
        assert_eq!(buzz(&w, &d, &s, &m).target, 8.2);
    }

    /// Starting needs 10 A of room, keeping a session needs 6.
    #[test]
    fn grid_max_needs_ten_to_start_six_to_keep() {
        let (mut w, mut d, s, m) = night_grid();
        d.kwartier_toegestaan_vermogen_w = Some(3080.0); // (3080 − 1700)/230 = 6.0
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0);
        d.kwartier_toegestaan_vermogen_w = Some(3056.0); // 5.89 → 5.8
        assert_eq!(buzz(&w, &d, &s, &m).target, 0.0);
        // Not drawing: own draw 0, so load = P1. Room for 9.9 A is not enough to start, 10.0 is.
        w.evse.state = Some("Connected to EV".into());
        d.ev_laadvermogen_smartevse_w = 0.0;
        w.p1.power_w = Some(1700.0);
        d.kwartier_toegestaan_vermogen_w = Some(3977.0); // 2277/230 = 9.9
        assert_eq!(buzz(&w, &d, &s, &m).target, 0.0);
        d.kwartier_toegestaan_vermogen_w = Some(4000.0); // 10.0 → capped at the 6 A start
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0);
    }

    /// The cable's own draw in `load` is current × voltage: P1 4000 with the EV3 at 8 A × 240 V
    /// → load 2080; (3837 − 2080)/230 = 7.63 → 7.6 A.
    #[test]
    fn grid_max_cable_load_uses_current_times_voltage() {
        let (mut w, d, mut s, m) = night_grid();
        ev3_charging(&mut w);
        w.cable.voltage_v = Some(240.0);
        w.evse.plug = Some("Disconnected".into());
        w.evse.state_id = Some("A".into());
        s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        let l = cable(&w, &d, &s, &m);
        assert_eq!((l.branch, l.target), (Branch::GridMax, 7.6));
    }

    /// Max tot piek ignores the sun (night charging) and takes precedence over the dump.
    #[test]
    fn grid_max_comes_before_the_dump_and_needs_no_sun() {
        let (w, d, mut s, m) = night_grid();
        s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        assert_eq!(buzz(&w, &d, &s, &m).branch, Branch::GridMax);
    }

    /// Live render of the Buzz `target` on 2026-11-07 ~13:10 (POST /api/template): Max tot
    /// piek at 0.3787 < 0.38, allowance 5265 W, P1 4557 W with the Buzz drawing 4646 W
    /// (load −89 W), SoC 100 so `resv_target` 0 but `batterij_reservering_effectief` 700 W:
    /// raw = (5265 + 89 − 700)/230 = 20.23 → 20.2; hard_max = min(29, 20.2 + 27 − 20.7, 23.2).
    /// HA rendered 20.2. Since the 2026-11-07 fixes `batterij_reservering_effectief` renders 0
    /// at SoC 100 (`derived`), and on that input the law gives (5265 + 89)/230 = 23.27 → 23.2.
    #[test]
    fn live_snapshot_grid_max_matches_ha() {
        let mut w = World::default();
        w.evse.plug = Some("Connected".into());
        w.evse.state_id = Some("C".into());
        w.evse.state = Some("Charging".into());
        w.evse.mode = Some(EvseMode::Normal);
        w.evse.override_a = Some(20.2);
        w.evse.override_max_a = Some(29.0);
        w.ev3.plugged = Some(false);
        w.solis.soc = Some(100.0);
        w.solis.battery_power_w = Some(0.0);
        w.p1.phase_a = [Some(1.07), Some(20.7), Some(19.95)];
        w.p1.power_w = Some(4557.0);
        let d = Derived {
            sun_above_horizon: true,
            ev_laadvermogen_smartevse_w: 4646.0,
            pv_overschot_beschikbaar_w: -4557.0,
            pv_bruto_overschot_w: 89.0,
            kwartier_toegestaan_vermogen_w: Some(5265.0),
            capaciteit_maandtarget_kw: 3.9,
            batterij_reservering_effectief_w: 700.0,
            stroomprijs_nu: Some(0.3787),
            ..Derived::default()
        };
        let s = Settings {
            zonprioriteit: Zonprioriteit([Buzz, Batterij, Ev3]),
            ev_net_niveau: NetNiveau::MaxTotPiek,
            ev_batterij_niveau: BatterijNiveau::Uit,
            batterij_reservering_actief: true,
            batterij_reservering_w: 700.0,
            goedkoop_laden_prijs_drempel: 0.38,
            ..Settings::default()
        };
        let l = buzz(&w, &d, &s, &Memory::default());
        assert_eq!(l.resv_target, 0.0);
        assert!((l.hard_max - 23.2).abs() < 1e-9);
        assert_eq!((l.branch, l.target), (Branch::GridMax, 20.2));
        let d = Derived { batterij_reservering_effectief_w: 0.0, ..d };
        assert_eq!(buzz(&w, &d, &s, &Memory::default()).target, 23.2);
    }

    // ---- battery dump ----------------------------------------------------------------------

    /// Ontlaad-budget, primary car: at least 6 A, day or night; `avail_dump` adds the battery's
    /// discharge back. besch 0, discharging 1150 W: 1150/230 + 10 = 15.0 → ramp cap 13.
    #[test]
    fn dump_primary_adds_the_discharge_and_holds_the_floor() {
        let (mut w, mut d, mut s, m) = sunny();
        s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        d.sun_above_horizon = false;
        d.pv_overschot_beschikbaar_w = 0.0;
        w.solis.battery_power_w = Some(1150.0);
        let l = buzz(&w, &d, &s, &m);
        assert_eq!(l.avail_dump, 15.0);
        assert_eq!((l.branch, l.target), (Branch::DumpPrimary, 13.0));
        // Nothing available at all: the primary car still gets the 6 A floor.
        d.pv_overschot_beschikbaar_w = -3450.0; // (−3450 + 1150)/230 + 10 = 0
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0);
    }

    /// `avail_dump = (((besch + batt_dis) / 230 + act) * k)|round(1, 'floor')`: the dump
    /// scales an under-drawing car by k like `avail` does. cur 10, act 8 (1840 W), besch 0,
    /// battery idle → 8 × 1.25 = 10.0.
    #[test]
    fn dump_scales_an_under_drawing_car_by_k() {
        let (w, mut d, mut s, m) = sunny();
        s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        d.ev_laadvermogen_smartevse_w = 1840.0;
        d.pv_overschot_beschikbaar_w = 0.0;
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.k, l.avail_dump), (1.25, 10.0));
    }

    /// Ontlaad-budget, secondary car: only on a genuine 6 A of avail_dump. The dump does not
    /// apply the reservation trim.
    #[test]
    fn dump_secondary_needs_six_amps() {
        let (mut w, mut d, mut s, m) = sunny();
        ev3_charging(&mut w);
        s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        w.cable.set_current_a = Some(16.0); // EV3 maxed: the Buzz may join as secondary
        s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        s.batterij_reservering_actief = true;
        s.batterij_reservering_w = 700.0;
        w.solis.battery_power_w = Some(-700.0);
        d.pv_overschot_beschikbaar_w = -943.0; // 5.9
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.branch, l.avail_dump, l.target), (Branch::DumpSecondary, 5.9, 0.0));
        d.pv_overschot_beschikbaar_w = 0.0; // 10.0, no reservation trim
        assert_eq!(buzz(&w, &d, &s, &m).target, 10.0);
    }

    // ---- bridges ---------------------------------------------------------------------------

    /// 6A-brug (grid, cheap) as the primary car: the floor is bridged on a passing cloud.
    #[test]
    fn grid_bridge_primary_holds_the_floor() {
        let (w, mut d, mut s, m) = sunny();
        s.ev_net_niveau = NetNiveau::Brug6A;
        d.stroomprijs_nu = Some(0.30);
        d.pv_overschot_beschikbaar_w = -1150.0; // 5.0
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.branch, l.target), (Branch::BridgePrimary, 6.0));
        // Dear power: the grid bridge collapses to plain solar.
        d.stroomprijs_nu = Some(0.40);
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.branch, l.target), (Branch::Solar, 0.0));
    }

    /// The bridge still needs the sun: at night 6A-brug is 0.
    #[test]
    fn bridges_need_the_sun() {
        let (w, mut d, mut s, m) = sunny();
        s.ev_batterij_niveau = BatterijNiveau::Brug6A;
        d.sun_above_horizon = false;
        assert_eq!(buzz(&w, &d, &s, &m).branch, Branch::NoSolar);
    }

    /// A secondary car needs a genuine 6 A surplus, even on a bridge level.
    #[test]
    fn a_secondary_car_needs_a_genuine_six_amp_surplus() {
        let (mut w, mut d, mut s, m) = sunny();
        ev3_charging(&mut w);
        w.cable.set_current_a = Some(16.0);
        s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        s.ev_batterij_niveau = BatterijNiveau::Brug6A;
        d.pv_overschot_beschikbaar_w = -943.0; // 5.9
        let l = buzz(&w, &d, &s, &m);
        assert_eq!((l.branch, l.target), (Branch::BridgeSecondary, 0.0));
        d.pv_overschot_beschikbaar_w = -920.0; // 6.0
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0);
    }

    /// The battery 6A-brug is not price-gated.
    #[test]
    fn the_battery_bridge_is_not_price_gated() {
        let (w, mut d, mut s, m) = sunny();
        s.ev_batterij_niveau = BatterijNiveau::Brug6A;
        d.stroomprijs_nu = Some(9.0);
        d.pv_overschot_beschikbaar_w = -1150.0;
        assert_eq!(buzz(&w, &d, &s, &m).target, 6.0);
    }

    /// The final `|round(1)` ties to even: hard_max 10 + 27 − 24.75 = 12.25 → 12.2.
    #[test]
    fn the_target_is_rounded_to_a_tenth() {
        let (mut w, d, s, m) = sunny();
        w.p1.phase_a = [Some(24.75), None, None]; // hard_max 12.25
        assert_eq!(buzz(&w, &d, &s, &m).target, 12.2);
    }

    // ---- the cable's own law ---------------------------------------------------------------

    /// The cable on plain solar: act is the measured current. EV3 at 8 A, besch 460 → 10.0.
    #[test]
    fn cable_follows_the_sun_on_its_measured_current() {
        let (mut w, d, mut s, m) = sunny();
        ev3_charging(&mut w);
        w.evse.plug = Some("Disconnected".into());
        w.evse.state_id = Some("A".into());
        s.zonprioriteit = Zonprioriteit([Ev3, Buzz, Batterij]);
        let mut d = d;
        d.pv_overschot_beschikbaar_w = 460.0;
        let l = cable(&w, &d, &s, &m);
        assert_eq!((l.branch, l.target), (Branch::Solar, 10.0));
        // The cable max (16) caps it.
        d.pv_overschot_beschikbaar_w = 5000.0;
        w.cable.set_current_a = Some(15.0);
        assert_eq!(cable(&w, &d, &s, &m).target, 16.0);
    }

    // ---- session protection and plumbing --------------------------------------------------

    /// BUZZ_PAUSE_OK: not already Pause; and (not charging, or not Normal, or ≥ 300 s).
    #[test]
    fn buzz_pause_ok_protects_a_live_session_for_300_s() {
        let (mut w, ..) = sunny();
        assert!(!buzz_pause_ok(&w, 299.0), "charging in Normal, 299 s");
        assert!(buzz_pause_ok(&w, 300.0));
        w.evse.state = Some("Connected to EV".into());
        assert!(buzz_pause_ok(&w, 0.0), "no live session: nothing to protect");
        w.evse.state = Some("Charging".into());
        w.evse.mode = Some(EvseMode::Solar);
        assert!(buzz_pause_ok(&w, 0.0), "not Normal");
        w.evse.mode = None;
        assert!(buzz_pause_ok(&w, 0.0), "mode unavailable is not Normal and not Pause");
        w.evse.mode = Some(EvseMode::Pause);
        assert!(!buzz_pause_ok(&w, 99999.0), "already paused");
    }

    /// `observe_risk` follows `last_changed`: set at start, on each flip, and if missing.
    #[test]
    fn observe_risk_tracks_last_changed() {
        use super::rig::Rig;
        struct Obs;
        impl crate::automations::Automation for Obs {
            fn id(&self) -> &'static str {
                "obs"
            }
            fn step(&mut self, cx: &mut Cx) {
                observe_risk(cx);
            }
        }
        assert_eq!(crate::automations::Automation::id(&Obs), "obs");
        let mut r = Rig::new();
        r.m.capaciteitspiek_risico_sinds = Some(at(2026, 1, 1, 0, 0, 0));
        let t0 = r.now;
        r.run(&mut Obs);
        assert_eq!(r.m.capaciteitspiek_risico_sinds, Some(t0), "start resets last_changed");
        r.wait(5);
        r.run(&mut Obs);
        assert_eq!(r.m.capaciteitspiek_risico_sinds, Some(t0), "no flip");
        r.wait(5);
        r.d.capaciteitspiek_risico = true;
        let t1 = r.now;
        r.run(&mut Obs);
        assert_eq!(r.m.capaciteitspiek_risico_sinds, Some(t1), "flip on");
        r.wait(5);
        r.m.capaciteitspiek_risico_sinds = None;
        r.run(&mut Obs);
        assert_eq!(r.m.capaciteitspiek_risico_sinds, Some(r.now), "missing");
        r.wait(5);
        r.run(&mut Obs);
        assert_eq!(r.m.capaciteitspiek_risico_sinds, Some(r.now - Duration::seconds(5)));
    }
}
