"""Narrative scenario spec for the battery brain in the solar-surplus cascade.

These read as the *behaviour spec*: each test is a named situation with the expected battery
setpoint. They complement test_decide.py (which checks the rules) by walking the worked example
from the design — order EV3 -> Batterij -> Buzz, sun rising — step by step.

Reminder of the brain's cascade role (decide.decide_setpoint):
  - base: d = battery_power + p1 + bias  (drive grid net -> 0)
  - while a HIGHER-priority EV is hungry (ev_higher_hungry):
        van batterij -> d = max(0, d)   (yield surplus, may feed the bridge)
        van net      -> d = min(0, d)   (charge leftover export, never feed the car)
  - else full self-consumption (mops up the remainder)
  + reserve floor / 100% ceiling / net_laden override.
"""

from __future__ import annotations

from dispatcher.decide import MAX_DISCHARGE_W, decide_setpoint, Inputs


def s(**kw) -> int:
    base = dict(
        battery_power_net=0.0, p1=0.0, soc=60.0, is_ochtend=False,
        ev_laadmodus="Uit", ev_higher_hungry=False, net_laden=False,
        maandtarget_kw=5.929, maxphase_a=15.0, bias=0.0,
    )
    base.update(kw)
    return decide_setpoint(Inputs(**base))


# ---- worked example: EV3 -> Batterij -> Buzz, sun rising (van net) ----------
# Step 1: faint sun. EV3 (top) holds its 6A bridge importing ~2A; battery yields (van net):
#   it must NOT discharge to feed the car, and there's no export -> hold.
def test_step1_faint_sun_battery_holds_while_ev3_bridges():
    assert s(ev_laadmodus="6A — van net", ev_higher_hungry=True, p1=460) == 0


# Step 2: sun climbs, EV3 ramps and consumes it; grid ~0 -> battery still holds.
def test_step2_ev3_consumes_battery_holds():
    assert s(ev_laadmodus="6A — van net", ev_higher_hungry=True, p1=0) == 0


# Step 2b: while EV3 is still ramping (hungry), the battery HOLDS — it yields the surplus so
# the EV can ramp into it, rather than grabbing it.
def test_step2b_battery_holds_while_ev_ramps():
    assert s(ev_laadmodus="6A — van net", ev_higher_hungry=True, p1=-700, soc=70) == 0


# Step 3: sun exceeds EV3's max -> EV3 satisfied (no longer hungry) -> battery charges surplus.
def test_step3_ev3_satisfied_battery_charges_surplus():
    assert s(ev_laadmodus="6A — van net", ev_higher_hungry=False, p1=-2500, soc=70) == -2500


# Step 4: battery full -> ceiling stops it (export then flows to the next consumer, the Buzz).
def test_step4_battery_full_stops_charging():
    assert s(ev_laadmodus="6A — van net", ev_higher_hungry=False, p1=-2500, soc=100) == 0


# ---- van batterij variant: battery feeds the bridge -------------------------
def test_van_batterij_feeds_bridge_then_yields_surplus():
    # bridge deficit imported (p1=+1400) -> battery discharges to cover it
    assert s(battery_bridges=True, ev_higher_hungry=True, p1=1400) == 1400
    # but real export while the car ramps -> battery yields it (does not grab)
    assert s(battery_bridges=True, ev_higher_hungry=True, p1=-1500, soc=70) == 0


# ---- battery ranked first (Batterij -> EV3): no EV is "higher" --------------
# ev_higher_hungry is False (battery outranks the cars) -> full self-consumption, battery first.
def test_battery_first_charges_before_cars():
    assert s(ev_laadmodus="6A — van net", ev_higher_hungry=False, p1=-3000, soc=60) == -3000


# ---- evening peak: battery covers the house (net_laden off) -----------------
def test_evening_battery_covers_house():
    assert s(ev_laadmodus="Uit", p1=900, soc=80) == 900


# ---- cheap-window grid charge to full (net_laden) ---------------------------
def test_netladen_fills_from_grid_phase_limited():
    # charging 2000, worst phase 18A, target 27 -> cap 2000+(27-18)*230 = 4070
    assert s(net_laden=True, soc=50, battery_power_net=-2000, p1=0, maxphase_a=18) == -4070


def test_netladen_stops_when_full():
    assert s(net_laden=True, soc=100, p1=0) == 0


