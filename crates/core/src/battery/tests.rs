//! Tests of the battery brain.
//!
//! The law tests (`setpoint`, `next_bias`) build a [`Brain`] directly, the way the Python mirror
//! builds `decide.Inputs`; the preamble tests build a [`World`] and check what [`Brain::read`]
//! and [`Flags::read`] make of it.
//!
//! Ported from `spec/dispatcher-tests/` (test_decide.py, test_scenarios.py, test_bias.py). The
//! neutral fixture is the Python `make()`: SoC 60 %, floor 40 % (Python's `FLOOR_SOC_RESERVE`;
//! the live default `input_number.batterij_reserve_soc` is 15 %), worst phase 20 A, reservation
//! toggle off (live default on), month target 5.929 kW with no budget sensor, so
//! `allowed = 5929 − 100 = 5829`. `p1_fast` equals `p1` unless a test sets it, like Python's
//! `p1_fast=None`.

use super::*;
use crate::derived::Derived;
use crate::memory::Memory;
use crate::settings::{BatterijNiveau, Consumer, NetNiveau, Settings, Zonprioriteit};
use crate::time::at;
use crate::world::World;

fn b() -> Brain {
    Brain {
        batt: 0.0,
        p1: 0.0,
        p1_fast: 0.0,
        soc: 60.0,
        vloer: 40.0,
        bias: 0.0,
        allowed: 5829.0,
        maxphase: 20.0,
        net_laden: false,
        battery_bridges: false,
        reserve_surplus: false,
        reserve_surplus_w: 800.0,
        reserve_grid_w: 0.0,
        valid: true,
        flags: Flags::default(),
    }
}

/// The fixture with both P1 readings at `x`.
fn p1(x: f64) -> Brain {
    Brain { p1: x, p1_fast: x, ..b() }
}

fn sp(b: Brain) -> f64 {
    setpoint(&b)
}

const HUNGRY: Flags =
    Flags { ev_higher_hungry: true, ev_grid_laden: false, ev_grid_parked: false, ev_batterij_auto_laden: false };
const GRID_LADEN: Flags =
    Flags { ev_higher_hungry: false, ev_grid_laden: true, ev_grid_parked: false, ev_batterij_auto_laden: false };
const PARKED: Flags =
    Flags { ev_higher_hungry: false, ev_grid_laden: false, ev_grid_parked: true, ev_batterij_auto_laden: false };
const AUTO: Flags =
    Flags { ev_higher_hungry: false, ev_grid_laden: false, ev_grid_parked: false, ev_batterij_auto_laden: true };

// ------------------------------------------------------------------ base self-consumption

/// Base law `d = batt + p1 + bias` (template "Batterij setpoint"): export is charged.
/// test_decide.py::test_export_charges.
#[test]
fn export_is_charged() {
    assert_eq!(sp(p1(-1500.0)), -1500.0);
}

/// Base law: import is discharged. test_decide.py::test_import_discharges.
#[test]
fn import_is_discharged() {
    assert_eq!(sp(p1(800.0)), 800.0);
    assert_eq!(sp(p1(0.0)), 0.0, "test_balanced_holds");
}

/// Base law adds the battery's own power: charging 2000 with 500 still exported → 2500.
/// test_decide.py::test_tracking_uses_current_battery.
#[test]
fn base_law_adds_the_current_battery_power() {
    assert_eq!(sp(Brain { batt: -2000.0, ..p1(-500.0) }), -2500.0);
}

/// Base law adds the integral: `0 + 0 + 50 = 50`. test_bias.py::test_bias_added_to_setpoint.
#[test]
fn base_law_adds_the_bias() {
    assert_eq!(sp(Brain { bias: 50.0, ..b() }), 50.0);
}

/// The base law steers on the 90 s median (`p1`), not on the instantaneous `p1_fast`.
#[test]
fn base_law_uses_the_median_p1() {
    assert_eq!(sp(Brain { p1: 800.0, p1_fast: 3000.0, ..b() }), 800.0);
}

/// Exporting never discharges. test_decide.py::test_never_discharge_while_exporting.
#[test]
fn never_discharges_while_exporting() {
    for x in [-100.0, -500.0, -1500.0, -3000.0, -6000.0] {
        assert!(sp(Brain { soc: 70.0, ..p1(x) }) <= 0.0, "{x}");
    }
}

// ------------------------------------------------------------------ floor and ceiling

/// `{% if soc <= vloer %}{% set d = [0, d]|min %}`: no discharge at or below the floor.
/// test_decide.py::test_reserve_floor_blocks_discharge / test_below_reserve_no_discharge /
/// test_editable_reserve_*.
#[test]
fn floor_blocks_discharge_at_and_below_vloer() {
    assert_eq!(sp(Brain { soc: 40.0, ..p1(1500.0) }), 0.0);
    assert_eq!(sp(Brain { soc: 35.0, ..p1(1500.0) }), 0.0);
    assert_eq!(sp(Brain { soc: 38.0, vloer: 10.0, ..p1(1500.0) }), 1500.0, "lowered reserve releases it");
    assert_eq!(sp(Brain { soc: 55.0, vloer: 60.0, ..p1(1500.0) }), 0.0, "raised reserve holds more");
    assert_eq!(sp(Brain { soc: 40.0, ..p1(-1500.0) }), -1500.0, "charging is still allowed");
}

/// The morning floor of 15 %: at 15 % hold, at 16 % discharge.
/// test_decide.py::test_morning_floor_still_15 (the window itself: `vloer_is_15_in_the_morning`).
#[test]
fn morning_floor_is_inclusive() {
    assert_eq!(sp(Brain { soc: 15.0, vloer: 15.0, ..p1(1500.0) }), 0.0);
    assert_eq!(sp(Brain { soc: 16.0, vloer: 15.0, ..p1(1500.0) }), 1500.0);
}

/// `{% if soc >= 100 %}{% set d = [0, d]|max %}`: no charge when full; 96 and 99 still charge.
/// test_decide.py::test_top_blocks_charge_at_100 / test_charges_up_to_100.
#[test]
fn ceiling_blocks_charge_at_100() {
    assert_eq!(sp(Brain { soc: 100.0, ..p1(-2000.0) }), 0.0);
    assert_eq!(sp(Brain { soc: 100.0, ..p1(2000.0) }), 2000.0, "discharge is still allowed");
    assert_eq!(sp(Brain { soc: 96.0, ..p1(-2000.0) }), -2000.0);
    assert_eq!(sp(Brain { soc: 99.0, ..p1(-2000.0) }), -2000.0);
}

// ------------------------------------------------------------------ yield to a hungry higher car

/// Yield with `battery_bridges` (`6A-brug`): `[0, d]|max`, the battery covers the car's bridge
/// import but does not grab the surplus, and the floor still wins.
/// test_decide.py::test_batterij_feeds_bridge / test_batterij_yields_surplus_not_charging /
/// test_batterij_bridge_respects_floor; test_scenarios.py::test_van_batterij_feeds_bridge_then_yields_surplus.
#[test]
fn yield_with_battery_bridge_feeds_the_bridge_but_not_from_surplus() {
    let bridge = |x: f64, soc: f64| sp(Brain { flags: HUNGRY, battery_bridges: true, soc, ..p1(x) });
    assert_eq!(bridge(1400.0, 60.0), 1400.0);
    assert_eq!(bridge(-1500.0, 70.0), 0.0);
    assert_eq!(bridge(1400.0, 40.0), 0.0);
}

