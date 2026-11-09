# Energy control logic — overview

One principle underneath everything: **every loop drives the grid meter (P1) toward zero.**
Priority + gating turn the independent loops into a **solar-surplus cascade** instead of a
tug-of-war. Hard safety (the breaker) sits on top, independent of all of it.

## Inputs (the whole control surface)

| Input | Entity | What it does |
|---|---|---|
| **Priority** | `input_select.zonprioriteit` | Cascade order of {EV3, Buzz, Batterij}. Higher rank consumes solar surplus first. |
| **Grid level** (v2) | `input_select.ev_net_niveau` | `Uit` / `6A-brug` / `Max tot piek`. How much the grid may contribute (car **and** battery-to-full). Always price-gated. |
| **Battery level** (v2) | `input_select.ev_batterij_niveau` | `Uit` / `6A-brug` / `Ontlaad-budget`. How much the home battery contributes to the car. |
| **Dump budget** | `input_number.batterij_naar_auto_budget` | kWh the battery dumps into the car (per-session) at the `Ontlaad-budget` level. |
| **Price gate** | `input_number.goedkoop_laden_prijs_drempel` | The one grid gate: grid contributes only while `price < threshold`. Set high = always. |
| **EV master** | `input_boolean.pv_overschot_laden_actief` | Global enable for EV charging (off = don't charge; replaces the old `Uit` mode). |
| **Peak guard** | `input_boolean.peak_guard_actief` | Enables the month-peak shedder. |
| **Battery steering** | `input_boolean.soluna_absorptie_actief` | Enables the dispatcher (executes the setpoint). |

Key live signals: `sensor.p1_meter_vermogen` (+import/−export), the **measured per-phase
currents** `sensor.p1_meter_stroom_fase_1/2/3` (the real breaker view), real PV
(`omnik…current_power_production` + `solis…total_pv_power_2`), `pv_overschot_beschikbaar`
(battery-adjusted surplus = the EV ramp signal), SoC, `capaciteit_maandtarget`.

## EV charging — two orthogonal levels (v2)

The old 6-way `ev_laadmodus` is retired. Solar is always the base; **grid** and **battery**
each contribute at one of three levels. The old modes are just combinations (see
`DESIGN-charge-control-v2.md` for the full mapping and rationale).

**Grid level** (`ev_net_niveau`) — always price-gated (`price < goedkoop_laden_prijs_drempel`;
when dear, every level collapses to solar-only):
- **Uit** — no grid.
- **6A-brug** — solar-follow; grid bridges the sub-6 A floor so a passing cloud doesn't stop the
  session (daytime; needs sun).
- **Max tot piek** — charge to the month-peak (grid-maximising, day or night). Also folds in
  **grid → battery-to-full**: when it's the battery's turn (`zonprioriteit`) the battery
  grid-charges via the internal `batterij_net_laden`, driven by `build_goedkoop_laden.py`. Only one
  grid-charger runs at a time (3-wire delta); the car defers via its `allowed` gate.

**Battery level** (`ev_batterij_niveau`) — how much the home battery gives the car:
- **Uit** — never discharge to feed the car (normal self-consumption + cascade yield).
- **6A-brug** — discharge to cover the car's sub-6 A bridge (the old `6A — van batterij`).
- **Ontlaad-budget** — *dump a kWh-budget into the car*. The battery **force-discharges a steady
  ~3 kW** (`ev_batterij_auto_laden` in `decide.py`) — a **fixed feed-forward**, not P1-tracking
  self-consumption, so only the *car* loop tracks P1 and they can't oscillate (the fix that started
  this: two P1-trackers with opposite setpoints fight). With grid = *Max tot piek* the car's peak
  formula absorbs the dump; with grid off the car ramps on `avail_dump` (which adds the discharge
  back to `besch`) and drives P1 → 0. Gated on a car actually **drawing** (never dumps to grid),
  bounded by the reserve floor. `automation.batterij_naar_auto_beheer` snapshots the discharge
  counter on entry and sets the battery level back to **Uit** once `batterij_naar_auto_budget` kWh
  are delivered (or the floor is hit) — leaving the grid axis untouched.

