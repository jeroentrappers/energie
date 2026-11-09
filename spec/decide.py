"""Pure-Python mirror of the HA `sensor.batterij_setpoint` Jinja template.

This is the *canonical, testable* spec of the battery decision module ("de brein").
The live HA template (a `template` helper) MUST stay in lock-step with this file;
`tests/test_parity.py` renders the live Jinja against random scenarios and asserts
it equals `decide_setpoint` to catch drift.

Sign convention (matches `battery_power_net`): **positive = discharge, negative = charge.**
Output is the desired battery power in W, clamped to [-MAX_CHARGE_W, +MAX_DISCHARGE_W].

Control surface:
  - input_select.zonprioriteit  — cascade order of {EV3, Buzz, Batterij}.
  - input_select.ev_laadmodus   — Uit / 6A — van net / 6A — van batterij / Max onder piek.
  - input_boolean.batterij_net_laden — grid-charge the battery to full (override).

The battery's role in the solar-surplus cascade (see the EV modulators for the car side):
  - It drives net grid to zero (self-consumption) + an integral bias.
  - While a HIGHER-priority EV is still hungry (solar-charging, not maxed) it YIELDS the
    surplus to that car instead of grabbing it:
      * '6A — van net'      -> charge only LEFTOVER export, never discharge to feed the car
                               (the grid covers the car's sub-6 A bridge);
      * '6A — van batterij' -> do NOT charge the surplus (leave it for the car), but DO
                               discharge to cover the car's sub-6 A bridge.
    Once the cars are satisfied (maxed / not hungry) it resumes full self-consumption and
    mops up the remaining export. If the battery outranks the cars, no EV is "higher", so it
    charges first.
  - net_laden overrides everything to grid-charge to full, bounded by a dynamic per-phase cap.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

# Hardware / policy constants (kept in sync with the live template).
MAX_CHARGE_W = 5000  # inverter/BMS ceiling (~100 A @ 54 V ≈ 5.5 kW; 5 kW with margin)
# Was 3000, carried unexplained from the first commit (ee3f7a6) and never revisited while the charge
# ceiling beside it grew its own justification. Nothing in the hardware asks for the asymmetry, and
# discharge is if anything the easier direction:
#
#     BMS discharge-current limit   200 A   (vs 50 A the BMS allows for CHARGE at 90 % SoC)
#     inverter battery_max_discharge_current  100 A  -> 100 A x 52.8 V = 5.3 kW
#     number.rc_force_battery_discharge_power range   0 - 655350 W (no software ceiling)
#     measured CHARGE on 2026-09-01                   5005 W, so the AC side does 5 kW
#
# And the cap was really binding, not theoretical: over the three days to 2026-09-01 the maximum
# discharge observed was 3002 W with p99 at 2986 W — the battery sat on the limit.
#
# Raising it buys peak protection, which is the whole point of the discharge side: a 5 kW oven +
# kettle coincidence can now be covered from the battery instead of being billed as a quarter peak.
# The reserve floor (FLOOR_SOC_RESERVE) still decides *how long* it may do that, so the faster drain
# is bounded by the same policy as before.
MAX_DISCHARGE_W = 5000
FLOOR_SOC_MORNING = 15  # 06:00–10:00: release the reserve for the morning peak
FLOOR_SOC_RESERVE = 40  # otherwise: keep 40 % in reserve for the morning peak

# Integral compensation for the proportional base law. A P-only loop against an inverter that
# under-delivers its commanded power leaves a steady-state grid import equal to that loss; the
# bias accumulates the residual P1 so net → 0. It must integrate SLOWLY against a roughly
# steady error — a high gain just chases the (very noisy) live P1 and windmills to its clamp.
BIAS_KI = 0.1  # fraction of the live P1 error folded into the bias each cycle (slow integral)
BIAS_LIMIT_W = 200  # anti-windup clamp on the accumulated bias
BIAS_DEADBAND_W = 20  # don't integrate small residuals/noise (avoids jitter around zero)

# Grid-charge-to-full override (input_boolean.batterij_net_laden): charge from solar + grid up
# to 100 %, driving net import up to (maandtarget − margin). The charge power is bounded
# DYNAMICALLY by the worst measured per-phase current: it ramps up until the busiest 230 V-delta
# conductor reaches PHASE_TARGET_A and backs off above it — pushing as hard as the live headroom
# allows (accounting for aircos/house) while staying clear of the 29 A breaker_guard / 32 A main.
# Net-import target = maandtarget − this. Sizing note: this absorbs how much a sudden unexpected
# load (oven, kettle) adds to the QUARTER-HOUR average before the 30 s modulator cycle and the car's
# ramp-down can trim it — roughly 3 kW for a minute ≈ 200 W of quarter-hour average.
#
# Measured 2026-08-07..28 (2064 complete quarters): only 5 exceeded the 2683 W setpoint and the worst
# overshoot was 199 W, landing at 2882 W against the 2883 W target — one watt of margin left. At 200 W
# this had never been breached. Tightened to 100 W on request: the gain is 100 W (~0.43 A) of usable
# charging power at the current low ceiling; the accepted risk is that a worst-case disturbance now
# overshoots the target by ~100 W and ratchets the month peak up by that much (~€0.50/month, and the
# raise-only target then keeps the extra headroom). Revisit once the ceiling is raised — the margin
# costs a fixed number of watts, so it is 7 % of a 2.883 kW ceiling but only 4 % of 5.0 kW, and the
# protection is worth more than the watts at that point.
NET_LADEN_MARGIN_W = 100
# Threshold for binary_sensor.capaciteitspiek_risico, which fires the peak guard (shed the cars,
# then the aircos). It MUST be strictly smaller than NET_LADEN_MARGIN_W.
#
# The invariant matters: the control laws steer the projected quarter average to
# `target − NET_LADEN_MARGIN_W`, so a guard set at the SAME threshold fires exactly when the
# controller succeeds. That is what happened when the risk flag was unified onto the control margin
# — 34 risk trips and 14 mode=Pause sheds in 90 minutes, each dumping the car from ~20 A to 6 A and
# swinging P1 by ~4 kW. A guard is a backstop for when control has FAILED, not a second copy of the
# control limit, so it sits between the controller's setpoint and the billed limit:
#
#     controller aims here          guard fires here      Fluvius bills here
#     target − 100 ................ target − 25 ......... target
PEAK_GUARD_MARGIN_W = 25
# Emergency trickle: below TRICKLE_ON_SOC the battery must take at least TRICKLE_W of charge,
# whatever else the laws want, and it keeps taking it until TRICKLE_OFF_SOC.
# Reconstructed from the LIVE template on 2026-09-01 -- it had been hand-added
# in HA and existed in neither decide.py nor live_template.py, so the next `build_template.py` run
# would have silently deleted it. Ordering matters and is preserved: the trickle is applied BEFORE
# the `soc <= vloer` clamp, which is `min(0, d)` and therefore keeps a negative (charging) d.
# HYSTERESIS, not one edge. This was a single `soc < 12` when reconstructed from the live template
# on 2026-09-01, and it pumped: the trickle lifted the SoC a point, stopped, the reading fell back,
# and it fired again. Measured that morning — 06:11 10 %, 06:15 9 %, 06:54 10 %, 06:55 11 %, 06:59
# back to 10 %, 07:11 11 %, 07:39 12 % — with the trickle re-firing at 07:08, 09:04 and 09:09. The
# discharge floor does NOT prevent this: it stops the CONTROL LAW discharging, but the pack keeps
# draining anyway. Two causes, both measured overnight 2026-09-01:
#
#   * the inverter's own standby draw. With the setpoint commanded to 0, battery_power_net still
#     read +45..+51 W discharge across 77 samples (mean +49 W). That is a real, permanent load.
#   * the BMS re-estimating near the knee. At 05:55:13 SoC read 18 %, at 05:55:36 it read 14 % --
#     four points in 23 s, which as energy would be 50 kW. Below roughly 18 % the flat LFP voltage
#     plateau ends and the BMS snaps its estimate down to reality, so SoC is not a trustworthy
#     energy measure down there. It errs toward triggering EARLY, which is the safe direction.
#
# The gap between the discharge floor and TRICKLE_ON_SOC is therefore not dead space -- it is the
# COAST BAND that absorbs that ~49 W without waking the trickle. At 8 kWh usable, one point is
# ~80 Wh, so a point buys ~1.6 h: floor 15 % -> on 10 % coasts about 8 h (a full night), and the
# 10-12 band re-arms after ~3.3 h. Charging back up to the floor instead would leave no band at
# all and the trickle would run continuously against the standby draw -- the same pumping, made
# permanent. Widen the band, never narrow it toward the floor.
#
# So: start at or below TRICKLE_ON_SOC, and once started keep going until TRICKLE_OFF_SOC. The 10 %
# start sits one point above the inverter's own over-discharge protection (10 %) and two above its
# force-charge (9 % @ 500 W), so we act just before its safeties do rather than after.
TRICKLE_ON_SOC = 10
TRICKLE_OFF_SOC = 12
TRICKLE_W = 220
PHASE_TARGET_A = 27       # charge until the worst measured conductor reaches this (margin to 29/32 A)
PHASE_V = 230

EvLaadmodus = Literal[
    "Uit",
    "Zon only",          # charge only on solar: ramp 6A->max, pause below 6A (no bridge)
    "6A — van net",      # solar-follow with a 6A floor bridged from the grid
    "6A — van batterij",  # solar-follow with a 6A floor bridged from the battery
    "Max onder piek",    # grid up to the month-peak (not solar-following)
    "Batterij → auto",   # dump a kWh-budget of the home battery into the car (forced discharge)
]

# The one mode in which the battery feeds the car (discharges to cover its sub-6 A bridge).
BATTERY_FEEDS_CAR = "6A — van batterij"

# 'Batterij → auto': force a STEADY discharge into the car (see decide_setpoint). Bounded by the
# reserve floor; the kWh-budget automation flips the mode back to 'Zon only' once it's delivered.
#
# DELIBERATELY its own number, not an alias of MAX_DISCHARGE_W any more. It is a chosen dump RATE,
# not a ceiling, and the two only ever coincided by accident. Aliased, raising the cap to 5000 would
# have raised the dump to 5 kW (21.7 A) as a side effect — which the Buzz could absorb but the EV3
# (16 A = 3680 W) could not, so the remainder would have gone to the grid. Change this when you want
# a different dump rate, not when the hardware ceiling moves.
BATTERIJ_NAAR_AUTO_W = 3000


@dataclass(frozen=True)
class Inputs:
    """Everything the decision module reads. Mirrors the template's `states(...)` calls."""

    battery_power_net: float  # W, + discharge / − charge (live reading)
    p1: float  # W net grid, + import / − export (median, raw fallback)
    soc: float  # %
    is_ochtend: bool  # 06:00 <= now < 10:00 (lower reserve floor)
    ev_laadmodus: EvLaadmodus  # shared charge mode (input_select.ev_laadmodus)
    # True when a car ranked ABOVE the battery is ELIGIBLE for the surplus in a solar mode:
    # plugged, not maxed, and the gross solar surplus is enough for it to charge. The battery
    # then yields so it can start/ramp on solar (computed in the preamble from pv_bruto_overschot,
    # so the yield bootstraps *before* the car draws — no deadlock). Name kept for test stability.
    ev_higher_hungry: bool = False
    inputs_valid: bool = True  # False if any critical sensor (P1/SOC/batt power) is unavailable
    bias: float = 0.0  # integral correction (W) accumulated by next_bias; added to the base law
    net_laden: bool = False  # grid-charge-to-full override (input_boolean.batterij_net_laden)
    maandtarget_kw: float = 2.5  # sensor.capaciteit_maandtarget (kW), the import ceiling
    maxphase_a: float = 0.0  # worst measured per-conductor current (A) — caps net-laden charge.
    # Global worst, NOT per-load: this is a 3-wire 230V DELTA, so every single-phase load is wired
    # line-to-line and shares a conductor with the others — no load "owns" a phase, so each grid
    # charger must respect the worst conductor (and only one grid-charges at a time; see goedkoop_laden).
    reserve_soc: float = FLOOR_SOC_RESERVE  # editable daytime/evening discharge floor (input_number)
    # True when a car is grid-charging in 'Max onder piek' (mode set + a car actually drawing). The
    # car's import is DELIBERATE, so the battery must NOT discharge to cover it (self-consumption
    # would otherwise dump the home battery into the car). Battery holds (charges surplus only).
    ev_grid_laden: bool = False
    # True when a car is plugged in a grid-charge mode but NOT drawing, because the modulator parked
    # it (no valid setpoint: headroom below the charger's 6 A minimum). Distinct from ev_grid_laden,
    # which now correctly requires a car to actually draw. In this state there is no deliberate car
    # import to protect, so the battery may help — but only by PEAK-SHAVING the excess over the
    # allowance, NOT by full self-consumption. Driving p1 → 0 here would drain the battery covering
    # the whole baseload (~2.7 kW overnight) for no peak benefit, since a projection already under
    # target needs nothing. So: discharge exactly the overshoot, and nothing when there is none.
    ev_grid_parked: bool = False
    # True when the battery contribution level is '6A-brug' (v2) — discharge to cover the car's
    # sub-6 A bridge (the old '6A — van batterij'). Resolved in the preamble from ev_batterij_niveau.
    battery_bridges: bool = False
    # True in the battery-dump level ('Ontlaad-budget', v2) when a car is actually drawing: force a
    # STEADY discharge into the car. Gated on a car drawing so we never force-discharge into the
    # grid; the car's own solar/peak formula ramps up to absorb it. A free self-consumption
    # discharge here would fight the car for P1 and oscillate — so this replaces the base law.
    ev_batterij_auto_laden: bool = False
    # Instantaneous P1 (not the 90s median) used ONLY for the net_laden import-cap, so grid-charging
    # backs off fast when a load (airco/washing machine) is added — keeping import under the month
    # peak without the median's lag. Defaults to p1 (so the base self-consumption stays anti-jitter).
    p1_fast: "float | None" = None
    # RESERVATION. While yielding the surplus to a higher-ranked car, reserve this many watts for
    # the home battery instead of holding at exactly 0 and injecting the remainder. This is one half
    # of a matched pair: the CAR subtracts the same number from its own available surplus
    # (ev_modulator.RESERVE_W) and steps down to make room, so the two control loops agree on the
    # split rather than racing for the same watts. Charge-only and bounded by the reservation, so
    # the worst case is that we charge less than asked (never that we import to fill the battery).
    # Driven by input_boolean.batterij_reservering_actief / input_number.batterij_reservering_w.
    reserve_surplus: bool = False
    reserve_surplus_w: float = 800.0
    # Import-headroom leftover for the PEAK branch, distinct from reserve_surplus_w above (which
    # bounds how much EXPORT the battery may take while a car ramps on solar). Computed outside as
    # max(manual floor, allowed - load_nonsteerable - car_hard_max) and read by BOTH loops from the
    # same sensor, which is what keeps them from fighting: the car subtracts exactly what the battery
    # takes, so total import lands on `allowed`.
    #
    # Reserving the car's FULL hardware max (16 A EV3 / 32 A Buzz) is deliberate -- the car climbs to
    # its maximum first and the battery only receives the genuine remainder. It must NOT be the
    # measured headroom: `resv` is watts the car's own formula treats as "not mine", so feeding the
    # measured headroom back would drop the car's target below its cap, freeing headroom, which the
    # battery would take, dropping the target again -- a runaway that displaces the car entirely.
    reserve_grid_w: float = 0.0
    # QUARTER-HOUR BUDGET. Fluvius bills the AVERAGE import per quarter, independently per quarter,
    # so the setpoint should not be a constant `target − margin` for the whole quarter: it should be
    # whatever keeps the *average* on target given the energy already spent —
    #     allowed = (target × 0.25 − kwartier_import) / remaining_hours
    # (sensor.kwartier_toegestaan_vermogen, built by build_kwartier_budget.py). That both recovers
    # the margin every quarter and self-corrects after a disturbance: a 3 kW oven for 5 min
    # mid-quarter drove the STATIC law's average to 3055 W against a 2883 W target (a new peak),
    # where the budget law lands on 2883.
    # None -> fall back to the static law, so a missing/unavailable sensor degrades safely to the
    # previous behaviour instead of steering on a default.
    allowed_w: "float | None" = None


