"""Behavioural tests for the battery decision module (cascade-yield model)."""

from __future__ import annotations

import itertools

import pytest

from dispatcher.decide import (
    BATTERIJ_NAAR_AUTO_W,
    MAX_CHARGE_W,
    MAX_DISCHARGE_W,
    NET_LADEN_MARGIN_W,
    PEAK_GUARD_MARGIN_W,
    Inputs,
    charge_w,
    _allowed_setpoint,
    decide_setpoint,
    discharge_w,
)

MODES = ["Uit", "Zon only", "6A — van net", "6A — van batterij", "Max onder piek", "Batterij → auto"]


def make(**kw) -> Inputs:
    """A neutral midday idle state; override fields per test."""
    base = dict(
        battery_power_net=0.0,
        p1=0.0,
        soc=60.0,
        is_ochtend=False,
        ev_laadmodus="Uit",
        ev_higher_hungry=False,
        net_laden=False,
        maandtarget_kw=5.929,
        maxphase_a=20.0,
    )
    base.update(kw)
    return Inputs(**base)


# ---------------------------------------------------------------- base self-consumption
def test_export_charges():
    assert decide_setpoint(make(p1=-1500, battery_power_net=0)) == -1500


def test_import_discharges():
    assert decide_setpoint(make(p1=800, battery_power_net=0)) == 800


def test_balanced_holds():
    assert decide_setpoint(make(p1=0, battery_power_net=0)) == 0


def test_tracking_uses_current_battery():
    assert decide_setpoint(make(battery_power_net=-2000, p1=-500)) == -2500


@pytest.mark.parametrize("p1", [-100, -500, -1500, -3000, -6000])
def test_never_discharge_while_exporting(p1):
    assert decide_setpoint(make(p1=p1, battery_power_net=0, soc=70)) <= 0


# ---------------------------------------------------------------- floor / reserve / ceiling
def test_reserve_floor_blocks_discharge():
    assert decide_setpoint(make(soc=40, p1=1500)) == 0


def test_below_reserve_no_discharge():
    assert decide_setpoint(make(soc=35, p1=1500)) == 0


def test_morning_releases_reserve():
    assert decide_setpoint(make(soc=35, p1=1500, is_ochtend=True)) == 1500


def test_editable_reserve_lowered_allows_discharge():
    # default 40% floor blocks discharge at SoC 38 ...
    assert decide_setpoint(make(soc=38, p1=1500)) == 0
    # ... but lowering the reserve to 10% releases it (evening peak: use it now)
    assert decide_setpoint(make(soc=38, p1=1500, reserve_soc=10)) == 1500


def test_editable_reserve_raised_holds_more():
    # raising the reserve to 60% blocks discharge at SoC 55
    assert decide_setpoint(make(soc=55, p1=1500, reserve_soc=60)) == 0


def test_morning_floor_still_15():
    assert decide_setpoint(make(soc=15, p1=1500, is_ochtend=True)) == 0
    assert decide_setpoint(make(soc=16, p1=1500, is_ochtend=True)) == 1500


def test_top_blocks_charge_at_100():
    assert decide_setpoint(make(soc=100, p1=-2000)) == 0


def test_charges_up_to_100():
    assert decide_setpoint(make(soc=96, p1=-2000)) == -2000
    assert decide_setpoint(make(soc=99, p1=-2000)) == -2000


# ---------------------------------------------------------------- cascade yield to a hungry EV
# When NO higher EV is hungry the battery does plain self-consumption regardless of mode.
@pytest.mark.parametrize("mode", MODES)
def test_no_hungry_ev_is_plain_self_consumption(mode):
    assert decide_setpoint(make(ev_laadmodus=mode, ev_higher_hungry=False, p1=1500)) == 1500
    assert decide_setpoint(make(ev_laadmodus=mode, ev_higher_hungry=False, p1=-1500, soc=70)) == -1500


# battery bridge (v2 ev_batterij_niveau='6A-brug') + hungry EV: yield surplus, discharge the bridge.
def test_batterij_feeds_bridge():
    # car pulling its 6A bridge from grid (p1=+1400) → battery discharges to cover it
    assert decide_setpoint(
        make(battery_bridges=True, ev_higher_hungry=True, p1=1400)) == 1400