Reproduction of the old modes: `Zon only` = both **Uit**; `6A — van net` = grid **6A-brug**;
`6A — van batterij` = battery **6A-brug**; `Max onder piek` = grid **Max tot piek**;
`Batterij → auto` = grid **Max tot piek** + battery **Ontlaad-budget**.

Each modulator's target current = `available = (pv_overschot_beschikbaar + my_own_draw +
lower_priority_battery_charge)/230` — an *absolute*, convergent signal. Adding the car's own draw
back makes it immune to a stale commanded current; adding back a **lower-priority battery's
charge** stops the battery from starving the car's ramp signal when it mops up behind it (a
charging battery drives P1→0, collapsing `pv_overschot_beschikbaar`). Without it the car and
battery oscillate at the car's max: car hits max → battery charges → besch collapses → car backs
off → battery yields → repeat. The battery's "car is maxed" gate also has 1 A of hysteresis
(treats ≥ max−1 A as maxed) so a single-amp ramp jitter doesn't toggle the battery hold/charge.

### Charge-current resolution — 0.1 A on the SmartEVSE

The shared target is in **amps to one decimal**. The SmartEVSE accepts deci-amps over MQTT
(`SmartEVSE/1234/Set/CurrentOverride`, payload in tenths, so `145` = 14.5 A; fw v3.11.2). The HA
`number` entity is a **lossy wrapper** over the same value — it declares `step: 1.0` and rounds a
written 14.5 down to 14.0 — so the Buzz is commanded over MQTT and never through the entity (reads
still come from the entity; only writes were lossy). 0.1 A ≈ 23 W instead of 230 W, so the car can
fine-track P1 to about zero. Each charger applies the shared target at its own resolution: the Buzz
publishes tenths; the Tuya cable (a real 1 A / 6–16 A device) rounds to whole amps — `|round(0)`,
not `|int`, or a 9.8 A target would systematically under-draw at 9 A.

### Battery reservation — `input_boolean.batterij_reservering_actief` (+ `…_w`)

Optional (default off). While yielding to a higher-ranked car the battery used to hold at exactly
0 W and everything the car didn't take was injected. The reservation holds back N watts for the
battery, and it is a **matched pair** — both halves are required, because the car's add-back above
means it would otherwise just ramp up and reclaim whatever the battery took:

- **car** subtracts the reservation from `available` (`+ batt_charge − resv`) — the one place the
  add-back is deliberately undone — so it steps down and makes room;
- **battery** charges up to the same number, charge-only (`max(−resv, min(0, d))`), so the worst
  case is charging less than asked, never importing to fill the battery.

Convergent by construction: once the battery is drawing its reservation the two terms cancel and
`available` is simply the true remaining surplus. Measured at 3000 W export with a car at 6 A:
19.0 A with the toggle off, **15.5 A either way** once on (whether or not the battery has started) —
a 3.5 A / 805 W handover that cannot oscillate.

The reservation applies in **both** regimes. On solar it moves surplus watts; under `Max tot piek`
it moves *peak headroom* — the car yields `resv` from its own ceiling formula and the battery takes
those watts from the grid, so total import is unchanged and stays under the month peak. Under the peak the car yields only the part the battery has **not yet taken**
(`resv_eff = max(0, resv − battery_charge)`). Its `load` term is `p1` minus its own draw, so it
already contains the battery's charge power; subtracting the whole reservation on top double-books
the same watts and parks total import at `ceiling − resv` — measured 2200 W against a 2683 W ceiling
with a 500 W reservation, i.e. 500 W of peak allowance simply unused. Yielding the unrealised part
only is convergent: before the battery starts it yields everything (which is what lets it start),
and once the battery holds its reservation it yields nothing extra, so import lands **on** the
ceiling with the reservation inside it. Note the ceiling is `maandtarget − NET_LADEN_MARGIN_W`, not `maandtarget`.

### Quarter-hour budget control (the peak setpoint)

Fluvius bills the **average** import over each quarter-hour, **independently per quarter**. So the
setpoint is not a flat `target − margin`: it is whatever keeps *this quarter's average* on target
given the energy already spent —

    allowed_W = (target_kW × 0.25 − kwartier_import_kWh) / remaining_hours × 1000

`sensor.kwartier_toegestaan_vermogen` (`build_kwartier_budget.py`), the exact inverse of
`kwartiervermogen_projectie`. All three peak laws steer to `allowed` directly: the net_laden charge
law, the reservation headroom, and the car's `grid_max` formula.

**The margin belongs in the BUDGET, not the setpoint** — and getting that wrong cost a whole
tuning cycle. The law is an *integrator*: draw less than the aim early and `allowed` rises to
compensate, so the achieved average is driven toward whatever the **budget** implies. A margin
subtracted from the instantaneous setpoint is therefore integrated away and yields **no standing
clearance at all**. Measured with the margin on the setpoint, quarters converged to a fixed point of
**99.1–99.2 % of target (2856–2860 W), grazing the guard** — and identically at gain 0.5, 0.65 and
0.8, which is how it became clear the gain was never the lever. (An earlier simulation had already
shown "the margin is a transient offset, not a permanent loss"; I read that as reassuring rather than
as the warning it was.)

So the budget is `(target − NET_LADEN_MARGIN_W) × 0.25 h`. The integrator now converges to
`target − margin` = real standing clearance, **and** the margin still absorbs a late disturbance,
landing at worst on target. It does both jobs instead of neither. Consequently the consumers must
**not** subtract the margin again — `allowed` is already the setpoint. Only the no-sensor fallback
path applies the margin itself, and a behavioural test asserts both halves.

Two things this buys, simulated at 10 s steps with the controller recomputing every 30 s (the real
modulator cycle), target 2883 W:

| scenario | static (target−100) | budget | car kWh |
|---|---|---|---|
| quiet house 600 W | 2783 W | **2883 W** | 0.546 → 0.571 |
| 3 kW oven at t=60 s for 5 min | **3055 W ⚠ new peak** | **2883 W** | 0.364 → 0.321 |
| 3 kW oven in the last 30 s | 2810 W | 2894 W | 0.528 → 0.548 |
| house ramps 600 → 2000 W | 2799 W | **2884 W** | 0.377 → 0.398 |

It lands *on* the target instead of a margin below it (headroom unused early in a quarter is real and
spendable, not lost), **and** it removes the overshoot the static law allows — the static law breaches
the target on the mid-quarter disturbance, which is exactly the 2882 W worst real quarter that set the
current peak.

**Export is never a credit.** Fluvius bills the quarter-hour average of the **offtake**, so export
must not reduce the budget — and it does not: `p1_meter_kwartier_import` is a `total_increasing`
utility_meter on the *import register*, so it physically cannot decrement. Verified empirically —
through a sustained 30 s export run at ~1.7 kW the counter stayed at exactly 0.286 kWh (delta
+0.000). The projection agrees by construction: `(kwartier_import + max(P1,0)·rem/3.6e6)·4`, which
read 1144 W at `p1 = −1740 W`, the export term contributing zero rather than a negative.

What export *does* provide is **headroom for consumption**, which is a different thing: with 1.7 kW
of export and 3.1 kW of allowance a load may take 4.8 kW before one watt of import registers, because
consuming it cancels the export first. That is why the car is correctly permitted 22–23 A during an
export window. `build_kwartier_budget.py` asserts the counter tracks the import register on every
run — a meter pointed at a net figure would silently over-permit.

**Credit is damped, debt is not.** `allowed` spends `CREDIT_GAIN` (**0.65**) of accumulated credit per
cycle and is capped at `CLAMP_FACTOR` (1.35) × target. Debt (allowed below target) is always applied
in full, because that side is what protects the peak.

The first version chased the full credit with a 3× cap, which let the car ramp to 27–28 A (6.4 kW)
early in a quarter and armed the peak guard 10 times in 20 minutes — hence the cap. The gain was then
set to 0.5 on the mistaken belief that the credit law was causing the mode flapping; the real cause
was the guard/controller threshold collision. It went to 0.8 once that was fixed, which **overshot**:
the 13:45 quarter landed at 2848 W = 99 % of target, 35 W under the billed limit and only 10 W under
the guard — shaving the guard rather than tracking the 96.5 % setpoint, which is exactly the situation
the margin structure exists to avoid. Settled at 0.65.

**Measure it with `kwartiervermogen_vastgelegd`, not the live projection.** The projection assumes the
current draw continues, so during the credit tail it over-predicts the final average (it read 98.5 %
mid-tail on a quarter that closes lower). Only the closed-quarter value is the arbiter.

Note the steady-house simulation is a poor guide to this constant and says so: it puts 0.5 at 98.5 %
of target and 0.8 at 99.2 %, 19 W apart, with identical swing and stop counts. The **real** quarters
under 0.5 landed at 82–91 % (2377–2611 W), well below the sim, because real credit accumulates from PV
variability and the car's ramp lag in a way a flat-load model never reproduces. Judge this by the
achieved quarter averages, not by the simulation.

The end-of-quarter behaviour is the visible payoff: measured tails of 3.2–3.3 kW against a 2883 W
target, with the quarter average still landing under it, and no carry-over into the next quarter
(13:15 closed at 3321 W, 13:30 opened at 1713 W — the 30 s cycle pulls it back before it can eat the
new budget). The static law would have held ~2783 W flat and forfeited the early shortfall.

**Known limit — a large late step is not survivable.** After 14 min at the aim, 649 Wh of the 721 Wh
allowance is spent, leaving 71 Wh for the final minute (4283 W sustained). A 6 kW appliance is 100 Wh
in that minute = 400 W on the quarter average. Modelled with both levers (cut ~2 kW of car, discharge
the battery's 3 kW), only a **5 s** reaction stays under target, at 2877 W — six watts of margin; at
10 s it is 2905 W and at 30 s 3016 W. The modulator cycle is 30 s, so such a quarter sets a new peak.
Cost is bounded (~300 W over ≈ €1.50 that month, and the raise-only target then keeps the headroom),
but it is a real breach and it means the margin should **not** be tightened further. The fast paths
that do exist: `breaker_guard` every 5 s on phase current, and the dispatcher re-running on
`p1 > 5800 W`.

Guards: the formula diverges as `remaining → 0` (drawing 20 kW for 2 s barely moves an average, so it
is not *wrong*) — clamped to 3× target, and in the last 30 s it may no longer raise the setpoint above
the plain target, because there is no time left to correct a mistake. If
`kwartier_toegestaan_vermogen` is unavailable, every consumer falls back to the flat target via a
`has_value` gate rather than steering on a `|float(default)`. Boundary alignment is taken from the
utility_meter's own `next_reset`, so the controller and the billing quarter cannot disagree about
where the quarter ends.

**The guard is armed only from 60 s to 880 s into a quarter**, and its hold is 10 s. Both ends are
suppressed for different reasons:

| window | state | why |
|---|---|---|
| 0–60 s | suppressed | just after a boundary `used ≈ 0` and `rem ≈ 900`, so the projection degenerates to "instantaneous p1 extrapolated over the whole quarter" — no accumulated evidence. The previous quarter's credit tail routinely leaves p1 near target, which armed the guard within seconds of *every* boundary. Observed: boundary 16:00:00, guard fired 16:00:13, shed the car 14 A → 6 A at 16:01:01 when p1 was 47 W and the projection 74 W. 60 s is the safe limit: 4 kW for the first 60 s then a return to the aim lands at 2864 W (under target), while 120 s reaches 2945 W and breaches. |
| 60–880 s | **armed** | the projection is meaningful and a shed can still change the outcome |
| 880–900 s | suppressed | a shed can no longer change this quarter, and with the 10 s hold it would land on the next one |

`GUARD_MIN_ELAPSED_S` / `GUARD_MIN_REM_S`. The threshold must exceed the hold, or a late arm fires *after* the boundary and punishes the
next quarter — but it must not exceed it by much, because a shed keeps real authority late on:
dropping 2 kW of car with 60 s left corrects `2000 × 60/900 = 133 W` of the average, and 20 s left is
still worth 44 W. This was briefly 60 s on the reasoning that "below ~30 s a shed is pointless", which
disabled the guard across the whole final minute — exactly the window where a large appliance step is
least survivable. Shedding X watts
with `rem` seconds left moves the quarter average by `X × rem/900`, so recovering the 25 W guard band
by dropping ~1 kW needs `rem ≥ 22 s`. Below that a shed cannot change the outcome — and the guard's
own `for: 30 s` delay means it can fire *after* the quarter it reacted to has closed, dumping the car
and charging the excess to the next quarter. Observed 2026-08-28: projection 2865 W at 12:44:38,
boundary at 12:45:00, guard fired at 12:45:08 and shed 960 W into a quarter that had just reset with a
full budget. Near a boundary the projection has converged to the accumulated average, so "risk" has
become "already happened".

### Charge-session protection — the Buzz's hard constraint (2026-09-12)

A VW MEB car (ID.3, ID.Buzz) does not experience `Pause -> Normal` as a throttle. Each one is a
full IEC 61851 session teardown and restart — `C -> C1 -> B1` (PWM off, contactor open, HV
isolation dropped), then `B -> Activate -> C` — and the car's charge controller counts aborted
starts. Past a threshold it **latches a fault**: dashboard "Laadstation controleren", red ring at
the port, and from then on it ignores the pilot completely.

Measured 2026-09-12: 59 teardowns in 30 h, peaking at 15/hour. After five in 45 minutes the Buzz
refused **27 consecutive `Activate` attempts over 2 h 28 m**, while `sensor.smartevse_1234_error`
read `None` throughout — the charger is healthy, the fault is entirely inside the car.

**There is no remote recovery.** The board can cut the control pilot (`PIN_CPOFF` via MQTT
`Set/CPPWMOverride 0`) but not the *proximity* pilot, which lives in the cable — and PP is the
signal an MEB car uses to tell "cable removed" (clean reset) from "pilot lost" (fault). Tested
live on fw v3.11.2: cutting CP on a **healthy, charging** car put it straight into the refusal
state, and a 90 s CP disconnect did not clear it. Only a physical replug does. Since fw v3.10.1
no mode change reaches state A either, so `Off` offers nothing here.

So the control law's job is not to recover from the fault but to never cause it. Three rules, all
defined once in `ev_modulator` and shared by every writer:

| Constant | Value | Meaning |
|---|---|---|
| `MIN_CHARGE_S` | 300 s | a **live** session (pilot C) is never aborted before this |
| `MIN_PAUSE_S` | 60 s | a paused charger is not re-offered Normal before this |
| `RISK_STOP_S` | 60 s | `capaciteitspiek_risico` throttles instantly, may only *stop* after this |

`BUZZ_PAUSE_OK` is the single predicate ("pausing now would not abort a live session"). It is read
by the modulator's stop branch and by the peak-guard shed ladder. The breaker hard-cut is the one
deliberate exemption: a conductor over 29 A outranks the car's session.

The `not is_state(STATE,'Charging')` term in that predicate matters — protection is only owed to a
session that exists. Holding `Normal` over a car sitting at B buys nothing and still parks a
1.4 kW offer against the month target, which at a 4.0 kW target is enough to set a new peak.

**Risk is a throttle before it is a stop.** `capaciteitspiek_risico` armed 128 times on 2026-09-12
with a **median on-duration of 10 s** (119 of 128 under 30 s). It used to sit on the modulator's
hard-disarm line, so each 10-second blip wrote `mode=Pause` — 30 of 58 teardowns fired within 45 s
of a risk edge, most within 11 s. It now clamps `hard_max` to the 6 A floor immediately (giving
back ~5 kW without touching the pilot) and only disarms once still asserted after `RISK_STOP_S`.
That is also what finally lets the shed ladder in `build_automations.py` do its job: it was always
written to step the Buzz down 3 A at a time, and the modulator front-ran it every time.

**The guard must not share the controller's threshold.** `binary_sensor.capaciteitspiek_risico` fires
the peak guard (shed the cars, then the aircos). Its threshold is `target − PEAK_GUARD_MARGIN_W`
(25 W), deliberately *smaller* than the control margin, and the ordering is asserted in the tests:

    controller aims here          guard fires here        Fluvius bills here
    target − 100 ................ target − 25 ........... target

Unifying the two onto one constant — which looked like removing duplication — instead made the guard
fire exactly when the controller succeeded, since steering the projection to `target − 100` is the
controller's *goal* and was also the guard's *alarm*. Observed: 34 risk trips and 14 `mode=Pause`
sheds in 90 minutes, each dumping the Buzz from ~20 A to 6 A and swinging P1 by ~4 kW. A guard is a
backstop for when control has failed, not a second copy of the control limit.

This also settles the margin's job: it now covers **only** a disturbance appearing inside the
controller's reaction window — 3 kW × 30 s / 900 s ≈ 100 W, exactly its value. The margin was
under-sized for the static law and is correctly sized for this one.

**The peak margin has one definition** — `dispatcher.decide.NET_LADEN_MARGIN_W`, interpolated into
the generated Jinja rather than written as a literal. It drives three separate laws (net_laden, the
reservation headroom, and the car's peak formula in `ev_modulator`), which previously carried a
hardcoded `200` in two files while only `decide.py` had the constant — four sites to move in lockstep
and parity covering only two of them. A test asserts the value never reappears as a literal.

Sizing: the margin absorbs what a sudden unexpected load (oven, kettle) adds to the **quarter-hour
average** before the 30 s modulator cycle and the car's ramp-down can trim it — ~3 kW for a minute
≈ 200 W of quarter-hour average. Measured 2026-08-07..28 over 2064 complete quarters: only 5 exceeded
the then-2683 W setpoint, worst overshoot **199 W** (2882 W against a 2883 W target — one watt
spare), and the target was never breached. **Now 100 W** by choice: +100 W (~0.43 A) of usable
charging at the current low ceiling, accepting that a worst-case disturbance can overshoot the target
by ~100 W and ratchet the month peak up by that much (~€0.50/month; the raise-only target then keeps
the extra headroom). Worth revisiting once the ceiling is raised — the margin costs a fixed number of
watts, so it is 3.5 % of a 2.883 kW ceiling but only 2 % of 5.0 kW. The battery
side is bounded by the REMAINING import headroom rather than taking the reservation on faith:
`ev_grid_laden` keys off intent (a car plugged and below target), not on the car actually drawing, so
without that bound an idle-but-plugged car would let the reservation stack on top of the real load and
push the peak.

The reservation may only claim surplus **above the car's 6 A floor**, and never decides whether we
charge at all: pause/charge is judged on the *unreserved* figure, then the reservation trims what is
left above 6 A. Subtracting it from the raw figure instead switched the car **off** rather than
throttling it down. `act` is pilot-*estimated* from the command for the Buzz (`evseW == cur × 230` —
there is no CT), so `act ≡ cur`, and once the car is eating its own surplus `besch → 0` and
`available` collapses to about `cur − resv/230`. At the 6 A floor that is 5.35 A → target 0 → mode
Off; with the car stopped the surplus reappears, `available` climbs back over 6 and it restarts —
40 Off transitions in an hour. A car cannot throttle below 6 A, so at the floor there is nothing to
concede, and stopping it to hand the battery 150 W throws away 1380 W of car charging.

### Cascade-yield liveness — `input_boolean.ev_hoger_laadstil`

The yield is based on *eligibility*, not "already drawing", so a car can bootstrap (the battery must
release the surplus before the car can ramp into it). That made the yield unconditional: a car that
**cannot** charge starved the battery indefinitely. On 2026-08-26 the Buzz was eligible all afternoon
while being disarmed every few minutes by integration dropouts — the battery held at 0 W on 96 % SoC
with 8.6 kWh of room and ~5 kW went to the grid. `ev_hoger_laadstil` latches on when a higher-ranked
car has been eligible but not drawing for **6 min** and clears the moment it draws, and the yield
gate ANDs `not stalled`. The bootstrap grace survives; a genuine stall falls through to the battery.
The car is never blocked while stalled — the modulator still commands it — so the latch self-clears.
Elapsed time is an explicit timestamp helper, not a template trigger with `for:` (a template trigger
starts its timer on a false→true *transition*, so a condition already true at load never fires).

## The surplus cascade (6 A modes)

Export is offered down the `zonprioriteit` list:
1. The **highest hungry consumer** ramps on the live surplus. An EV: 6 A → its max. The
   **primary** car (no plugged car above it) holds the 6 A bridge; a **secondary** car only runs
   on genuine ≥ 6 A surplus (no bridge — "to start an extra EV the excess must exceed 6 A again").
2. When it's maxed and export remains, the **next** consumer joins. The **battery** has no 6 A
   minimum, so it usually mops up the remainder.
3. A consumer **waits** while any higher-priority consumer is still hungry (other car plugged &
   not maxed, or battery ranked above it & SoC < 99).
4. The **airco dump** sits at the very bottom, below both cars and the battery. It starts only when
   the battery is ~full *and* no car still wants the surplus — a car that is plugged, below its
   target and not yet maxed WILL take it, so the dump waits. `ev_hoger_laadstil` is the escape
   hatch: a car that has been eligible but not drawing for 6 min has demonstrably given up, so the
   dump proceeds rather than waiting forever on a car that cannot charge.

   That cascade gate is the *only* reliable guard, because two of the dump's three triggers check
   the surplus **instantaneously**. Only `besch above threshold for 15 min` requires it to be
   sustained; the SoC trigger and the `/10` re-check read it at a single instant, and a car ramping
   from 6 A to 20 A produces exactly that transient. On 2026-08-26 the SoC trigger fired two minutes
   after SoC hit 99, read 998 W at that instant, and started the aircos — while the Buzz, restarted
   37 s earlier, ramped 1932 → 3772 → 4715 W and had the house importing 90 s later.

**Unavailable components are transparent.** A higher-priority consumer only blocks the cascade
if its charger is actually **reachable** (`has_value` on its command entity). "Wants charge" is
read from the *car's* telemetry, which stays valid while the *charger* is offline — so without
this an offline charger (e.g. the Tuya cable drops out) would make the battery/next car defer to
a consumer that can't be commanded, and all the surplus would export. Gating eligibility on
reachability makes an offline charger fall through to the next consumer; an offline charger also
disarms its own target (0 A).

**Bootstrap (no deadlock):** the battery yields based on the **gross** solar surplus
(`pv_bruto_overschot`, = PV − house) being above the threshold (6 A for `Zon only`, small for the
6 A modes) while a higher EV is *plugged* — i.e. it stops charging *before* the car draws, so the
export appears and the car can start. (`pv_overschot_beschikbaar` alone reads ~0 while the battery
is eating the solar, which is why the yield can't be gated on "already charging".)

Implemented in `ev_modulator.py` (shared by both chargers); `decide.py` carries the battery's
half (yield / mop-up).

## Battery brain (`dispatcher/decide.py`, mirrored in `live_template.py`, parity-tested)

```
d = battery_power + p1 + bias                    # self-consumption
if a higher-priority EV is hungry (6A modes):
    van batterij -> d = max(0, d)                # yield surplus, may feed the bridge
    else         -> d = min(0, d)                # charge leftover export, never feed the car