/// Yield without bridge or reservation: `{% set d = 0 %}`, hold whatever P1 says.
/// test_decide.py::test_net_does_not_feed_car / test_net_holds_while_ev_ramping /
/// test_reservation_off_by_default_still_holds; test_scenarios.py steps 1, 2 and 2b.
#[test]
fn yield_without_bridge_or_reservation_holds_at_zero() {
    for x in [1400.0, 460.0, 0.0, -700.0, -1500.0] {
        assert_eq!(sp(Brain { flags: HUNGRY, soc: 70.0, ..p1(x) }), 0.0, "{x}");
    }
}

/// No hungry car: plain self-consumption mops up the leftover export.
/// test_decide.py::test_net_mops_up_when_ev_maxed; test_scenarios.py step 3 and
/// test_battery_first_charges_before_cars, step 4 (full) and test_evening_battery_covers_house.
#[test]
fn no_hungry_car_is_plain_self_consumption() {
    assert_eq!(sp(Brain { soc: 70.0, ..p1(-2500.0) }), -2500.0);
    assert_eq!(sp(Brain { soc: 60.0, ..p1(-3000.0) }), -3000.0);
    assert_eq!(sp(Brain { soc: 100.0, ..p1(-2500.0) }), 0.0);
    assert_eq!(sp(Brain { soc: 80.0, ..p1(900.0) }), 900.0);
}

/// Yield with the surplus reservation on: `[-reserve_surplus_w, [0, d]|min]|max`, charge the
/// leftover export up to the reservation, never discharge, never above 100 %.
/// LOGIC.md "Battery reservation"; test_decide.py::test_reservation_takes_leftover_up_to_reservation /
/// _below_amount_takes_only_what_is_there / _never_discharges / _respects_charge_ceiling.
#[test]
fn yield_with_reservation_charges_leftover_up_to_the_reservation() {
    let resv = |x: f64, soc: f64| sp(Brain { flags: HUNGRY, reserve_surplus: true, soc, ..p1(x) });
    assert_eq!(resv(-1500.0, 70.0), -800.0);
    assert_eq!(resv(-300.0, 70.0), -300.0);
    assert_eq!(resv(1400.0, 70.0), 0.0);
    assert_eq!(resv(-1500.0, 100.0), 0.0);
}

/// The bridge outranks the reservation (`if battery_bridges ... elif reserve_surplus`).
/// test_decide.py::test_reservation_yields_to_battery_bridge.
#[test]
fn battery_bridge_outranks_the_reservation() {
    let b = Brain { flags: HUNGRY, battery_bridges: true, reserve_surplus: true, soc: 70.0, ..p1(1400.0) };
    assert_eq!(sp(b), 1400.0);
}

/// `ev_higher_hungry and not net_laden`: grid-charging owns the battery, the yield is skipped.
/// Phase cap `0 + (27 − 20)·230 = 1610`, law `0 − 1500 − 5829 = −7329` → −1610.
/// test_decide.py::test_reservation_not_applied_while_net_laden (Python asserted `< −800`).
#[test]
fn net_laden_skips_the_yield() {
    let b = Brain { flags: HUNGRY, net_laden: true, reserve_surplus: true, soc: 70.0, ..p1(-1500.0) };
    assert_eq!(sp(b), -1610.0);
}

// ------------------------------------------------------------------ a car grid-charging (ev_grid_laden)

/// `ev_grid_laden`: `[0, d]|min`, never discharge into a car that imports on purpose, but still
/// charge real surplus. test_decide.py::test_grid_charging_car_holds_battery /
/// _still_charges_real_surplus / _flag_off_behaves_normally / test_reservation_under_peak_off_by_default.
#[test]
fn grid_laden_holds_instead_of_feeding_the_car() {
    assert_eq!(sp(Brain { flags: GRID_LADEN, soc: 70.0, ..p1(3000.0) }), 0.0);
    assert_eq!(sp(Brain { flags: GRID_LADEN, soc: 70.0, ..p1(-1500.0) }), -1500.0);
    assert_eq!(sp(Brain { soc: 70.0, ..p1(3000.0) }), 3000.0);
    assert_eq!(sp(Brain { flags: GRID_LADEN, soc: 70.0, ..p1(2000.0) }), 0.0);
}

/// `ev_grid_laden` reservation: `resv = −max(0, min(reserve_grid_w, allowed − p1_fast +
/// max(0, −batt)))`, `d = min(d, resv)`. LOGIC.md "Battery reservation" (peak half).
/// Headrooms: 5829 − 2000 = 3829 (full 300); 5829 − 5829 = 0; 5829 − 5679 = 150.
/// test_decide.py::test_reservation_under_peak_charges_the_reserved_watts / _bounded_by_headroom /
/// _partial_headroom.
#[test]
fn grid_laden_reservation_is_bounded_by_the_import_headroom() {
    let r = |x: f64| sp(Brain { flags: GRID_LADEN, reserve_grid_w: 300.0, soc: 70.0, ..p1(x) });
    assert_eq!(r(2000.0), -300.0);
    assert_eq!(r(5829.0), 0.0);
    assert_eq!(r(5679.0), -150.0);
}

/// The battery's current charge is inside `p1_fast`, so the headroom adds it back:
/// `5829 − 5829 + 500 = 500` → holds its 500 W; `5829 − 2000 + 300` → keeps its 300 W.
/// test_decide.py::test_reservation_under_peak_total_import_reaches_the_ceiling / _counts_existing_charge.
#[test]
fn grid_laden_reservation_adds_back_the_existing_charge() {
    let b1 = Brain { flags: GRID_LADEN, reserve_grid_w: 500.0, batt: -500.0, soc: 70.0, ..p1(5829.0) };
    assert_eq!(sp(b1), -500.0);
    let b2 = Brain { flags: GRID_LADEN, reserve_grid_w: 300.0, batt: -300.0, soc: 70.0, ..p1(2000.0) };
    assert_eq!(sp(b2), -300.0);
}

/// `reserve_grid_w > 0 and soc < 100`: no reservation when full.
/// test_decide.py::test_reservation_under_peak_respects_ceiling.
#[test]
fn grid_laden_reservation_stops_at_100() {
    assert_eq!(sp(Brain { flags: GRID_LADEN, reserve_grid_w: 300.0, soc: 100.0, ..p1(2000.0) }), 0.0);
}

/// `ev_grid_laden and not net_laden`: net_laden's law runs instead (−1610 as in
/// `net_laden_skips_the_yield`, with `2000 − 5829` instead). Python asserted only `!= −300`.
/// test_decide.py::test_reservation_under_peak_yields_to_net_laden.
#[test]
fn net_laden_skips_the_grid_laden_hold() {
    let b = Brain { flags: GRID_LADEN, net_laden: true, reserve_grid_w: 300.0, soc: 70.0, ..p1(2000.0) };
    assert_eq!(sp(b), -1610.0);
}