def test_batterij_yields_surplus_not_charging():
    # solar exporting 1500 while a higher EV is ramping → battery does NOT grab it (yields)
    assert decide_setpoint(
        make(battery_bridges=True, ev_higher_hungry=True, p1=-1500, soc=70)) == 0


def test_batterij_bridge_respects_floor():
    assert decide_setpoint(
        make(battery_bridges=True, ev_higher_hungry=True, p1=1400, soc=40)) == 0


# 'van net' + hungry EV: charge leftover export, never discharge to feed the car.
def test_net_does_not_feed_car():
    # car pulling its bridge from grid (p1=+1400) → battery holds (grid covers it)
    assert decide_setpoint(
        make(ev_laadmodus="6A — van net", ev_higher_hungry=True, p1=1400)) == 0


def test_net_holds_while_ev_ramping():
    # EV still ramping (eligible) + export → battery HOLDS (yields the surplus to the EV)
    assert decide_setpoint(
        make(ev_laadmodus="6A — van net", ev_higher_hungry=True, p1=-1500, soc=70)) == 0


def test_net_mops_up_when_ev_maxed():
    # EV maxed (no longer hungry) + export remains → battery charges the leftover
    assert decide_setpoint(
        make(ev_laadmodus="6A — van net", ev_higher_hungry=False, p1=-1500, soc=70)) == -1500


# ------------------------------------------------- battery reservation of the surplus (opt-in)
def test_reservation_off_by_default_still_holds():
    # Default (toggle off) must be byte-identical to the old behaviour: hold at 0 while yielding.
    assert decide_setpoint(make(ev_higher_hungry=True, p1=-1500, soc=70)) == 0


def test_reservation_takes_leftover_up_to_reservation():
    # 1500 W of leftover export, 800 W cap -> charge exactly the cap, not the whole surplus
    # (the car keeps ramping into the rest).
    assert decide_setpoint(
        make(ev_higher_hungry=True, p1=-1500, soc=70,
             reserve_surplus=True, reserve_surplus_w=800)) == -800


def test_reservation_below_amount_takes_only_what_is_there():
    # Only 300 W left over -> take 300 W, not the full cap (never invent import).
    assert decide_setpoint(
        make(ev_higher_hungry=True, p1=-300, soc=70,
             reserve_surplus=True, reserve_surplus_w=800)) == -300


def test_reservation_never_discharges():
    # Car pulling its bridge from the grid (import) -> the reservation must NOT
    # discharge to cover it.
    assert decide_setpoint(
        make(ev_higher_hungry=True, p1=1400, soc=70,
             reserve_surplus=True, reserve_surplus_w=800)) == 0


def test_reservation_respects_charge_ceiling():
    # At 100 % SoC the ceiling wins over the reservation.
    assert decide_setpoint(
        make(ev_higher_hungry=True, p1=-1500, soc=100,
             reserve_surplus=True, reserve_surplus_w=800)) == 0


def test_reservation_yields_to_battery_bridge():
    # battery_bridges (feed the car's 6 A bridge) outranks the reservation:
    # that branch may discharge.
    assert decide_setpoint(
        make(ev_higher_hungry=True, battery_bridges=True, p1=1400, soc=70,
             reserve_surplus=True, reserve_surplus_w=800)) == 1400


def test_reservation_not_applied_while_net_laden():
    # net_laden owns the battery; the yield branch (and so the reservation) is skipped entirely.
    out = decide_setpoint(make(ev_higher_hungry=True, p1=-1500, soc=70, net_laden=True,
                               reserve_surplus=True, reserve_surplus_w=800))
    assert out < -800  # the net_laden law, not the reservation


# ------------------------------------------- reservation while a car grid-charges under the peak
def test_reservation_under_peak_charges_the_reserved_watts():
    # Car grid-charging, importing 2000 W against a 5929 W peak target -> plenty of headroom, so the
    # battery takes the full reservation. The car yields the same watts, so import is unchanged.
    assert decide_setpoint(
        make(ev_grid_laden=True, p1=2000, soc=70,
             reserve_grid_w=300)) == -300


def test_reservation_under_peak_bounded_by_headroom():
    # Import already at the ceiling (5929 - 200 = 5729) -> no headroom left, so take nothing rather
    # than pushing the month peak. ev_grid_laden is based on intent, not on the
    # car actually drawing.
    assert decide_setpoint(
        make(ev_grid_laden=True, p1=5929 - NET_LADEN_MARGIN_W, soc=70,
             reserve_grid_w=300)) == 0