def _allowed_setpoint(i: Inputs) -> float:
    """Import setpoint (W) the peak laws steer to. Mirrors the Jinja exactly.

    `allowed_w` is the quarter-hour budget law; the margin on top covers the only overshoot it
    cannot correct — a disturbance appearing inside the controller's reaction window
    (3 kW × 30 s / 900 s ≈ 100 W = NET_LADEN_MARGIN_W). With `allowed_w` absent this collapses to
    the old static law, so an unavailable sensor is safe rather than silently permissive.
    """
    if i.allowed_w is None:
        # Fallback: no budget sensor, so the static law must apply the margin itself.
        return i.maandtarget_kw * 1000 - NET_LADEN_MARGIN_W
    # The budget sensor already aims at (target - margin) — see build_kwartier_budget.py. Subtracting
    # it again here would double-count the same watts, the way the reservation once did.
    return i.allowed_w


def _battery_feeds_car(i: Inputs) -> bool:
    """True when the battery should discharge to cover the car's sub-6 A bridge.

    v2: driven by the battery contribution level (ev_batterij_niveau == '6A-brug'), resolved
    to `battery_bridges` in the preamble — the old '6A — van batterij' mode.
    """
    return i.battery_bridges


def decide_setpoint(i: Inputs) -> int:
    """Return desired battery power (W); + discharge, − charge. Mirrors the Jinja exactly."""
    # FAIL-SAFE: a stale/unavailable critical sensor must HOLD, never mis-steer on a default.
    if not i.inputs_valid:
        return 0
    vloer = FLOOR_SOC_MORNING if i.is_ochtend else i.reserve_soc

    # Base law: drive net grid to zero (self-consumption) + integral bias.
    d = i.battery_power_net + i.p1 + i.bias

    # 'Batterij → auto': dump the home battery into the car at a stable, FORCED discharge so only
    # the CAR loop tracks P1 (its peak formula ramps up to absorb this and holds import at the
    # peak). A free self-consumption discharge here would chase P1 → 0 while the car chases P1 →
    # (peak − 200), and the two would oscillate. Gated (ev_batterij_auto_laden) on a car actually
    # drawing so we never force-discharge into the grid; bounded below by the reserve floor.
    if i.ev_batterij_auto_laden and not i.net_laden:
        d = BATTERIJ_NAAR_AUTO_W

    # Cascade yield: while a higher-priority EV is still ramping on the surplus, RELEASE it —
    # the battery must not charge from the surplus (else the export never appears and the EV
    # can't start: the bootstrap deadlock). Once the EV is maxed it's no longer "hungry", this
    # lifts, and the battery mops up the leftover via full self-consumption.
    #   van batterij -> may discharge to feed the car's sub-6 A bridge (d >= 0);
    #   van net / Zon only -> HOLD (d = 0): yield the surplus, grid covers any deficit.
    # Skipped while net_laden owns the battery.
    if i.ev_higher_hungry and not i.net_laden:
        if _battery_feeds_car(i):
            d = max(0.0, d)
        elif i.reserve_surplus:
            # Reservation: charge from the LEFTOVER export only (never discharge), bounded by it.
            # min(0, d) drops any discharge intent; max(-cap, ...) bounds the charge.
            d = max(-i.reserve_surplus_w, min(0.0, d))
        else:
            d = 0.0

    # A car grid-charging in 'Max onder piek'/'Max tot piek' is importing deliberately — never
    # discharge the home battery to cover it (that would drain it into the car). Hold: charge from
    # real surplus only. EXCEPTION: the battery-dump level (ev_batterij_auto_laden) IS an explicit
    # request to drain the battery into the car, so the forced dump above must survive this guard —
    # grid tops the car up to the peak while the battery adds its steady discharge on top.
    if i.ev_grid_laden and not i.net_laden and not i.ev_batterij_auto_laden:
        d = min(0.0, d)

        # RESERVATION under the month peak. The hold above leaves the battery at 0 while the car
        # grid-charges, so the reservation had no effect in 'Max tot piek' — the surplus path is the
        # only place it applied. The car now yields `reserve_surplus_w` of its peak headroom
        # (ev_modulator's grid_max branch), so taking those watts here leaves TOTAL import unchanged:
        # they simply move from the car to the battery, under the same ceiling.
        #
        # Bounded by the REMAINING import headroom rather than taken on faith: ev_grid_laden is based
        # on intent (a car plugged and below target), not on the car actually drawing, so without this
        # bound a car that is plugged but idle would let us add reservation watts on top of the real
        # load and push the peak. `now_charge` is added back because the battery's existing charge is
        # already reflected in p1.
        if i.reserve_grid_w > 0 and i.soc < 100:
            p1_fast = i.p1 if i.p1_fast is None else i.p1_fast
            now_charge = max(0.0, -i.battery_power_net)
            headroom = _allowed_setpoint(i) - p1_fast + now_charge
            resv = -max(0.0, min(i.reserve_grid_w, headroom))
            # `min`, NOT an assignment. The reservation reserves a slice of the peak IMPORT
            # headroom for the battery; it was never meant to cap how much free SOLAR the battery
            # may absorb. Assigning here overwrote the base law, so while a car grid-charged the
            # battery was pinned at exactly reserve_surplus_w even with the surplus far above it.
            # Measured 2026-08-30: 45 min of export up to 1.8 kW with the battery held at ~390 W
            # at 15-19 % SoC, and later 5.4 kW of surplus with p1 at -864 W (still exporting) while
            # the battery took only its 1300 W reservation. `d` is already <= 0 from the hold above,
            # so min() picks whichever charges harder.
            #
            # Convergent, and it cannot breach the peak: the base-law term only exceeds the
            # reservation while there is real export to absorb (it drives p1 -> 0 and stops), and
            # as the car grows the export shrinks until the reservation dominates again and total
            # import settles on `allowed` exactly as before. The solar-yield branch above keeps its
            # max(-cap, ...) BOUND on purpose -- there the cap protects a ramping car's surplus
            # from being eaten (the bootstrap deadlock); here there is no such car to protect.
            d = min(d, resv)

    # Cars plugged but PARKED: shave the overshoot only. `max(0, ...)` means a safe projection asks
    # for nothing, so the battery is spent on peaks rather than on baseload. Uses the instantaneous
    # P1 against the same allowance the chargers steer to, so all three loops share one error signal.
    if i.ev_grid_parked and not i.net_laden and not i.ev_batterij_auto_laden:
        p1_fast = i.p1 if i.p1_fast is None else i.p1_fast
        # `min(d, ...)`, NOT an assignment -- the same mistake this file already carries a note about
        # in the reservation branch below, made again here when this branch was added on 2026-08-29.
        # Assigning discards the base law, so while EXPORTING (p1 negative) the overshoot term is 0
        # and the battery was forced to 0 W. Measured 2026-08-30: 5.4 kW of export with the battery
        # idle at 36 % SoC, because a plugged-but-not-drawing car put us in this branch.
        #
        # The two sides are different questions and need different limits:
        #   discharge: only the overshoot, so the battery is spent on peaks and not on baseload;
        #   charge:    the full base law, so real export is absorbed.
        # min() gives exactly that -- it keeps a charging base law untouched and clamps a
        # discharging one down to the overshoot.
        d = min(d, max(0.0, p1_fast - _allowed_setpoint(i)))

    # Grid-charge-to-full override, bounded by the live per-phase headroom (incremental cap).
    # Uses the INSTANTANEOUS P1 (p1_fast) so it throttles the charge fast when a load is added,
    # holding total import under the peak — a throttle, not a stop (and not mixed with price).
    if i.net_laden and i.soc < 100:
        p1_fast = i.p1 if i.p1_fast is None else i.p1_fast
        d = i.battery_power_net + p1_fast - _allowed_setpoint(i)
        now_charge = max(0.0, -i.battery_power_net)
        phase_cap = now_charge + (PHASE_TARGET_A - i.maxphase_a) * PHASE_V
        phase_cap = max(0.0, min(MAX_CHARGE_W, phase_cap))
        d = max(-phase_cap, d)  # don't charge harder than the worst conductor allows

    # Charge ceiling.
    # Emergency trickle below TRICKLE_SOC: force at least TRICKLE_W of charge so a deeply drained
    # pack is pulled off its cut-off regardless of price, cascade or peak. Placed before the
    # clamps below for the reason given at TRICKLE_SOC.
    # Hysteresis carried by the plant's own state rather than a helper — same idiom as the solar
    # gate (PV_STOP_W if drawing else PV_START_W) and the grid branch's 6/GRID_START_A split.
    # "Already trickling" is read off the battery actually taking charge, so the band latches on
    # the cycle after the first command and cannot be lost across a restart.
    _trickling = i.battery_power_net <= -(TRICKLE_W / 2)
    if i.soc <= TRICKLE_ON_SOC or (_trickling and i.soc < TRICKLE_OFF_SOC):
        d = min(d, -float(TRICKLE_W))
    if i.soc >= 100:
        d = max(0.0, d)  # no charge above the ceiling
    # Discharge floor (morning-reserve protection).
    if i.soc <= vloer:
        d = min(0.0, d)  # no discharge below the floor

    return round(max(-MAX_CHARGE_W, min(MAX_DISCHARGE_W, d)))