/// The reservation is a floor on charging, not a cap on free solar: base law −1269 − 864 + 200 =
/// −1933 beats the 1300 reservation. Importing: headroom 3969 − 2000 = 1969 → −1300; at
/// p1 3500 headroom 469 → −469; full → 0.
/// test_scenarios.py::test_reservation_does_not_cap_solar_absorption (2026-08-30 regression).
#[test]
fn grid_laden_reservation_does_not_cap_solar_absorption() {
    let g = |batt: f64, x: f64, bias: f64, soc: f64| {
        sp(Brain { flags: GRID_LADEN, reserve_grid_w: 1300.0, batt, bias, soc, allowed: 3969.0, ..p1(x) })
    };
    assert_eq!(g(-1269.0, -864.0, 200.0, 19.0), -1933.0);
    assert_eq!(g(0.0, 2000.0, 0.0, 19.0), -1300.0);
    assert_eq!(g(0.0, 3500.0, 0.0, 19.0), -469.0);
    assert_eq!(g(0.0, 2000.0, 0.0, 100.0), 0.0);
}

/// The effective reservation (`sensor.batterij_reservering_effectief`) takes the leftover
/// headroom: 3525 − 2060 = 1465 ≥ 1056 → −1056; 0 → nothing; 5000 bounded to 3525 − 3400 = 125.
/// test_scenarios.py::test_reserve_grid_takes_leftover_headroom.
#[test]
fn grid_laden_takes_the_effective_reservation() {
    let g = |resv: f64, x: f64, soc: f64| {
        sp(Brain { flags: GRID_LADEN, reserve_grid_w: resv, soc, vloer: 40.0, allowed: 3525.0, ..p1(x) })
    };
    assert_eq!(g(1056.0, 2060.0, 35.0), -1056.0);
    assert_eq!(g(0.0, 2060.0, 35.0), 0.0);
    assert_eq!(g(5000.0, 3400.0, 35.0), -125.0);
    assert_eq!(g(1056.0, 2060.0, 100.0), 0.0);
}

/// Behind a drawing car the battery holds even at 2709 W import; without one it covers it;
/// the floor wins at 13 %; the clamp caps 9000 at 5000.
/// test_scenarios.py::test_deliberate_import_hold_needs_a_drawing_car.
#[test]
fn deliberate_import_hold_needs_a_drawing_car() {
    assert_eq!(sp(Brain { soc: 69.0, ..p1(2709.0) }), 2709.0);
    assert_eq!(sp(Brain { flags: GRID_LADEN, soc: 69.0, ..p1(2709.0) }), 0.0);
    assert_eq!(sp(Brain { soc: 13.0, ..p1(2709.0) }), 0.0);
    assert_eq!(sp(Brain { soc: 69.0, ..p1(9000.0) }), 5000.0);
}

// ------------------------------------------------------------------ cars parked under the peak

/// `ev_grid_parked`: `d = [d, [0, p1_fast − allowed]|max]|min`, discharge only the overshoot.
/// 2710 < 2801 → 0; 2763 − 2735 = 28; 9000 − 2735 = 6265 → clamp 5000; floor at 13 %.
/// test_scenarios.py::test_parked_cars_shave_overshoot_not_baseload.
#[test]
fn parked_cars_shave_only_the_overshoot() {
    let p = |x: f64, allowed: f64, soc: f64| sp(Brain { flags: PARKED, allowed, soc, ..p1(x) });
    assert_eq!(p(2710.0, 2801.0, 69.0), 0.0);
    assert_eq!(p(2763.0, 2735.0, 69.0), 28.0);
    assert_eq!(p(9000.0, 2735.0, 69.0), 5000.0);
    assert_eq!(p(2763.0, 2735.0, 13.0), 0.0);
}

/// `min`, not an assignment: export is still absorbed (−5374 → clamp −5000; −800).
/// test_scenarios.py::test_parked_cars_still_absorb_export (2026-08-30 regression).
#[test]
fn parked_cars_still_absorb_export() {
    let p = |x: f64| sp(Brain { flags: PARKED, allowed: 3969.0, soc: 36.0, ..p1(x) });
    assert_eq!(p(-5374.0), -5000.0);
    assert_eq!(p(-800.0), -800.0);
}

/// The shave bound uses `p1_fast` while the base law uses the median: raw 3000 over an
/// allowance of 2800 bounds a discharge at 200 W. Median 0 → `min(0, 200) = 0`; median 1000 →
/// `min(1000, 200) = 200`.
#[test]
fn parked_shave_reads_the_instantaneous_p1() {
    let p = |median: f64| sp(Brain { flags: PARKED, allowed: 2800.0, p1: median, p1_fast: 3000.0, ..b() });
    assert_eq!(p(0.0), 0.0);
    assert_eq!(p(1000.0), 200.0);
}

/// The parked branch takes the reservation too (live, not in the Python mirror): headroom
/// 5829 − 2000 = 3829 → −300 instead of 0.
#[test]
fn parked_cars_take_the_grid_reservation() {
    assert_eq!(sp(Brain { flags: PARKED, reserve_grid_w: 300.0, ..p1(2000.0) }), -300.0);
    assert_eq!(sp(Brain { flags: PARKED, reserve_grid_w: 300.0, soc: 100.0, ..p1(2000.0) }), 0.0);
}

/// `reserve_grid_w > 0 and soc < 100` (template "Batterij setpoint", parked branch): at
/// exactly 100 % the reservation is skipped, so a parked car's overshoot shave survives the
/// ceiling. P1 6329 W, allowed 5829: shave 500 W. With the reservation the headroom
/// (5829 − 6329 = −500) would pin the setpoint to 0.
#[test]
fn a_full_battery_skips_the_reservation_and_keeps_the_shave() {
    let parked = |soc: f64| sp(Brain { flags: PARKED, reserve_grid_w: 1000.0, soc, ..p1(6329.0) });
    assert_eq!(parked(100.0), 500.0);
    assert_eq!(parked(99.9), 0.0, "below 100 the reservation caps it");
}

/// Template "Batterij setpoint": `net_laden = is_state(batterij_net_laden, 'on') and soc < 100`.
/// At 100 % with the flag still on (until `goedkoop_laden` clears it) `net_laden` is false, so
/// the cascade branches apply and then the ceiling: dump 3000 W; yield to the hungry car
/// (P1 1400) holds at 0 instead of discharging 1400; the grid-laden hold min(0, 2000) = 0; the
/// parked shave min(2763, 2763 − 2735) = 28 W.
#[test]
fn net_laden_at_100_leaves_the_cascade_branches_on() {
    let mut w = world();
    w.solis.soc = Some(100.0);
    let m = Memory { batterij_net_laden: true, ..Memory::default() };
    let b = read(&w, &Settings::default(), &m, &Derived::default());
    assert!(!b.net_laden, "the flag is on, but soc < 100 is not");
    w.solis.soc = Some(99.9);
    assert!(read(&w, &Settings::default(), &m, &Derived::default()).net_laden);

    let full = |flags: Flags, x: f64| sp(Brain { flags, net_laden: b.net_laden, soc: 100.0, allowed: 2735.0, ..p1(x) });
    assert_eq!(full(AUTO, 0.0), 3000.0, "dump");
    assert_eq!(full(HUNGRY, 1400.0), 0.0, "yield");
    assert_eq!(full(GRID_LADEN, 2000.0), 0.0, "hold");
    assert_eq!(full(PARKED, 2763.0), 28.0, "shave");
}

/// `ev_grid_parked and ... not ev_batterij_auto_laden`, pinned with hand-built flags.
/// LIVE: this clause is dead: [`Flags::read`] cannot set both (parked needs no car drawing, the
/// dump needs one).
#[test]
fn parked_shave_yields_to_the_dump() {
    let both = Flags { ev_grid_parked: true, ev_batterij_auto_laden: true, ..Flags::default() };
    assert_eq!(sp(Brain { flags: both, allowed: 2735.0, ..p1(2763.0) }), 3000.0);
}