def test_reservation_under_peak_partial_headroom():
    # Only 150 W of headroom left -> take 150, not the full 300.
    assert decide_setpoint(
        make(ev_grid_laden=True, p1=5929 - NET_LADEN_MARGIN_W - 150, soc=70,
             reserve_grid_w=300)) == -150


def test_reservation_under_peak_total_import_reaches_the_ceiling():
    """The car+battery split must land total import ON the ceiling, not below it.

    Regression: the car's `load` (p1 minus its own draw) already contains the battery's charge
    power, so subtracting the full reservation on top double-booked the same watts and parked
    import at (ceiling - resv) — 2200 W against a 2683 W ceiling with resv=500, wasting 500 W of
    peak allowance. The car now yields only the part the battery has not yet taken. Here the
    battery already draws its full 500 W, so from the battery's side the headroom is exactly
    consumed and it holds at -500 rather than backing off.
    """
    out = decide_setpoint(make(ev_grid_laden=True, p1=5929 - NET_LADEN_MARGIN_W,
                               battery_power_net=-500, soc=70,
                               maandtarget_kw=5.929, reserve_grid_w=500))
    assert out == -500


def test_reservation_under_peak_counts_existing_charge():
    # The battery is already charging 300 W, which is in p1 -> headroom must add it back, so a
    # steady 300 W reservation stays 300 W instead of ratcheting itself down to zero.
    assert decide_setpoint(
        make(ev_grid_laden=True, p1=2000, battery_power_net=-300, soc=70,
             reserve_grid_w=300)) == -300


def test_reservation_under_peak_off_by_default():
    # Toggle off -> the old hold-at-0 behaviour, unchanged.
    assert decide_setpoint(make(ev_grid_laden=True, p1=2000, soc=70)) == 0


def test_reservation_under_peak_respects_ceiling():
    assert decide_setpoint(
        make(ev_grid_laden=True, p1=2000, soc=100,
             reserve_grid_w=300)) == 0


def test_reservation_under_peak_yields_to_net_laden():
    # net_laden owns the battery; its own law runs instead of the reservation.
    out = decide_setpoint(make(ev_grid_laden=True, net_laden=True, p1=2000, soc=70,
                               reserve_grid_w=300))
    assert out != -300


# 'Max onder piek' car grid-charging: battery must HOLD, never discharge to feed the car.
def test_grid_charging_car_holds_battery():
    # car importing 3000 W from grid → battery does NOT discharge to cover it
    assert decide_setpoint(
        make(ev_laadmodus="Max onder piek", ev_grid_laden=True, p1=3000, soc=70)) == 0


def test_grid_charging_car_still_charges_real_surplus():
    # genuine solar export while the car grid-charges → battery may still charge it
    assert decide_setpoint(
        make(ev_laadmodus="Max onder piek", ev_grid_laden=True, p1=-1500, soc=70)) == -1500


def test_grid_charging_flag_off_behaves_normally():
    # without the flag, Max onder piek mode alone does normal self-consumption (discharge to cover)
    assert decide_setpoint(
        make(ev_laadmodus="Max onder piek", ev_grid_laden=False, p1=3000, soc=70)) == 3000


# ---------------------------------------------------------------- 'Batterij → auto' (dump-to-car)
# Forced steady discharge into the car; the car's own peak formula absorbs it. Decoupled from P1 so
# the battery and car don't fight (which is what oscillated in the P1-tracking self-consumption law).
def test_batterij_auto_forces_full_discharge():
    # regardless of P1 (car about to absorb it), the battery pushes a steady BATTERIJ_NAAR_AUTO_W.
    # NOT MAX_DISCHARGE_W: the dump is a chosen RATE and the two stopped coinciding when the
    # discharge ceiling rose to 5 kW. A 5 kW dump would exceed what the EV3 (16 A) can absorb.
    assert decide_setpoint(
        make(ev_laadmodus="Batterij → auto", ev_batterij_auto_laden=True, p1=0, soc=70)
    ) == BATTERIJ_NAAR_AUTO_W