# ---- no car drawing: the deliberate-import hold must not apply --------------
# Regression for 2026-08-28. `ev_grid_laden` is documented as "mode set + a car actually drawing",
# but the live preamble had drifted to intent-only, so merely having a car PLUGGED pinned the
# battery at 0. The PARKED case is covered separately below with bounded peak-shaving; this full
# self-consumption law is the no-car-plugged case.
def test_deliberate_import_hold_needs_a_drawing_car():
    # nothing plugged -> ordinary self-consumption, battery covers the house.
    assert s(ev_grid_laden=False, p1=2709, soc=69) == 2709
    # a car actually drawing -> its import is deliberate -> battery must NOT feed it.
    assert s(ev_grid_laden=True, p1=2709, soc=69) == 0
    # ...and the discharge floor still wins over shaving. soc is 13, not 10: below TRICKLE_SOC the
    # emergency trickle would add a 220 W CHARGE and stop this asserting what it means to assert.
    assert s(ev_grid_laden=False, p1=2709, soc=13) == 0
    # bounded by MAX_DISCHARGE_W, never more. 5000 since 2026-09-01 — the old 3000 was an
    # unexplained first-commit value and the battery was measurably sitting on it.
    assert s(ev_grid_laden=False, p1=9000, soc=69) == MAX_DISCHARGE_W


# ---- cars parked: shave the OVERSHOOT only, never the whole baseload -------
# The first version of this fix let the parked case fall through to full self-consumption, which
# would have drained the battery covering a ~2.7 kW night baseload for no peak benefit. Peak-shaving
# is bounded by the overshoot, so a projection already under target asks for nothing.
def test_parked_cars_shave_overshoot_not_baseload():
    # p1 under the allowance -> nothing to shave, battery stays put.
    assert s(ev_grid_parked=True, p1=2710, allowed_w=2801, soc=69) == 0
    # p1 over the allowance -> discharge exactly the excess (2763 - 2735).
    assert s(ev_grid_parked=True, p1=2763, allowed_w=2735, soc=69) == 28
    # a big overshoot is still capped by MAX_DISCHARGE_W.
    assert s(ev_grid_parked=True, p1=9000, allowed_w=2735, soc=69) == MAX_DISCHARGE_W
    # below the floor, no discharge at all (soc 13: see the TRICKLE_SOC note above).
    assert s(ev_grid_parked=True, p1=2763, allowed_w=2735, soc=13) == 0


# ---- reservation must not cap free solar absorption -------------------------
# Regression for 2026-08-30: while a car grid-charged, the reservation ASSIGNED the setpoint instead
# of bounding it, pinning the battery at the reservation. Result was 45 min of export up to 1.8 kW
# with the battery at 15-19 % SoC.
def test_reservation_does_not_cap_solar_absorption():
    # exporting 864 W with the battery already taking 1269: base law wants 1933 -> absorb it all,
    # not just the 1300 reservation.
    assert s(ev_grid_laden=True, reserve_grid_w=1300,
             battery_power_net=-1269, p1=-864, bias=200, soc=19, allowed_w=3969) == -1933
    # no export (importing): the reservation slice still applies exactly as designed.
    assert s(ev_grid_laden=True, reserve_grid_w=1300,
             battery_power_net=0, p1=2000, soc=19, allowed_w=3969) == -1300
    # reservation still bounded by the remaining import headroom.
    assert s(ev_grid_laden=True, reserve_grid_w=1300,
             battery_power_net=0, p1=3500, soc=19, allowed_w=3969) == -469
    # and never charges above the ceiling.
    assert s(ev_grid_laden=True, reserve_grid_w=1300,
             battery_power_net=0, p1=2000, soc=100, allowed_w=3969) == 0


# ---- reservation as a FLOOR that climbs to absorb import headroom ----------
# 2026-08-30: with the car pinned at its 16 A cable max, 1.3-3.0 kW of peak headroom sat unused
# because the battery could only ever take the manual reservation (and its boolean was off).
# reserve_grid_w is now computed outside as max(manual, allowed - load_nonsteerable - car_hard_max)
# and read by both loops, so the car climbs to its maximum and the battery takes the remainder.
def test_reserve_grid_takes_leftover_headroom():
    # 1056 W of leftover after the car's 16 A max -> battery takes it (importing, base law holds at 0)
    assert s(ev_grid_laden=True, reserve_grid_w=1056, battery_power_net=0, p1=2060,
             soc=35, allowed_w=3525) == -1056
    # zero leftover -> battery takes nothing, car keeps the whole allowance
    assert s(ev_grid_laden=True, reserve_grid_w=0, battery_power_net=0, p1=2060,
             soc=35, allowed_w=3525) == 0
    # still bounded by the REMAINING measured headroom, so it cannot push past the ceiling
    assert s(ev_grid_laden=True, reserve_grid_w=5000, battery_power_net=0, p1=3400,
             soc=35, allowed_w=3525) == -125
    # and the full-SoC ceiling still wins
    assert s(ev_grid_laden=True, reserve_grid_w=1056, battery_power_net=0, p1=2060,
             soc=100, allowed_w=3525) == 0