// ------------------------------------------------------------------ dump into the car (Ontlaad-budget)

/// `{% if ev_batterij_auto_laden and not net_laden %}{% set d = 3000 %}`: a steady 3000 W,
/// independent of P1, blocked by the floor.
/// test_decide.py::test_batterij_auto_forces_full_discharge / _is_p1_independent /
/// _respects_reserve_floor / _flag_off_no_forced_discharge.
#[test]
fn dump_forces_a_steady_3000_w() {
    for x in [0.0, -2000.0, 4000.0] {
        assert_eq!(sp(Brain { flags: AUTO, soc: 70.0, ..p1(x) }), DUMP_W, "{x}");
    }
    assert_eq!(sp(Brain { flags: AUTO, soc: 40.0, ..p1(0.0) }), 0.0);
    assert_eq!(sp(Brain { soc: 70.0, ..p1(-1500.0) }), -1500.0);
}

/// `ev_grid_laden and ... not ev_batterij_auto_laden`: the dump survives the grid-charge hold.
/// test_decide.py::test_batterij_auto_overrides_grid_laden.
#[test]
fn dump_survives_the_grid_laden_hold() {
    let f = Flags { ev_grid_laden: true, ev_batterij_auto_laden: true, ..Flags::default() };
    assert_eq!(sp(Brain { flags: f, soc: 70.0, ..p1(3000.0) }), DUMP_W);
}

/// net_laden wins over the dump: cap 2000 + 1610 = 3610, law −2000 + 0 − 5829 → −3610.
/// test_decide.py::test_batterij_auto_net_laden_takes_precedence (Python asserted `< 0`).
#[test]
fn net_laden_wins_over_the_dump() {
    let b = Brain { flags: AUTO, net_laden: true, soc: 50.0, batt: -2000.0, ..p1(0.0) };
    assert_eq!(sp(b), -3610.0);
}

// ------------------------------------------------------------------ grid-charge to full (net_laden)

/// `phase_cap = [0, [5000, max(0, −batt) + (27 − maxphase)·230]|min]|max`,
/// `d = [−phase_cap, batt + p1_fast − allowed]|max`.
/// 2000 + 7·230 = 3610; 2000 − 3·230 = 1310; 3000 + 17·230 = 6910 → 5000.
/// test_decide.py::test_net_laden_dynamic_phase_cap / _backs_off_when_phase_high /
/// _ramps_to_inverter_max_when_phases_cold; test_scenarios.py::test_netladen_fills_from_grid_phase_limited
/// (2000 + 9·230 = 4070).
#[test]
fn net_laden_is_capped_by_the_worst_phase() {
    let n = |batt: f64, maxphase: f64| sp(Brain { net_laden: true, soc: 50.0, batt, maxphase, ..p1(0.0) });
    assert_eq!(n(-2000.0, 20.0), -3610.0);
    assert_eq!(n(-2000.0, 30.0), -1310.0);
    assert_eq!(n(-3000.0, 10.0), -5000.0);
    assert_eq!(n(-2000.0, 18.0), -4070.0);
}

/// A phase already past 27 A with an idle battery: `0 + (27 − 31)·230 = −920` → cap 0, hold.
#[test]
fn net_laden_phase_cap_never_goes_negative() {
    assert_eq!(sp(Brain { net_laden: true, maxphase: 31.0, ..p1(0.0) }), 0.0);
}

/// The law steers import onto `allowed`: −500 + 6000 − 5829 = −329 (cap 500 + 15·230 = 3950).
/// test_decide.py::test_net_laden_never_charges_past_economic_target /
/// test_budget_absent_falls_back_to_static_law (the fallback itself: `allowed_falls_back_to_target_minus_100`).
#[test]
fn net_laden_steers_import_onto_allowed() {
    let b = Brain { net_laden: true, soc: 50.0, batt: -500.0, maxphase: 12.0, ..p1(6000.0) };
    assert_eq!(sp(b), -329.0);
}

/// A budget credit charges harder one-for-one, a debt backs off (and here even discharges:
/// −500 + 6000 − 4829 = 671), an exhausted budget asks for no import.
/// LOGIC.md "Quarter-hour budget control"; test_decide.py::test_budget_credit_allows_more_charge /
/// _debt_backs_off / _exhausted_stops_importing.
#[test]
fn net_laden_follows_the_quarter_budget() {
    let n =
        |allowed: f64| sp(Brain { net_laden: true, soc: 50.0, batt: -500.0, maxphase: 12.0, allowed, ..p1(6000.0) });
    assert_eq!(n(5829.0), -329.0);
    assert_eq!(n(6829.0), -1329.0);
    assert_eq!(n(4829.0), 671.0);
    assert_eq!(sp(Brain { net_laden: true, soc: 50.0, maxphase: 12.0, allowed: 0.0, ..p1(0.0) }), 0.0);
}

/// net_laden throttles on the instantaneous P1: median 0 → −5000 (cap 2000 + 17·230 → 5000);
/// raw 3000 → −2000 + 3000 − 5829 = −4829. test_decide.py::test_net_laden_fast_p1_throttles_on_import_spike.
#[test]
fn net_laden_throttles_on_the_instantaneous_p1() {
    let n = |fast: f64| {
        sp(Brain { net_laden: true, soc: 50.0, batt: -2000.0, maxphase: 10.0, p1: 0.0, p1_fast: fast, ..b() })
    };
    assert_eq!(n(0.0), -5000.0);
    assert_eq!(n(3000.0), -4829.0);
}

/// `net_laden and soc < 100`: at 100 % the law is off and the ceiling holds.
/// test_decide.py::test_net_laden_stops_at_100; test_scenarios.py::test_netladen_stops_when_full.
#[test]
fn net_laden_stops_at_100() {
    assert_eq!(sp(Brain { net_laden: true, soc: 100.0, ..p1(-2000.0) }), 0.0);
    assert_eq!(sp(Brain { net_laden: true, soc: 100.0, ..p1(0.0) }), 0.0);
}

/// net_laden replaces the yield. test_decide.py::test_net_laden_overrides_yield.
#[test]
fn net_laden_overrides_the_yield() {
    let b = Brain { flags: HUNGRY, battery_bridges: true, net_laden: true, soc: 50.0, batt: -2000.0, ..p1(0.0) };
    assert_eq!(sp(b), -3610.0);
}

// ------------------------------------------------------------------ emergency trickle

/// `soc <= 10`: at least 220 W of charge, whatever else wants (importing, a car on the grid),
/// without throttling a harder charge. test_scenarios.py::test_trickle_charges_a_deeply_drained_pack_whatever_else_wants.
#[test]
fn trickle_charges_a_drained_pack() {
    assert_eq!(sp(Brain { soc: 10.0, ..p1(2763.0) }), -220.0);
    assert_eq!(sp(Brain { soc: 10.0, flags: GRID_LADEN, ..p1(2709.0) }), -220.0);
    assert_eq!(sp(Brain { soc: 10.0, ..p1(-2500.0) }), -2500.0);
}