def test_batterij_auto_discharge_is_p1_independent():
    # the whole point: the forced discharge does NOT vary with P1 (no fight with the car loop)
    lo = decide_setpoint(make(ev_laadmodus="Batterij → auto", ev_batterij_auto_laden=True, p1=-2000, soc=70))
    hi = decide_setpoint(make(ev_laadmodus="Batterij → auto", ev_batterij_auto_laden=True, p1=4000, soc=70))
    assert lo == hi == BATTERIJ_NAAR_AUTO_W


def test_batterij_auto_respects_reserve_floor():
    # at/below the reserve floor the forced discharge is blocked (protects the reserve)
    assert decide_setpoint(
        make(ev_laadmodus="Batterij → auto", ev_batterij_auto_laden=True, p1=0, soc=40)) == 0


def test_batterij_auto_flag_off_no_forced_discharge():
    # mode selected but no car drawing → plain self-consumption, never a forced grid dump
    assert decide_setpoint(
        make(ev_laadmodus="Batterij → auto", ev_batterij_auto_laden=False, p1=-1500, soc=70)) == -1500


def test_batterij_auto_overrides_grid_laden():
    # 'Batterij → auto' + grid = Max tot piek (ev_grid_laden): the explicit dump must WIN over the
    # grid-charge guard — the battery drains into the car while the grid tops it up to the peak.
    # (Regression: the guard used to clamp d<=0 here, silently killing the dump.)
    assert decide_setpoint(make(
        ev_laadmodus="Batterij → auto", ev_batterij_auto_laden=True,
        ev_grid_laden=True, p1=3000, soc=70)) == BATTERIJ_NAAR_AUTO_W


def test_batterij_auto_net_laden_takes_precedence():
    # grid-charge-to-full override wins over the dump (contradictory intents; net_laden owns the batt)
    sp = decide_setpoint(make(
        ev_laadmodus="Batterij → auto", ev_batterij_auto_laden=True,
        net_laden=True, soc=50, battery_power_net=-2000, p1=0, maxphase_a=20))
    assert sp < 0  # charging, not the forced +3000 discharge


# ---------------------------------------------------------------- clamps & invariants
@pytest.mark.parametrize("p1", range(-8000, 8001, 250))
@pytest.mark.parametrize("soc", [16, 50, 96])
def test_output_always_within_power_limits(p1, soc):
    sp = decide_setpoint(make(p1=p1, soc=soc, battery_power_net=0))
    assert -MAX_CHARGE_W <= sp <= MAX_DISCHARGE_W


def test_charge_discharge_helpers():
    assert charge_w(-2000) == 2000 and discharge_w(-2000) == 0
    assert discharge_w(1500) == 1500 and charge_w(1500) == 0


@pytest.mark.parametrize("mode,hungry", list(itertools.product(MODES, [False, True])))
def test_full_sweep_invariants(mode, hungry):
    for soc in (10, 30, 41, 60, 96, 99):
        for p1 in (-4000, -800, 0, 800, 4000):
            inp = make(ev_laadmodus=mode, ev_higher_hungry=hungry, soc=soc, p1=p1, battery_power_net=0)
            sp = decide_setpoint(inp)
            assert -MAX_CHARGE_W <= sp <= MAX_DISCHARGE_W
            if soc <= 40 and sp > 0:
                pytest.fail(f"discharged below floor: soc={soc} sp={sp} {inp}")
            if soc >= 100 and sp < 0:
                pytest.fail(f"charged above ceiling: soc={soc} sp={sp} {inp}")


# ---------------------------------------------------------------- grid-charge to full (net_laden)
def test_net_laden_dynamic_phase_cap():
    # charging 2000 W, worst phase 20 A, target 27 → cap = 2000 + (27-20)*230 = 3610
    assert decide_setpoint(
        make(net_laden=True, soc=50, battery_power_net=-2000, p1=0, maxphase_a=20)) == -3610


def test_net_laden_backs_off_when_phase_high():
    assert decide_setpoint(
        make(net_laden=True, soc=50, battery_power_net=-2000, p1=0, maxphase_a=30)) == -1310


def test_net_laden_ramps_to_inverter_max_when_phases_cold():
    assert decide_setpoint(
        make(net_laden=True, soc=50, battery_power_net=-3000, p1=0, maxphase_a=10)) == -5000