if net_laden and soc < 100:                      # grid-charge-to-full override
    d = batt + p1 - (maandtarget - 200)          # drive import to the peak target,
    d = max(d, -phase_cap)                        #   capped by live per-phase headroom (->27A)
if soc >= 100: d = max(0, d)                      # ceiling
if soc <= floor: d = min(0, d)                    # reserve floor: input_number.batterij_reserve_soc
                                                  #   (default 40%), 15% during 06–10h morning peak.
                                                  #   Lower it on the dashboard to use the battery now
                                                  #   (e.g. an expensive evening) instead of saving it.
clamp(-5000, d, +3000)
```

**Bias** is a slow integral (`KI = 0.1`, 20 W deadband, ±200 W clamp) that cancels the inverter's
steady-state under-delivery. It only accumulates in plain self-consumption (frozen while yielding,
grid-charging, or saturated). The deadband + low gain stop it chasing P1 noise.

## Safety (always on, independent)

- **`breaker_guard`** — every 5 s, on **measured** conductor current (3×32 A main). Two-stage:
  at **> 28 A** it first **throttles** the charging cars down fast (one proportional step aims the
  worst conductor back to ~26 A — so a sudden airco/washing-machine load trims the car instead of
  killing the session); only at **> 29 A with a car already at its 6 A floor** does it **hard-cut**,
  in order Buzz → EV3 → battery grid-charge. Uses real per-phase *current* (per-phase *power* is
  misleading in a 230 V delta), so it needs no wiring assumptions.
- **Dynamic per-phase cap** on every ramp (EV modulators + net_laden) keeps the worst conductor
  ≈ 27 A proactively; the guard is the hard backstop. It measures the **import direction only**
  (each phase clamped at 0). It used to take `|abs` of each phase, which made *export* eat the
  charger's headroom — backwards: adding charger load while exporting drives the conductor current
  *toward* zero (exporting 19 A and drawing 10 A leaves 9 A, not 29 A). With `|abs`, a strong solar
  day pushed `cur + (27 − maxphase)` below 6 and disarmed the charger exactly when the surplus was
  largest, and charging would have relieved the very current the guard reacted to. The live reading
  dropped 18.9 A → 8.2 A on the switch. The thermal case (a conductor loaded hard in *either*
  direction) stays `breaker_guard`'s job and is unchanged.
- **Peak guard** — keeps the month-peak projection under target. Two separate mechanisms, not
  mixed with price:
  - **Settable target (raise-only):** `sensor.capaciteit_maandtarget` = `max(2.5, peak-so-far-this-
    month, input_number.capaciteit_maandtarget_instelling)`. Out of the box the target just ratchets
    to the peak already hit; the `…_instelling` helper lets you *raise* the ceiling above what's
    already spent, so grid-charging (`net_laden` / `Max onder piek`) may push harder toward a peak
    you accept. It can't lower the target below a peak already recorded or below 2.5 kW. Source:
    `build_capaciteit_target.py`.
  - **Throttle (continuous):** the grid-charge LEVEL self-limits to stay under the peak —
    `net_laden` and the EV `Max onder piek` both cap import at `maandtarget − 200 W`. `net_laden`
    uses the **instantaneous P1** for that cap, so it backs the battery off fast when a load
    (airco/washing machine) is added — a throttle, not a stop. `goedkoop_laden` decides *whether*
    to charge purely on **price**; the peak only throttles the *level*.
  - **Shed (backstop):** if the projection still climbs, `peakguard_afschakelen` sheds flexible →
    comfort: Buzz → EV3 → battery discharge-boost → aircos (last resort).

## Tests

`dispatcher/tests/`: `test_decide.py` (rules), `test_scenarios.py` (worked-example cascade),
`test_bias.py`, `test_parity.py` — the last two render the **deployed Jinja** against HA and
assert it equals `decide.py`, so the live brain can't silently drift from this spec.