/// The ON edge: 9 and 10 start it, 11 with an idle battery does not.
/// test_scenarios.py::test_trickle_starts_at_the_on_threshold_not_before.
#[test]
fn trickle_starts_at_10_not_11() {
    assert_eq!(sp(Brain { soc: 9.0, ..p1(2763.0) }), -220.0);
    assert_eq!(sp(Brain { soc: 11.0, ..p1(2763.0) }), 0.0);
}

/// `_trickling and soc < 12`: a battery already charging carries on through 11 and stops at
/// 12. Base law −220 + 2763 = 2543 → −220 at 11, floor → 0 at 12 and 13.
/// test_scenarios.py::test_trickle_keeps_going_through_the_band_once_latched.
#[test]
fn trickle_latches_until_12() {
    let t = |soc: f64| sp(Brain { soc, batt: -220.0, ..p1(2763.0) });
    assert_eq!(t(11.0), -220.0);
    assert_eq!(t(12.0), 0.0);
    assert_eq!(t(13.0), 0.0);
}

/// `_trickling = batt <= -110.0`: −2500 holds the latch harmlessly (−5000 stays), −150 and
/// exactly −110 count, −109 and −50 do not.
/// test_scenarios.py::test_trickle_latch_reads_the_plant_not_a_helper.
#[test]
fn trickle_latch_reads_the_battery_power() {
    assert_eq!(sp(Brain { soc: 11.0, batt: -2500.0, ..p1(-2500.0) }), -5000.0);
    for (batt, want) in [(-150.0, -220.0), (-110.0, -220.0), (-109.0, 0.0), (-50.0, 0.0)] {
        assert_eq!(sp(Brain { soc: 11.0, batt, ..p1(2763.0) }), want, "{batt}");
    }
}

// ------------------------------------------------------------------ validity, clamp, rounding

/// `0 if not valid`: a stale critical sensor holds. test_decide.py::test_invalid_inputs_force_hold /
/// test_valid_inputs_behave_normally.
#[test]
fn invalid_inputs_hold_at_zero() {
    assert_eq!(sp(Brain { valid: false, soc: 70.0, ..p1(2000.0) }), 0.0);
    assert_eq!(sp(Brain { valid: false, soc: 5.0, ..p1(-2000.0) }), 0.0, "even the trickle");
    assert_eq!(sp(p1(-1500.0)), -1500.0);
}

/// `[5000, [-5000, d]|max]|min`. test_decide.py::test_output_always_within_power_limits.
#[test]
fn output_is_clamped_to_5000_both_ways() {
    assert_eq!(sp(p1(9000.0)), 5000.0);
    assert_eq!(sp(p1(-9000.0)), -5000.0);
    for soc in [16.0, 50.0, 96.0] {
        let mut x = -8000.0;
        while x <= 8000.0 {
            let v = sp(Brain { soc, ..p1(x) });
            assert!((-MAX_W..=MAX_W).contains(&v), "{soc} {x} {v}");
            x += 250.0;
        }
    }
}

/// `| round(0)`: ties to even (HA's `round` is Python's), and never a negative zero (HA turns a
/// precision-0 round into an int).
#[test]
fn output_is_rounded_half_to_even_without_negative_zero() {
    assert_eq!(sp(p1(100.5)), 100.0);
    assert_eq!(sp(p1(101.5)), 102.0);
    let z = sp(p1(-0.4));
    assert_eq!(z, 0.0);
    assert!(z.is_sign_positive());
}

/// Every flag combination, SoC and P1: inside the inverter limits, never discharging at or
/// below the floor (40), never charging at 100. test_decide.py::test_full_sweep_invariants
/// (sweeps flags instead of the retired `ev_laadmodus`).
#[test]
fn sweep_respects_limits_floor_and_ceiling() {
    for bits in 0..64u8 {
        let flags = Flags {
            ev_higher_hungry: bits & 1 != 0,
            ev_grid_laden: bits & 2 != 0,
            ev_grid_parked: bits & 4 != 0,
            ev_batterij_auto_laden: bits & 8 != 0,
        };
        for soc in [10.0, 30.0, 41.0, 60.0, 96.0, 99.0, 100.0] {
            for x in [-4000.0, -800.0, 0.0, 800.0, 4000.0] {
                let v = sp(Brain {
                    flags,
                    battery_bridges: bits & 16 != 0,
                    reserve_surplus: bits & 32 != 0,
                    reserve_grid_w: 500.0,
                    soc,
                    ..p1(x)
                });
                assert!((-MAX_W..=MAX_W).contains(&v));
                assert!(!(soc <= 40.0 && v > 0.0), "discharged below floor: {flags:?} {soc} {x} {v}");
                assert!(!(soc >= 100.0 && v < 0.0), "charged above ceiling: {flags:?} {soc} {x} {v}");
            }
        }
    }
}

// ------------------------------------------------------------------ bias (soluna_dispatcher.new_bias)

fn nb(b: Brain) -> f64 {
    next_bias(&b)
}

/// `bias + 0.1 * p1`: 0 + 10 = 10, 10 + 10 = 20, export lowers it.
/// test_bias.py::test_accumulates_slowly_toward_killing_import / test_sign_export_lowers_bias.
#[test]
fn bias_integrates_a_tenth_of_p1() {
    assert_eq!(nb(p1(100.0)), 10.0);
    assert_eq!(nb(Brain { bias: 10.0, ..p1(100.0) }), 20.0);
    assert_eq!(nb(p1(-200.0)), -20.0);
}

/// `(p1|abs) < 20` freezes it; 20 itself integrates; 30 nudges 25 → 28.
/// test_bias.py::test_deadband_freezes_small_residual / test_corrects_residual_above_deadband.
#[test]
fn bias_has_a_20_w_deadband() {
    assert_eq!(nb(Brain { bias: 25.0, ..p1(15.0) }), 25.0);
    assert_eq!(nb(Brain { bias: 37.5, ..p1(0.0) }), 37.5);
    assert_eq!(nb(Brain { bias: 25.0, ..p1(-19.9) }), 25.0);
    assert_eq!(nb(p1(20.0)), 2.0);
    assert_eq!(nb(Brain { bias: 25.0, ..p1(30.0) }), 28.0);
}

/// `[200, [-200, ...]|max]|min`. test_bias.py::test_clamped_to_limit.
#[test]
fn bias_is_clamped_to_200() {
    assert_eq!(nb(Brain { bias: 190.0, ..p1(5000.0) }), 200.0);
    assert_eq!(nb(Brain { bias: -190.0, ..p1(-5000.0) }), -200.0);
}

/// `| round(1)` on both paths: 0.1 · 33.33 = 3.333 → 3.3; a frozen 12.34 → 12.3.
#[test]
fn bias_is_rounded_to_a_tenth() {
    assert_eq!(nb(p1(33.33)), 3.3);
    assert_eq!(nb(Brain { bias: 12.34, valid: false, ..p1(900.0) }), 12.3);
}