def test_net_laden_never_charges_past_economic_target():
    # Expressed via the margin constant, not a literal: the net_laden law is
    # d = batt + p1_fast - (target - margin), so the expected value moves with the margin. Hardcoding
    # it meant tightening the margin failed this test for the right reason but the wrong message.
    expect = -500 + 6000 - (5929 - NET_LADEN_MARGIN_W)
    assert decide_setpoint(
        make(net_laden=True, soc=50, battery_power_net=-500, p1=6000, maxphase_a=12)) == expect


def test_net_laden_stops_at_100():
    assert decide_setpoint(make(net_laden=True, soc=100, p1=0)) == 0


def test_net_laden_fast_p1_throttles_on_import_spike():
    # median p1=0 (calm) -> charge to the inverter cap; a sudden instantaneous import (p1_fast=3000,
    # e.g. an airco just came on) backs the grid-charge off fast to stay under the peak.
    base = decide_setpoint(make(net_laden=True, soc=50, battery_power_net=-2000, p1=0, maxphase_a=10))
    spike = decide_setpoint(
        make(net_laden=True, soc=50, battery_power_net=-2000, p1=0, p1_fast=3000, maxphase_a=10))
    assert base == -5000      # full charge (inverter-capped)
    # throttled by the live import: batt + p1_fast - (target - margin)
    assert spike == -2000 + 3000 - (5929 - NET_LADEN_MARGIN_W)
    assert spike > base       # less charge, without stopping


def test_net_laden_overrides_yield():
    # net_laden wins even if a higher EV is hungry
    assert decide_setpoint(
        make(net_laden=True, ev_higher_hungry=True, ev_laadmodus="6A — van batterij",
             soc=50, battery_power_net=-2000, p1=0, maxphase_a=20)) == -3610


# ---------------------------------------------------------------- FAIL-SAFE: stale inputs → HOLD
def test_invalid_inputs_force_hold():
    assert decide_setpoint(make(p1=2000, soc=70, inputs_valid=False)) == 0


def test_valid_inputs_behave_normally():
    assert decide_setpoint(make(p1=-1500, inputs_valid=True)) == -1500


# ---------------------------------------------------------------- Batterij → auto (kWh budget)
# The mode has no dedicated branch: plain self-consumption discharges the battery to cover the
# car's grid draw. These lock that in (the budget/floor stop lives in build_batterij_naar_auto.py).
def test_batt_auto_discharges_to_cover_car():
    # car pulls 2 kW from the grid (p1=+2000) -> battery discharges 2 kW to cover it.
    assert decide_setpoint(make(ev_laadmodus="Batterij → auto", p1=2000, soc=70)) == 2000


def test_batt_auto_discharge_clamped_to_inverter_max():
    # car faster than the battery can supply -> battery maxes at MAX_DISCHARGE_W, grid tops up.
    # The CEILING here, not the dump rate: ev_batterij_auto_laden is false, so this is the base law
    # running into the inverter clamp.
    assert decide_setpoint(make(ev_laadmodus="Batterij → auto", p1=6000, soc=70)) == MAX_DISCHARGE_W


def test_batt_auto_respects_reserve_floor():
    # at/below the reserve floor the battery must not discharge into the car (the stop automation
    # then flips the mode to 'Zon only').
    assert decide_setpoint(
        make(ev_laadmodus="Batterij → auto", p1=2000, soc=40, reserve_soc=40)) == 0

# ---------------------------------------------------------------- quarter-hour budget setpoint
def test_budget_absent_falls_back_to_static_law():
    """No allowed_w -> the old static setpoint. An unavailable sensor must not steer on a default."""
    a = decide_setpoint(make(net_laden=True, soc=50, battery_power_net=-500, p1=6000, maxphase_a=12))
    assert a == -500 + 6000 - (5929 - NET_LADEN_MARGIN_W)


def test_budget_credit_allows_more_charge():
    """Under-consumed so far this quarter -> allowed above target -> charge harder.

    This is the whole point: the quarter is billed on its AVERAGE, so headroom unused early in the
    quarter is real and spendable, not lost.
    """
    # allowed_w IS the setpoint now (the budget sensor already aims at target - margin), so a
    # higher allowed_w means a higher setpoint and therefore a harder charge, one-for-one.
    lo = decide_setpoint(make(net_laden=True, soc=50, battery_power_net=-500, p1=6000,
                              maxphase_a=12, allowed_w=5829))
    hi = decide_setpoint(make(net_laden=True, soc=50, battery_power_net=-500, p1=6000,
                              maxphase_a=12, allowed_w=6829))   # 1000 W more allowance
    assert hi == lo - 1000             # more negative = charging harder