# ---- parked cars must still absorb EXPORT, not just shave overshoot -------
# Regression for 2026-08-30: the ev_grid_parked branch ASSIGNED the overshoot, so while exporting
# (overshoot 0) the battery was forced to 0 -- 5.4 kW going to the grid at 36 % SoC.
def test_parked_cars_still_absorb_export():
    # exporting 5374 W -> base law charges it; the overshoot term must not veto that
    assert s(ev_grid_parked=True, battery_power_net=0, p1=-5374, bias=0,
             soc=36, allowed_w=3969) == -5000          # clamped by MAX_CHARGE_W
    # exporting modestly -> absorb exactly the export
    assert s(ev_grid_parked=True, battery_power_net=0, p1=-800, bias=0,
             soc=36, allowed_w=3969) == -800
    # importing under the allowance -> nothing to do
    assert s(ev_grid_parked=True, battery_power_net=0, p1=2710, bias=0,
             soc=36, allowed_w=2801) == 0
    # importing over the allowance -> shave the overshoot only, never full self-consumption.
    # soc must clear the discharge floor (FLOOR_SOC_RESERVE), else the floor correctly wins -- at
    # 36 % it does not, which is why this uses 70 %.
    assert s(ev_grid_parked=True, battery_power_net=0, p1=2763, bias=0,
             soc=70, allowed_w=2735) == 28
    # ...and below the floor the floor wins even with an overshoot (soc 13, above TRICKLE_SOC).
    assert s(ev_grid_parked=True, battery_power_net=0, p1=2763, bias=0,
             soc=13, allowed_w=2735) == 0


# ---- emergency trickle below TRICKLE_SOC ------------------------------------
# Reconstructed from the LIVE template on 2026-09-01: it had been hand-added in HA and was in
# neither decide.py nor live_template.py, so the next build_template.py run would have deleted it
# silently. These pin it so that can't happen again.
def test_trickle_charges_a_deeply_drained_pack_whatever_else_wants():
    # below the discharge floor AND importing hard: without the trickle this holds at 0.
    assert s(p1=2763, soc=10) == -220
    # a car deliberately grid-charging does not get to starve it either.
    assert s(ev_grid_laden=True, p1=2709, soc=10) == -220
    # already charging harder than the trickle -> the trickle must not throttle it back.
    assert s(p1=-2500, soc=10) == -2500


# ---- the hysteresis band, pinned from both sides -----------------------------
# Regression for 2026-09-01: a single `soc < 12` edge pumped. The trickle lifted the SoC a point,
# stopped, the reading fell back and it re-fired -- observed 9->10->11->10->11->12 over 90 min with
# three separate firings. The band must latch, so these assert BOTH edges and the middle.
def test_trickle_starts_at_the_on_threshold_not_before():
    # 10 % or below starts it, idle battery (no latch yet).
    assert s(p1=2763, soc=10, battery_power_net=0) == -220
    assert s(p1=2763, soc=9, battery_power_net=0) == -220
    # 11 % with an idle battery must NOT start it -- that is the pumping this replaced.
    assert s(p1=2763, soc=11, battery_power_net=0) == 0


def test_trickle_keeps_going_through_the_band_once_latched():
    # already taking the trickle -> carry on above the ON threshold...
    assert s(p1=2763, soc=11, battery_power_net=-220) == -220
    # ...but stop at the OFF threshold even while still charging.
    assert s(p1=2763, soc=12, battery_power_net=-220) == 0
    assert s(p1=2763, soc=13, battery_power_net=-220) == 0


def test_trickle_latch_reads_the_plant_not_a_helper():
    # a battery merely charging on solar is enough to hold the latch, which is harmless: the
    # trickle is a min() and cannot throttle a harder charge. (base law: batt + p1 = -5000)
    assert s(p1=-2500, soc=11, battery_power_net=-2500) == -5000
    # half the trickle counts as "trickling" (sensor lag tolerance); a trickle of ~0 does not.
    assert s(p1=2763, soc=11, battery_power_net=-150) == -220
    assert s(p1=2763, soc=11, battery_power_net=-50) == 0