/// `track` is false outside plain self-consumption, so the bias is held: invalid, at the floor,
/// full, yielding (with or without bridge), grid-charging, a car grid-charging, dumping.
/// test_bias.py::test_frozen_outside_base_tracking (+ the live `ev_grid_laden` and
/// `ev_batterij_auto_laden` clauses).
#[test]
fn bias_is_frozen_outside_base_tracking() {
    let cases = [
        Brain { valid: false, ..b() },
        Brain { soc: 40.0, ..b() },
        Brain { soc: 100.0, ..b() },
        Brain { flags: HUNGRY, ..b() },
        Brain { flags: HUNGRY, battery_bridges: true, ..b() },
        Brain { net_laden: true, soc: 50.0, ..b() },
        Brain { flags: GRID_LADEN, ..b() },
        Brain { flags: AUTO, ..b() },
    ];
    for c in cases {
        let dbg = format!("{c:?}");
        assert_eq!(nb(Brain { bias: 25.0, p1: 900.0, p1_fast: 900.0, ..c }), 25.0, "{dbg}");
    }
}

/// `soluna_dispatcher.new_bias` `track … and not ev_grid_parked`: with cars parked under the
/// peak the setpoint only shaves and P1 stays non-zero on purpose, so the bias holds at 25
/// (it would otherwise integrate 25 + 0.1 × 900 = 115). A bridge level without a hungry car
/// tracks (test_bias.py::test_tracks_when_no_higher_ev_hungry).
#[test]
fn bias_is_frozen_while_cars_are_parked() {
    assert_eq!(nb(Brain { flags: PARKED, bias: 25.0, ..p1(900.0) }), 25.0);
    assert_eq!(nb(Brain { bias: 25.0, ..p1(900.0) }), 115.0, "not parked: tracks");
    assert!(bias_tracking(&Brain { battery_bridges: true, ..b() }));
    assert!(!bias_tracking(&Brain { net_laden: true, soc: 99.0, ..b() }));
}

// ------------------------------------------------------------------ preamble: Brain::read

fn world() -> World {
    let mut w = World::default();
    w.p1.power_w = Some(1500.0);
    w.solis.soc = Some(60.0);
    w.solis.battery_power_w = Some(0.0);
    w
}

fn noon() -> Ts {
    at(2026, 11, 7, 12, 0, 0)
}

fn read(w: &World, s: &Settings, m: &Memory, d: &Derived) -> Brain {
    Brain::read(noon(), w, s, m, d)
}

/// `p1 = mediaan|float(P1|float(0))`, `p1_fast = P1|float(0)`.
#[test]
fn p1_is_the_median_falling_back_to_the_raw_reading() {
    let (s, m) = (Settings::default(), Memory::default());
    let mut d = Derived::default();
    let b = read(&world(), &s, &m, &d);
    assert_eq!((b.p1, b.p1_fast), (1500.0, 1500.0));
    d.p1_mediaan_w = Some(1200.0);
    let b = read(&world(), &s, &m, &d);
    assert_eq!((b.p1, b.p1_fast), (1200.0, 1500.0));
    let mut w = world();
    w.p1.power_w = None;
    let b = read(&w, &s, &m, &Derived::default());
    assert_eq!((b.p1, b.p1_fast), (0.0, 0.0));
}

/// `valid = has_value(P1) and has_value(soc) and has_value(batt)`; missing values read as 0.
#[test]
fn valid_needs_p1_soc_and_battery_power() {
    let (s, m, d) = (Settings::default(), Memory::default(), Derived::default());
    assert!(read(&world(), &s, &m, &d).valid);
    let mut w = world();
    w.p1.power_w = None;
    assert!(!read(&w, &s, &m, &d).valid);
    let mut w = world();
    w.solis.soc = None;
    let b = read(&w, &s, &m, &d);
    assert!(!b.valid);
    assert_eq!(b.soc, 0.0);
    let mut w = world();
    w.solis.battery_power_w = None;
    let b = read(&w, &s, &m, &d);
    assert!(!b.valid);
    assert_eq!(b.batt, 0.0);
}

/// `vloer = 15 if today_at('06:00') <= now() < today_at('10:00') else reserve`.
/// test_decide.py::test_morning_releases_reserve.
#[test]
fn vloer_is_15_in_the_morning() {
    let s = Settings { batterij_reserve_soc: 40.0, ..Settings::default() };
    let (w, m, d) = (world(), Memory::default(), Derived::default());
    assert_eq!(Brain::read(at(2026, 11, 7, 7, 0, 0), &w, &s, &m, &d).vloer, 15.0);
    assert_eq!(Brain::read(at(2026, 11, 7, 10, 0, 0), &w, &s, &m, &d).vloer, 40.0);
    let mut w35 = world();
    w35.solis.soc = Some(35.0);
    assert_eq!(setpoint(&Brain::read(at(2026, 11, 7, 7, 0, 0), &w35, &s, &m, &d)), 1500.0);
    assert_eq!(setpoint(&Brain::read(noon(), &w35, &s, &m, &d)), 0.0);
}

/// `allowed = kwartier_toegestaan if has_value(...) else maandtarget·1000 − 100`: the budget
/// sensor already carries the margin. LOGIC.md "Quarter-hour budget control";
/// test_decide.py::test_peak_margin_has_one_definition (behavioural half).
#[test]
fn allowed_falls_back_to_target_minus_100() {
    let (w, s, m) = (world(), Settings::default(), Memory::default());
    let mut d = Derived { capaciteit_maandtarget_kw: 2.883, ..Derived::default() };
    assert_eq!(read(&w, &s, &m, &d).allowed, 2783.0);
    d.kwartier_toegestaan_vermogen_w = Some(2783.0);
    assert_eq!(read(&w, &s, &m, &d).allowed, 2783.0);
    d.kwartier_toegestaan_vermogen_w = Some(3100.0);
    assert_eq!(read(&w, &s, &m, &d).allowed, 3100.0);
}

/// Memory, settings and derived inputs land in the right fields.
#[test]
fn read_carries_memory_settings_and_derived() {
    let mut w = world();
    w.p1.phase_a = [Some(-19.0), Some(10.0), None];
    let s = Settings {
        ev_batterij_niveau: BatterijNiveau::Brug6A,
        batterij_reservering_actief: false,
        batterij_reservering_w: 650.0,
        ..Settings::default()
    };
    let m = Memory { batterij_setpoint_bias: -12.5, batterij_net_laden: true, ..Memory::default() };
    let d = Derived { batterij_reservering_effectief_w: 420.0, ..Derived::default() };
    let b = read(&w, &s, &m, &d);
    assert_eq!(b.maxphase, 10.0, "import direction only");
    assert_eq!(b.bias, -12.5);
    assert!(b.net_laden && b.battery_bridges && !b.reserve_surplus);
    assert_eq!(b.reserve_surplus_w, 650.0);
    assert_eq!(b.reserve_grid_w, 420.0);
}

/// Ontlaad-budget without a drawing car is plain self-consumption: 2000 → 2000, 6000 → 5000,
/// the floor holds. test_decide.py::test_batt_auto_discharges_to_cover_car /
/// _discharge_clamped_to_inverter_max / test_batt_auto_respects_reserve_floor.
#[test]
fn dump_level_without_a_drawing_car_is_self_consumption() {
    let s = Settings {
        ev_batterij_niveau: BatterijNiveau::OntlaadBudget,
        batterij_reserve_soc: 40.0,
        ..Settings::default()
    };
    let (m, d) = (Memory::default(), Derived::default());
    let run = |x: f64, soc: f64| {
        let mut w = world();
        w.p1.power_w = Some(x);
        w.solis.soc = Some(soc);
        setpoint(&read(&w, &s, &m, &d))
    };
    assert_eq!(run(2000.0, 70.0), 2000.0);
    assert_eq!(run(6000.0, 70.0), 5000.0);
    assert_eq!(run(2000.0, 40.0), 0.0);
}