def test_budget_debt_backs_off():
    """Over-consumed early in the quarter -> allowed below target -> back off, so the AVERAGE lands
    on target instead of overshooting (the 3055 W failure of the static law)."""
    base = decide_setpoint(make(net_laden=True, soc=50, battery_power_net=-500, p1=6000,
                                maxphase_a=12, allowed_w=5829))
    debt = decide_setpoint(make(net_laden=True, soc=50, battery_power_net=-500, p1=6000,
                                maxphase_a=12, allowed_w=4829))   # 1000 W less allowance
    assert debt == base + 1000         # less negative = charging less


def test_budget_exhausted_stops_importing():
    """allowed_w = 0 (quarter budget spent) -> the law must not ask for any import."""
    d = decide_setpoint(make(net_laden=True, soc=50, battery_power_net=0, p1=0,
                             maxphase_a=12, allowed_w=0))
    assert d >= 0     # no charge command (charging would add import)


def test_budget_applies_to_the_reservation_headroom_too():
    """The reservation shares the same setpoint, so a credit widens its headroom."""
    tight = decide_setpoint(make(ev_grid_laden=True, p1=5929 - NET_LADEN_MARGIN_W, soc=70,
                                 reserve_grid_w=300))
    loose = decide_setpoint(make(ev_grid_laden=True, p1=5929 - NET_LADEN_MARGIN_W, soc=70,
                                 allowed_w=5929 + 300,
                                 reserve_grid_w=300))
    assert tight == 0        # no headroom at the static setpoint
    assert loose == -300     # 300 W of credit -> the full reservation fits


# ---------------------------------------------------------------- peak margin single source
def test_peak_margin_has_one_definition():
    """The month-peak margin must appear as a literal nowhere in the generated Jinja.

    It drives the net_laden law, the reservation headroom and ev_modulator's peak formula. Those
    used to carry a hardcoded `200` while only decide.py had a named constant — four sites to change
    in lockstep, and parity only covers two of them. This asserts the value is threaded, so
    tightening the margin stays a one-line change.
    """
    import os
    import sys
    root = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    sys.path.insert(0, root)
    import ev_modulator

    from dispatcher.decide import NET_LADEN_MARGIN_W
    from dispatcher.live_template import deployed_template

    assert ev_modulator.NET_LADEN_MARGIN_W == NET_LADEN_MARGIN_W
    m = str(NET_LADEN_MARGIN_W)

    # BEHAVIOURAL, not string-matching: the margin now lives in the BUDGET sensor, so a supplied
    # allowed_w is already the setpoint and must be used as-is. Subtracting it again here is the
    # double-count that made the reservation and then the peak setpoint wrong.
    assert _allowed_setpoint(make(allowed_w=2783)) == 2783
    # ...while the no-sensor fallback must still apply the margin itself.
    assert _allowed_setpoint(make(allowed_w=None, maandtarget_kw=2.883)) == 2883 - NET_LADEN_MARGIN_W

    # The fallback path is the only place the margin should appear in the generated Jinja.
    assert f"allowed_w - {m}" in deployed_template()
    # The OLD hardcoded value must not reappear as a bare peak-margin literal anywhere.
    for tpl in (ev_modulator.buzz_target(), ev_modulator.ev3_target(), deployed_template()):
        assert "- 200 -" not in tpl
        assert "tgt_w - 200" not in tpl


def test_peak_guard_margin_is_below_the_control_margin():
    """A guard set at the controller's own setpoint fires whenever control succeeds.

    The laws steer the projected quarter average to `target - NET_LADEN_MARGIN_W`. If
    capaciteitspiek_risico also trips at `target - NET_LADEN_MARGIN_W`, the peak guard sheds the cars
    every time the controller reaches its setpoint — observed: 34 trips and 14 sheds in 90 minutes,
    each dropping the Buzz from ~20 A to 6 A with a ~4 kW swing in P1. The guard must sit strictly
    between the setpoint and the billed limit.
    """
    assert 0 < PEAK_GUARD_MARGIN_W < NET_LADEN_MARGIN_W