def _base_tracking(i: Inputs) -> bool:
    """True only when the loop is driving raw P1 → 0, so the integral may accumulate.

    Frozen while yielding to an EV, grid-charging, or saturated at floor/ceiling — in those
    regimes the loop isn't trying to drive raw P1 to zero, so folding P1 in would wind it up.
    """
    if not i.inputs_valid:
        return False
    vloer = FLOOR_SOC_MORNING if i.is_ochtend else i.reserve_soc
    if i.ev_higher_hungry:  # yielding to a higher EV → not tracking raw P1
        return False
    if i.ev_grid_laden:  # holding while a car grid-charges → not tracking raw P1
        return False
    if i.ev_batterij_auto_laden:  # forced dump-to-car → not tracking raw P1
        return False
    if i.net_laden and i.soc < 100:  # intentional grid charge → not tracking raw P1
        return False
    if i.soc >= 100:  # charge ceiling
        return False
    if i.soc <= vloer:  # discharge floor
        return False
    return True


def next_bias(i: Inputs) -> float:
    """Updated integral bias for the next cycle (uses i.bias as the previous value).

    Frozen outside the base-tracking regime and within a small deadband around zero; otherwise
    nudged slowly by BIAS_KI·P1 and clamped to ±BIAS_LIMIT_W. Positive P1 (import) raises the
    bias → more discharge next cycle → import trends to zero.
    """
    if not _base_tracking(i):
        return round(i.bias, 1)
    if abs(i.p1) < BIAS_DEADBAND_W:  # ignore noise / near-zero residual
        return round(i.bias, 1)
    raw = i.bias + BIAS_KI * i.p1
    return round(max(-BIAS_LIMIT_W, min(BIAS_LIMIT_W, raw)), 1)


def charge_w(setpoint: int) -> int:
    """Convenience: the charge power the dispatcher commands (0 if discharging)."""
    return max(0, -setpoint)


def discharge_w(setpoint: int) -> int:
    """Convenience: the discharge power the dispatcher commands (0 if charging)."""
    return max(0, setpoint)