/// With no car, every grid × battery level is plain self-consumption.
/// test_decide.py::test_no_hungry_ev_is_plain_self_consumption (levels instead of `ev_laadmodus`).
#[test]
fn every_level_without_a_car_is_self_consumption() {
    let d = Derived { stroomprijs_nu: Some(0.1), pv_bruto_overschot_w: 3000.0, ..Derived::default() };
    for net in [NetNiveau::Uit, NetNiveau::Brug6A, NetNiveau::MaxTotPiek] {
        for bat in [BatterijNiveau::Uit, BatterijNiveau::Brug6A, BatterijNiveau::OntlaadBudget] {
            let s = Settings { ev_net_niveau: net, ev_batterij_niveau: bat, ..Settings::default() };
            for (x, soc) in [(1500.0, 60.0), (-1500.0, 70.0)] {
                let mut w = world();
                w.p1.power_w = Some(x);
                w.solis.soc = Some(soc);
                let b = read(&w, &s, &Memory::default(), &d);
                assert_eq!(b.flags, Flags::default(), "{net:?} {bat:?}");
                assert_eq!(setpoint(&b), x);
            }
        }
    }
}

// ------------------------------------------------------------------ preamble: Flags::read

/// Buzz ranked first, plugged, reachable, not drawing.
fn buzz_world() -> World {
    let mut w = world();
    w.evse.plug = Some("Connected".into());
    w.evse.override_a = Some(10.0);
    w.evse.override_max_a = Some(32.0);
    w
}

fn flags(w: &World, s: &Settings, m: &Memory, d: &Derived) -> Flags {
    Flags::read(w, s, m, d)
}

fn solar(bruto: f64) -> Derived {
    Derived { pv_bruto_overschot_w: bruto, stroomprijs_nu: Some(0.30), ..Derived::default() }
}

fn levels(net: NetNiveau, bat: BatterijNiveau) -> Settings {
    Settings { ev_net_niveau: net, ev_batterij_niveau: bat, ..Settings::default() }
}

/// `_bruto > _yt` with `_yt = 1400` when both levels are `Uit`, else 200 (strictly above).
/// LOGIC.md "The surplus cascade", bootstrap.
#[test]
fn hungry_needs_gross_surplus_above_the_yield_threshold() {
    let m = Memory::default();
    let uit = levels(NetNiveau::Uit, BatterijNiveau::Uit);
    assert!(flags(&buzz_world(), &uit, &m, &solar(1401.0)).ev_higher_hungry);
    assert!(!flags(&buzz_world(), &uit, &m, &solar(1400.0)).ev_higher_hungry);
    // `Max tot piek` at a dear price: not grid_max, but the threshold drops to 200.
    let dear = Derived { stroomprijs_nu: Some(0.50), ..solar(201.0) };
    let max = levels(NetNiveau::MaxTotPiek, BatterijNiveau::Uit);
    assert!(flags(&buzz_world(), &max, &m, &dear).ev_higher_hungry);
    let brug = levels(NetNiveau::Uit, BatterijNiveau::Brug6A);
    assert!(flags(&buzz_world(), &brug, &m, &solar(201.0)).ev_higher_hungry);
    assert!(!flags(&buzz_world(), &brug, &m, &solar(200.0)).ev_higher_hungry);
}

/// `_solar_mode = not grid_max and not batt_dump`: no yield under a cheap `Max tot piek` or in
/// `Ontlaad-budget`.
#[test]
fn hungry_only_in_solar_mode() {
    let m = Memory::default();
    let cheap = Derived { stroomprijs_nu: Some(0.10), ..solar(3000.0) };
    let max = levels(NetNiveau::MaxTotPiek, BatterijNiveau::Uit);
    assert!(!flags(&buzz_world(), &max, &m, &cheap).ev_higher_hungry);
    let dump = levels(NetNiveau::Uit, BatterijNiveau::OntlaadBudget);
    assert!(!flags(&buzz_world(), &dump, &m, &solar(3000.0)).ev_higher_hungry);
}

/// `and not _stalled`: `ev_hoger_laadstil` releases the battery. LOGIC.md "Cascade-yield liveness".
#[test]
fn a_stalled_higher_car_does_not_make_the_battery_yield() {
    let s = levels(NetNiveau::Uit, BatterijNiveau::Uit);
    let m = Memory { ev_hoger_laadstil: true, ..Memory::default() };
    assert!(!flags(&buzz_world(), &s, &m, &solar(3000.0)).ev_higher_hungry);
}

/// `_buzz_elig`: ranked above the battery, wants (plug or pilot B/C, latch fallback),
/// reachable, and not within 1.5 A of its max while charging. LOGIC.md "Unavailable components
/// are transparent".
#[test]
fn buzz_eligibility() {
    let s = levels(NetNiveau::Uit, BatterijNiveau::Uit);
    let m = Memory::default();
    let d = solar(3000.0);
    let hungry = |w: &World, s: &Settings, m: &Memory| flags(w, s, m, &d).ev_higher_hungry;

    let mut w = buzz_world();
    w.evse.state = Some("Charging".into());
    w.evse.override_a = Some(30.5);
    assert!(!hungry(&w, &s, &m), "30.5 ≥ 32 − 1.5: maxed");
    w.evse.override_a = Some(30.4);
    assert!(hungry(&w, &s, &m));
    w.evse.state = Some("Connected to EV".into());
    w.evse.override_a = Some(31.0);
    assert!(hungry(&w, &s, &m), "not drawing: never maxed");

    let mut w = buzz_world();
    w.evse.override_a = None;
    assert!(!hungry(&w, &s, &m), "charger unreachable");

    let mut w = buzz_world();
    w.evse.plug = Some("unavailable".into());
    assert!(!hungry(&w, &s, &m), "no latch");
    let latched = Memory { smartevse_plug_laatst_bekend: "Connected".into(), ..Memory::default() };
    assert!(hungry(&w, &s, &latched), "latch");

    let low = Settings { zonprioriteit: Zonprioriteit([Consumer::Ev3, Consumer::Batterij, Consumer::Buzz]), ..s };
    assert!(!hungry(&buzz_world(), &low, &m), "ranked below the battery");
}

/// `_ev3_elig`: ranked above the battery, wants (cable status or car plug), reachable, and not
/// drawing (> 0.2 kW) at 15 A or more.
#[test]
fn ev3_eligibility() {
    let s = Settings {
        zonprioriteit: Zonprioriteit([Consumer::Ev3, Consumer::Batterij, Consumer::Buzz]),
        ..levels(NetNiveau::Uit, BatterijNiveau::Uit)
    };
    let m = Memory::default();
    let d = solar(3000.0);
    let mut w = world();
    w.cable.status = Some("plugged_in".into());
    w.cable.set_current_a = Some(15.0);
    assert!(flags(&w, &s, &m, &d).ev_higher_hungry);
    w.cable.power_kw = Some(0.2);
    assert!(flags(&w, &s, &m, &d).ev_higher_hungry, "0.2 kW is not drawing");
    w.cable.power_kw = Some(0.3);
    assert!(!flags(&w, &s, &m, &d).ev_higher_hungry, "drawing at 15 A: maxed");
    w.cable.set_current_a = Some(14.0);
    assert!(flags(&w, &s, &m, &d).ev_higher_hungry);
    w.cable.set_current_a = None;
    assert!(!flags(&w, &s, &m, &d).ev_higher_hungry, "unreachable");
    let low = Settings { zonprioriteit: Zonprioriteit([Consumer::Batterij, Consumer::Ev3, Consumer::Buzz]), ..s };
    w.cable.set_current_a = Some(10.0);
    assert!(!flags(&w, &low, &m, &d).ev_higher_hungry, "battery first");
}

/// `ev_grid_laden` / `ev_grid_parked`: cheap `Max tot piek`, a car wants; drawing or not.
/// Neither when dear, or when the drawing car does not "want" (cable says `available`).
#[test]
fn grid_flags_split_on_a_drawing_car() {
    let s = levels(NetNiveau::MaxTotPiek, BatterijNiveau::Uit);
    let m = Memory::default();
    let cheap = Derived { stroomprijs_nu: Some(0.20), ..Derived::default() };
    let mut w = buzz_world();
    let f = flags(&w, &s, &m, &cheap);
    assert!(f.ev_grid_parked && !f.ev_grid_laden);
    w.evse.state = Some("Charging".into());
    let f = flags(&w, &s, &m, &cheap);
    assert!(f.ev_grid_laden && !f.ev_grid_parked);
    let dear = Derived { stroomprijs_nu: Some(0.38), ..Derived::default() };
    assert_eq!(flags(&w, &s, &m, &dear), Flags::default(), "0.38 is not below 0.38");

    let mut w = world();
    w.cable.status = Some("available".into());
    w.cable.power_kw = Some(1.0);
    assert_eq!(flags(&w, &s, &m, &cheap), Flags::default(), "drawing but not wanting");
    w.cable.status = Some("charging".into());
    assert!(flags(&w, &s, &m, &cheap).ev_grid_laden, "EV3 drawing");
}

/// `ev_batterij_auto_laden = batt_dump and (_ev3_drawing or _buzz_drawing)`.
#[test]
fn dump_flag_needs_a_drawing_car() {
    let s = levels(NetNiveau::Uit, BatterijNiveau::OntlaadBudget);
    let (m, d) = (Memory::default(), Derived::default());
    let mut w = world();
    assert!(!flags(&w, &s, &m, &d).ev_batterij_auto_laden);
    w.cable.power_kw = Some(3.0);
    assert!(flags(&w, &s, &m, &d).ev_batterij_auto_laden);
    let mut w = world();
    w.evse.state = Some("Charging".into());
    assert!(flags(&w, &s, &m, &d).ev_batterij_auto_laden);
    assert!(!flags(&w, &levels(NetNiveau::Uit, BatterijNiveau::Brug6A), &m, &d).ev_batterij_auto_laden);
}

// ------------------------------------------------------------------ off-target and fill

fn off(now: Ts, w: &World, s: &Settings, sp: f64) -> bool {
    off_target(now, w, s, &Derived { batterij_setpoint_w: sp, ..Derived::default() })
}

/// Template "Dispatcher off-target": `|batt − sp| > 800`, dispatcher on, valid, and
/// `vloer + 2 < soc < 95`.
#[test]
fn off_target_when_more_than_800_w_away_in_the_followable_band() {
    let s = Settings { batterij_reserve_soc: 40.0, ..Settings::default() };
    let mut w = world();
    w.solis.battery_power_w = Some(1000.0);
    assert!(off(noon(), &w, &s, 0.0));
    assert!(!off(noon(), &w, &s, 200.0), "exactly 800");
    assert!(off(noon(), &w, &s, -1000.0));
    assert!(!off(noon(), &w, &Settings { soluna_absorptie_actief: false, ..s.clone() }, 0.0));
    w.solis.soc = Some(95.0);
    assert!(!off(noon(), &w, &s, 0.0), "BMS taper");
    w.solis.soc = Some(94.0);
    assert!(off(noon(), &w, &s, 0.0));
    w.solis.soc = Some(42.0);
    assert!(!off(noon(), &w, &s, 0.0), "vloer + 2");
    w.solis.soc = Some(42.5);
    assert!(off(noon(), &w, &s, 0.0));
    w.solis.soc = Some(30.0);
    assert!(off(at(2026, 11, 7, 7, 0, 0), &w, &s, 0.0), "morning floor 15");
    assert!(!off(noon(), &w, &s, 0.0));
}

/// `valid` needs the battery power and the SoC (the SoC default 50 never matters then).
#[test]
fn off_target_needs_battery_readings() {
    let s = Settings::default();
    let mut w = world();
    w.solis.battery_power_w = None;
    assert!(!off(noon(), &w, &s, 1000.0));
    let mut w = world();
    w.solis.soc = None;
    w.solis.battery_power_w = Some(1000.0);
    assert!(!off(noon(), &w, &s, 0.0));
}

/// `fill` writes the setpoint and evaluates off-target against that same setpoint: P1 1500 W
/// with an idle battery → setpoint 1500, |0 − 1500| > 800 → off-target.
#[test]
fn fill_writes_setpoint_then_off_target() {
    let mut d = Derived::default();
    fill(noon(), &world(), &Settings::default(), &Memory::default(), &mut d);
    assert_eq!(d.batterij_setpoint_w, 1500.0);
    assert!(d.dispatcher_off_target);
}

// ------------------------------------------------------------------ HA state formatting

/// Integer-typed states print without a decimal; fractions as Python floats.
#[test]
fn num_states_print_like_ha() {
    assert_eq!(fmt_num_state(Some(3066.0)), "3066");
    assert_eq!(fmt_num_state(Some(-0.0)), "0");
    assert_eq!(fmt_num_state(Some(-1500.0)), "-1500");
    assert_eq!(fmt_num_state(Some(52.5)), "52.5");
    assert_eq!(fmt_num_state(Some(1e17)), "1e+17");
    // From 1e16 on a whole number prints like the Python float it then is (`1e+16`).
    assert_eq!(fmt_num_state(Some(1e16)), "1e+16");
    assert_eq!(fmt_num_state(Some(9e15)), "9000000000000000");
    assert_eq!(fmt_num_state(None), "unavailable");
}

/// Float-typed states print as Python's `repr(float)`.
#[test]
fn float_states_print_like_python() {
    assert_eq!(fmt_float_state(Some(3066.0)), "3066.0");
    assert_eq!(fmt_float_state(Some(0.0)), "0.0");
    assert_eq!(fmt_float_state(Some(0.21152)), "0.21152");
    assert_eq!(fmt_float_state(Some(-0.0123)), "-0.0123");
    assert_eq!(fmt_float_state(Some(0.0001)), "0.0001");
    assert_eq!(fmt_float_state(Some(-5e-5)), "-5e-05");
    assert_eq!(fmt_float_state(Some(1.5e-7)), "1.5e-07");
    assert_eq!(fmt_float_state(Some(1e16)), "1e+16");
    assert_eq!(fmt_float_state(None), "unavailable");
}

#[test]
fn rc_mode_prints_the_select_option() {
    assert_eq!(fmt_rc_mode(None), "unavailable");
    assert_eq!(fmt_rc_mode(Some(RcMode::None)), "None");
    assert_eq!(fmt_rc_mode(Some(RcMode::ForceCharge)), "Solis RC Force Battery Charge");
    assert_eq!(fmt_rc_mode(Some(RcMode::ForceDischarge)), "Solis RC Force Battery Discharge");
}
