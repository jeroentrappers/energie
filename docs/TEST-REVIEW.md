# Test review

Every test in `crates/core` (the controller logic) was read against the live Home Assistant
config it ports (`the earlier configuration` at the time of the review). The catalogue at the end
lists each one with the rule it pins down; this page records how the behaviour was confirmed.
The section "Updated for the 2026-11-07 HA fixes" lists the tests that changed when the port
moved to the original configuration.

## How the behaviour was confirmed

| Check | What it shows | Result |
| --- | --- | --- |
| Read each test against the live YAML/Jinja | The rule a test claims is the rule HA runs (operator, threshold, order of the `choose` branches, trigger semantics) | 413 core tests, all consistent; hand-worked numbers re-derived for the quarter budget, projection, reservation, phase cap, k-factor, breaker/peak-guard payloads, trickle and clamps |
| Live template renders (read-only `POST /api/template`) | The Rust value equals what HA's own Jinja computes for the same inputs | Battery setpoint: 17 of 17 scenarios equal. Buzz target: equal on the live state (20.2 A). Floor rounding: `2.3|round(2,'floor')` = 2.29, `((2300/230+4)*1.4)|round(1,'floor')` = 19.5, as the port now does |
| Live snapshot fixture | Every template sensor reproduces the state HA showed at one instant | 59 numeric fields plus the flags equal (`derived::tests::live_snapshot_2026_11_07_reproduces_every_template_state`) |
| Mutation testing (`cargo mutants`, see `MUTANTS.md`) | Each comparison, branch and arithmetic step is pinned by some test | 1923 caught, 24 missed (all shown equivalent), 69 unviable, 6 timeouts |
| History replay (see `REPLAY.md`) | On 4.6 days of real recorder data the engine decides what HA decided | 99.75 % of field samples agree; battery RC timeline 98.5–99.9 %; latches 100 %; Buzz current within 0.5 A at every HA decision |
| Coverage | Nothing untested | Every line executed (lcov, 14 507 lines after the fixes); region coverage 99.4 % |

## Bugs the review found and fixed

| Where | Bug | Found by |
| --- | --- | --- |
| `jinja::round` | Multiply-round-divide disagreed with Python's `round()` on ~1.5 % of inputs | Driver work (price rounding); now 0 mismatches on 20 000 random CPython cases |
| `jinja::round_floor` | Nudged values just below a step up before flooring; HA floors the raw product (19.599… → 19.5 A, not 19.6) | Mutation testing, confirmed by a live render |
| `ev_laden_buzz` | Ignored the availability template of `sensor.ev_laadvermogen_smartevse_w` | Merging the derived-sensor port |
| Peak guard / Buzz | Read the mode-change stamp under the HA entity name instead of the automation id, which would have disabled the 300 s session protection | Guards port |
| Meters and templates | Integrated per tick instead of per source change; re-rendered `now()` templates every tick; P1 meters updated after the templates; the P1 median sampled per tick | History replay |
| `sun.sun` | Flipped at astral's sunrise/sunset; HA samples the refracted elevation every 2 min from civil dawn / sunset | History replay; now every recorded transition matches to the second |

## Live behaviour that looks wrong

Reproduced faithfully, listed in `FINDINGS.md`, each marked `// LIVE:` in the code and pinned by a
test where it is observable. Most were fixed in HA itself on 2026-11-07; the port followed (next
section). What is still open keeps its `// LIVE:` marker.

## Updated for the 2026-11-07 HA fixes

The tests that pinned the old behaviour now pin the new one, with hand-worked numbers from the
fixed YAML/Jinja (the original configuration). 374 core tests before, 388 after.

| Rule | Tests (new or rewritten) |
| --- | --- |
| `peakguard_herstel` `below: 2000` | `the_boost_is_cancelled_after_5_min_below_2_kw`, `a_projection_of_1500_w_cancels_the_boost_and_2500_w_does_not` |
| `peakguard_afschakelen` kW message, EV3 `has_value` | `the_start_note_prints_the_projection_in_kw_with_2_decimals`, `an_unavailable_cable_current_stops_the_cable`, `the_peak_guard_sheds_the_buzz_first_after_10_s_of_risk` (3.14 kW) |
| `breaker_guard` Buzz step-down needs `Charging` | `an_idle_buzz_override_does_not_shadow_the_ev3_step`; the Buzz-step tests now set `Charging` |
| `nachtdekking_eind` real elapsed `duur` | `the_duration_is_real_elapsed_time_from_22_00` (16.0 h / 14.0 h on the DST nights) |
| `goedkoop_laden` pilot B/C | `yields_the_grid_turn_to_a_wanting_car` |
| `batterij_autonoom_netladen_stop` message | `cuts_grid_charge_after_20_s` |
| `new_bias` frozen on `ev_grid_parked` | `bias_is_frozen_while_cars_are_parked` |
| Setpoint `net_laden = flag and soc < 100` | `net_laden_at_100_leaves_the_cascade_branches_on` |
| Modulators: reservation after the `< 6` gate, `resv_on`, cable `max` | `the_reservation_shortfall_never_disarms_a_floor_car_on_the_phase_cap`, `the_reservation_shortfall_trims_the_cap_above_the_floor`, `reservation_terms_and_soc_100`, `buzz_view_other_maxed_is_the_cable_at_its_max`, `live_snapshot_grid_max_matches_ha` (23.2 A with the fixed reservation) |
| `ev_laden_cable` start retry every 30 min | `a_charged_ev3_is_restarted_at_most_every_30_min`, `end_to_end_the_start_script_is_rate_limited_on_a_charged_ev3` |
| Unplug triggers `to: Disconnected` | `a_plug_blip_does_not_reset_but_a_return_as_disconnected_does` (both automations) |
| `buzz_mode_stempel` against the last valid mode | `stamps_a_real_mode_change_only`, `a_change_to_the_known_mode_is_not_stamped` |
| `ev_cable_autoheal` `/30` retry | `reconnects_after_ten_minutes_and_retries_every_half_hour`, `unavailable_at_start_is_healed_by_the_half_hour_pattern`, `the_pattern_needs_600_s_of_unavailable` |
| `entsoe_qh_kwartier_verversen`, `vriezer_bewaking` off | `it_runs_five_seconds_past_each_quarter_and_sends_nothing`, `the_registry_holds_the_29_enabled_live_automations_by_their_ha_ids` |
| `Batterij reservering effectief` 0 at SoC 100, fallback to the target | `a_full_battery_reserves_nothing`, `the_reservation_falls_back_to_the_month_target_without_an_allowance` |
| `Kwartier toegestaan vermogen` unknown without the meter | `without_the_meter_the_allowance_is_unknown_and_the_average_uses_the_clock` |
| `Gefactureerde piek` `split(':', 1)` | `gefactureerde_piek_rolls_twelve_months` (3.0 kW) |
| `Zelfvoorziening vandaag` | `zelfvoorziening_is_own_solar_over_own_solar_plus_import`; the live snapshot now expects 12 % (HA showed 0 % with the old formula) and a 0 W reservation (SoC 100) |
| Meters `periodically_resetting: false` | `a_non_resetting_meter_counts_the_gap_against_the_last_valid_reading`, `a_decrease_across_the_gap_is_dropped_without_net_consumption`, `a_periodically_resetting_meter_rebaselines_after_an_unavailable_source`, `utility_meter_holds_on_an_unknown_source` |

In `crates/app`: `memory_seeds_the_mode_helper_and_the_cable_start_script` (`ha::mapping`),
`latch_flips_become_acts` (the mode latch), `control_mode_actuates_and_records` (a note with a
`notification_id` replaces the earlier one).

## What the tests do not cover

- Real hardware for Solis, the Tuya cable and the SmartEVSE commands: the drivers are tested
  against protocol simulators only (a second Modbus master or Tuya session would collide with HA).
- Cloud integrations (Kia, VW, LG ThinQ, Fluvius): read through Home Assistant.
- EMHASS (shadow-only in HA, not ported).

## Catalogue


### `automations/airco_dump.rs` (8)

- `the_dump_starts_on_the_ten_minute_recheck` — `airco_dump_start` via the `/10` re-check: both units on, the session latched, and the surplus printed as HA's integer state.
- `the_airco_dump_waits_for_a_car_that_will_still_take_the_surplus` — LOGIC.md cascade item 4: "a car that is plugged, below its target and not yet maxed WILL take it, so the dump waits." EV3 plugged at 10 A waits; at the fixed 16 A maximum it no longer does. Same for the Buzz: override 27.4 A again
- `a_stalled_car_does_not_block_the_dump` — LOGIC.md cascade item 4: "`ev_hoger_laadstil` is the escape hatch: a car that has been eligible but not drawing for 6 min has demonstrably given up, so the dump proceeds."
- `every_start_condition_blocks_on_its_own` — `airco_dump_start` conditions, each one failing on its own: switch off, peak risk, SoC not above 98 (or unavailable), surplus not above 800, outside May–September, outside 10:00–18:30 (18:30 itself excluded, 10:00 included), a uni
- `a_surplus_held_for_15_min_starts_the_dump` — `airco_dump_start` trigger 1: surplus above the threshold for 15 min, then the start runs without waiting for the `/10` pattern. The hold starts at 14:00:31 and ends at 14:15:31; the 14:10 re-check in between is blocked by a peak 
- `a_full_battery_held_for_2_min_starts_the_dump` — `airco_dump_start` trigger 2: SoC above its threshold for 2 min. The threshold is the helper's live value (here lowered to 95 %).
- `the_dump_stops_when_the_surplus_is_gone_for_10_min` — `airco_dump_stop` `overschot_weg`: surplus below −300 W for 10 min ends a running session; the trigger id is in the message.
- `the_dump_stops_at_18_30_but_leaves_manual_aircos_alone` — `airco_dump_stop` `einde_venster` at 18:30, and only for a dump session: aircos a person switched on by hand are left alone.

### `automations/batterij_autonoom_netladen_stop.rs` (3)

- `cuts_grid_charge_after_20_s` — Template trigger `for: 20 s`: switch off and notify with the raw states, once.
- `rechecks_every_minute` — `time_pattern minutes: /1` re-checks while it persists (06:01:00).
- `conditions_gate_the_minute_recheck` — Conditions: the switch must be on and the template true at run time; each clause of the template matters (−1200, 2500, −200 are strict).

### `automations/batterij_naar_auto.rs` (8)

- `budget_delivered_sets_the_level_back_to_uit` — Entering snapshots the counter; 5 kWh later the level goes back to Uit.
- `the_reserve_floor_ends_the_dump` — The SoC reaching the floor ends the dump too (and says so).
- `the_morning_floor_is_15` — In the 06–10 h morning window the floor is 15 % whatever the setting.
- `an_unmeasurable_counter_only_ends_on_the_floor` — Without the counter (`meetbaar` false) the budget never ends the dump, the floor still does; an unknown SoC reads 100.
- `only_acts_in_ontlaad_budget` — Not in Ontlaad-budget: a check does nothing, even past the budget.
- `check_triggers` — Each check trigger runs it; nothing between the minute marks otherwise.
- `enter_and_check_in_one_tick` — Entering at or below the floor: snapshot, then the same tick's check ends it.
- `python_float_formatting` — 

### `automations/breaker_guard.rs` (12)

- `the_breaker_guard_throttles_before_it_cuts` — LOGIC.md Safety: "at > 28 A it first throttles the charging cars down fast (one proportional step aims the worst conductor back to ~26 A)". Buzz at 16 A, worst phase 30.5 A: 16 − (30.5 − 26) = 11.5 → round → 12 (ties to even) → pa
- `the_buzz_step_rounds_to_whole_amps_ties_to_even_before_tenths` — `breaker_guard` payload chain `[6, ov − (mp − 26)]|max|round(0)` then ×10: the step rounds to whole amps with ties to even before the tenths. Override 15.0, phase 30.5: 10.5 → 10 → 100 (not 105, not 110). Override 14.3, phase 28.4
- `export_current_arms_the_guard_and_the_ev3_step_follows_the_buzz` — The guard looks at the conductor in either direction (`|abs`): 29.5 A of *export* on one phase arms it like import would. EV3 at 13 A, cable drawing: 13 − 3.5 = 9.5 → 10 (ties to even). Buzz override at 6 A does not take the Buzz 
- `at_exactly_28_a_the_guard_stays_idle` — Exactly 28 A does not arm (`> 28`).
- `a_car_already_at_6_a_is_cut_only_above_29_a` — LOGIC.md Safety: "only at > 29 A with a car already at its 6 A floor does it hard-cut". Buzz charging at 6 A: 29.0 A only arms; 29.4 A pauses it, and the message prints the phase rounded to an integer (29.4 → 29).
- `the_ev3_is_cut_when_the_buzz_is_not_charging` — Cut order Buzz → EV3 → battery (`breaker_guard` choose): with the Buzz not charging the EV3 at its floor (6 A, 1.4 kW) is stopped; 30.5 A prints as 30 (ties to even).
- `battery_grid_charge_is_the_last_thing_cut` — Last in the cut order: grid-charging the battery is switched off (and stays off until someone turns it back on). A cable between 0.2 and 0.3 kW is neither throttled (already at 6 A) nor cut (≤ 0.3 kW).
- `the_guard_runs_every_5_s_or_on_a_phase_change` — Triggers: `/5 s` or a change of any phase. Between 5 s boundaries with steady phases nothing runs; a phase change runs it at once.
- `an_idle_buzz_override_does_not_shadow_the_ev3_step` — `breaker_guard` Buzz step-down requires `sensor.smartevse_1234_state == Charging`: an idle Buzz with a 16 A override is skipped and the charging EV3 is stepped down instead (13 − (30 − 26) = 9 A); with an unavailable Buzz state li
- `a_cable_at_exactly_0_2_kw_is_not_stepped_down` — `breaker_guard` choose, EV3 step: `ev_charging_cable_vermogen > 0.2`. A cable at exactly 0.2 kW is not stepped down; at 29.5 A nothing else matches either (Buzz idle, cable ≤ 0.3 kW, no grid charge), so the run is empty.
- `the_ev3_cut_needs_strictly_above_29_a_and_0_3_kw` — `breaker_guard` choose, EV3 cut: `maxphase > 29 and cable > 0.3`. At exactly 29 A or with the cable at exactly 0.3 kW the EV3 at its floor is not cut; the cut bumps `counter.peak_guard_interventies` by one.
- `the_battery_cut_needs_strictly_above_29_a_and_grid_charging_on` — `breaker_guard` choose, battery cut: `maxphase > 29` *and* `batterij_net_laden` on. At exactly 29 A grid-charging stays on; above 29 A with grid-charging already off there is nothing to cut and nothing is counted.

### `automations/buzz_handshake.rs` (5)

- `first_time_cycles_the_mode_once` — First time: count it, Pause, then 30 s later Normal and a notification.
- `second_time_pauses_and_asks_for_a_replug` — Stuck again after the cycle (counter 1): Pause for good and ask for a replug.
- `a_trigger_during_the_delay_is_dropped` — `mode: single`: a new 5-minute stuck period that completes during the 30 s delay is dropped (and, being a transition trigger, does not fire again later in that run).
- `conditions` — Each condition on its own blocks the run.
- `already_stuck_at_start_does_not_fire` — Already stuck when we start: does not fire (a transition trigger).

### `automations/buzz_laadweigering_reset.rs` (4)

- `resets_when_the_pilot_reaches_c` — 
- `resets_on_unplug` — 
- `a_plug_blip_does_not_reset_but_a_return_as_disconnected_does` — `to: Disconnected`: a Connected → unavailable blip (and back) keeps the counter; a blip that comes back as Disconnected (unavailable → Disconnected) is an unplug and resets it.
- `needs_a_count_and_a_transition` — Already 0: the condition fails, no run is recorded. Staying in C does not re-fire.

### `automations/buzz_mode_stempel.rs` (2)

- `stamps_a_real_mode_change_only` — `buzz_mode_stempel`: stamped when the select changes to a valid mode other than the helper's; a blip back to the same mode is not, a change across a blip is, and the helper follows the stamped mode.
- `a_change_to_the_known_mode_is_not_stamped` — The helper, not the previous state, decides: a select change to the mode the helper already holds (e.g. Normal → Solar with the helper still at Solar after a restart) stamps nothing; without a trigger a differing helper is left al

### `automations/buzz_reset_op_uitplug.rs` (3)

- `unplug_parks_the_override_at_six` — 
- `conditions` — Already at 6 A, or the override itself unavailable: nothing.
- `a_plug_blip_does_not_reset_but_a_return_as_disconnected_does` — `to: Disconnected`: a blip of the plug alone (override still readable, 2026-11-07 12:57:30) publishes nothing; the plug returning from `unavailable` as `Disconnected` is an unplug and parks the override.

### `automations/dagbalans.rs` (1)

- `the_day_report_rounds_meters_to_tenths_and_truncates_percentages` — `dagbalans_rapport`: at 23:55 the daily meters are rounded to 0.1 kWh (ties to even, printed as floats) and the percentages truncated to integers; two lines.

### `automations/dispatcher.rs` (11)

- `runs_every_30_seconds` — `time_pattern seconds: /30` fires on :00 and :30, not in between.
- `discharge_branch_commands_in_order` — Discharge branch (`sp > 30`): timer, RC timeout 10, bias, then grid-charge off, force discharge, discharge power, `soluna_absorptie_bezig` on. Bias: plain tracking, P1 500 → 0 + 0.1·500 = 50.
- `charge_branch_commands_in_order` — Charge branch (`sp < -30`): grid-charge on, force charge, `charge_w = max(0, −sp)`.
- `hold_branch_between_minus_30_and_30` — Hold branch (`-30 <= sp <= 30`): grid-charge off, force charge at 0 and discharge 0, `soluna_absorptie_bezig` off. Both edges ±30 hold.
- `does_nothing_while_disabled` — `condition: input_boolean.soluna_absorptie_actief on`: off → nothing, bias untouched.
- `p1_above_5800_triggers_once_on_the_crossing` — `numeric_state sensor.p1_meter_vermogen above: 5800`: fires on the crossing only.
- `phase_excursions_trigger_on_every_conductor` — Each phase `above: 28` and `below: -28` triggers; unavailable never matches but re-arms.
- `a_phase_at_exactly_28_a_is_not_a_crossing` — `numeric_state` `above: 28` / `below: -28` are strict: a phase at exactly ±28.0 A is no crossing; ±28.01 A is.
- `crossing_while_disabled_is_not_replayed` — A crossing while disabled is consumed: re-enabling does not replay it.
- `bias_is_frozen_at_the_floor` — `new_bias` freezes outside plain tracking: SoC at the 15 % floor → `bias|round(1)`.
- `bias_reads_the_median_and_the_stored_bias` — `new_bias` reads the median P1 and the current memory: median 300 → 0.1·300 = 30 on top of the stored −10.

### `automations/dispatcher_off_target_alert.rs` (3)

- `alerts_after_ten_minutes_off_target` — `for: 00:10:00`: notify and log once after ten minutes on, with the raw states.
- `needs_a_transition_and_prints_unavailable` — Already on at start: a `state to:` trigger needs a change, so no alert (`trigger::Held`). Unavailable battery power prints `unavailable` and counts 0 in the deviation.
- `the_deviation_is_battery_minus_setpoint` — `dispatcher_off_target_alert` message: `afwijking` is `|batt − setpoint|`. Battery charging 400 W (−400) against a 1500 W discharge setpoint is 1900 W off.

### `automations/entsoe_qh_refresh.rs` (1)

- `it_runs_five_seconds_past_each_quarter_and_sends_nothing` — `entsoe_qh_kwartier_verversen`: fires at hh:00:05, :15:05, :30:05, :45:05 (recorded as `last_triggered`), never on the start tick, and sends no command (the native driver already serves the current quarter).

### `automations/entsoe_refresh.rs` (2)

- `prices_are_refreshed_every_2_min_until_both_feeds_have_tomorrow` — `entsoe_dayahead_morgen_ophalen`: on the even minute inside 13:00–23:59, refresh while either feed lacks tomorrow; stop once both have it.
- `a_start_inside_the_window_refreshes_at_once` — The `homeassistant: start` trigger: a restart in the window refreshes at once; one at 23:59:00 or later is past `before: 23:59:00` (HA's `time` condition: exclusive).

### `automations/ev_cable_autoheal.rs` (4)

- `reconnects_after_ten_minutes_and_retries_every_half_hour` — `ev_cable_autoheal`: 10 min unavailable reloads the cable (12:00:15 → 12:10:15); while it stays down the `/30` pattern retries at 12:30:00, not at the 10 min mark after the first heal.
- `only_unavailable_counts_and_must_hold` — The explicit `unavailable` string counts; `unknown` and a blip back do not.
- `unavailable_at_start_is_healed_by_the_half_hour_pattern` — Unavailable already at start (12:00:10): `last_changed` is the start, so the first `/30` pattern (12:30:00, 1790 s later) heals it.
- `the_pattern_needs_600_s_of_unavailable` — The pattern alone needs the condition: the cable up, or down for less than 600 s (12:25:00 → 12:30:00 is 300 s), does nothing; the 10 min hold heals it at 12:35:00.

### `automations/ev_hoger_laadstil.rs` (8)

- `latches_after_six_minutes_and_clears_when_drawing` — Stamp the stall on the start tick, latch after 360 s with one notification, clear and unstamp the moment the car draws.
- `exactly_360_s_latches` — 360 s exactly is enough (`>=`).
- `the_stamp_is_rounded_epoch_seconds` — The stamp is `as_timestamp(now())|round(0)`: 12:00:10.6 → …11.
- `eligibility_needs_a_plugged_car_above_the_battery` — Not eligible: car not ranked above the battery, or not plugged.
- `opportunity_is_surplus_or_cheap_grid` — Opportunity: gross surplus > 200 W, or cheap Max tot piek.
- `either_car_drawing_is_not_a_stall` — Drawing on either charger is not a stall: cable > 0.2 kW or SmartEVSE > 200 W.
- `triggers` — Each listed state change triggers a run; nothing else between the minute marks.
- `id` — 

### `automations/ev_laden_buzz.rs` (13)

- `publishes_the_target_in_tenths_over_mqtt` — Normal and a different override: publish the target in tenths (13.0 A → 130).
- `waits_while_the_smartevse_power_sensor_is_unavailable` — Condition `has_value('sensor.ev_laadvermogen_smartevse_w')`: while that sensor's availability template is false the modulator does not run (and records no run).
- `tenths_payload_rounds` — The tenths payload is `(target × 10)|round(0)|int`: 12.2 → 122.
- `no_publish_within_five_hundredths` — No publish when the override is within 0.05 A of the target; 0.1 A apart is published.
- `normal_is_offered_only_60_s_after_the_last_mode_change` — Not Normal: offer Normal only ≥ 60 s after the last real mode change, and publish nothing until the charger reports Normal.
- `the_pause_timer_is_the_mode_stamp` — The minimum-pause timer reads `buzz_mode_stempel`'s last run; never stamped = 99999 s.
- `stop_parks_at_six_and_pauses_when_allowed` — Stop branch: park the override at 6 A (payload 60), and Pause when allowed.
- `pause_is_not_sent_within_300_s_while_charging` — Pause is not sent within 300 s of the last mode change while charging in Normal: the car is throttled to 6 A instead.
- `end_to_end_stop_respects_session_protection` — End to end: no sun → the session is stopped, but only after its 300 s.
- `end_to_end_risk_throttles_then_stops` — Risk: 6 A at once, Pause only once risk has been on > 60 s (and the session is old).
- `triggers` — Triggers: the /30 s pattern and state changes; nothing in between.
- `conditions` — Conditions: 25 s debounce on its own last run, a reachable override and a known state.
- `id` — 

### `automations/ev_laden_cable.rs` (12)

- `sets_the_rounded_current_while_drawing` — Drawing: set the new current when it differs (10.0 → 10 A).
- `whole_amps_round_ties_to_even` — `round(0)`, not `int`: 9.8 → 10; ties to even: 8.5 → 8, 9.5 → 10; never below 6.
- `no_command_when_the_current_already_matches` — Same current: nothing to do. Unknown set current (|float(-1)) is re-sent.
- `not_drawing_starts_the_cable` — Not drawing (power ≤ 0.2 kW, whatever the current reads): start the cable at the amps.
- `a_charged_ev3_is_restarted_at_most_every_30_min` — `ev_laden_cable` start branch: a `charged` EV3 that does not draw is sent the start script only if it never ran or ran more than 1800 s ago; otherwise the else branch sets the current (8 → 10 A). Any other status starts at once, h
- `end_to_end_the_start_script_is_rate_limited_on_a_charged_ev3` — End to end: the start is recorded as the script's `last_triggered`, so the next run 30 s later with the EV3 still `charged` and idle only re-sets the current; 1801 s after the start it is sent again.
- `stop_stops_and_parks_at_six` — Stop branch: stop if drawing, and park the set current at 6.
- `end_to_end_yields_to_a_hungry_higher_car` — End to end: the Buzz ranked above and plugged, not maxed → the EV3 waits (stops).
- `triggers_on_cable_status` — Triggers: the /30 pattern, the shared state changes, and the cable status.
- `conditions` — Conditions: 25 s debounce and a reachable set-current entity.
- `risk_long_stops_the_cable` — Risk is tracked by the cable modulator too (it may run without the Buzz one).
- `id` — 

### `automations/ev_modulator.rs` (50)

- `buzz_view_reads_the_smartevse` — The per-charger table in the module docs, Buzz column.
- `buzz_view_other_maxed_is_the_cable_at_its_max` — `ev_laden_buzz` `other_maxed`: the cable set current `>= state_attr(cable, 'max')|float(16)`. 15 A with a max of 13 is maxed; without the attribute the max reads 16, so 15 is not and 16 is; an unavailable set current reads 0.
- `cable_view_reads_the_tuya_cable` — The per-charger table, cable column: current-based `started`/`drawing`, `act` in A, own draw = current × voltage.
- `cable_view_other_maxed_is_buzz_max_minus_half` — The cable sees the Buzz maxed at `override >= max − 0.5` (0.1 A resolution).
- `plain_solar_ramps_at_most_3_a_per_run` — Solar: avail = (besch/230 + act) floored to 0.1 A, capped at cur + 3 while ramping. 1150/230 + 10 = 15.0 → capped to 13.0 (ramp step).
- `available_adds_the_cars_own_draw_back` — The absolute signal: avail adds the car's own draw back. besch −230 W with act 10 A → 9.0 A, whatever the stale command says.
- `available_floors_to_a_tenth` — `round(1, 'floor')`: 10 A + 100 W/230 = 10.43 → 10.4.
- `plain_solar_needs_six_amps` — A plain-solar car (any rank) needs a genuine 6 A: 5.9 A → 0.
- `solar_needs_sun_and_gross_surplus_with_start_hysteresis` — `solar` needs the sun above the horizon and pv_bruto > 150 W while drawing, > 800 W to start.
- `a_new_session_starts_at_six` — A session that has not started gets 6 A, not more (ramp_cap = 6).
- `the_charger_max_caps_the_target` — The charger max caps the target: Buzz max 11 A.
- `the_phase_cap_counts_import_only` — Per-phase cap: `cur + (27 − maxphase)`, import direction only. 10 + 27 − 25 = 12; a 30 A *export* phase does not eat headroom.
- `k_factor_scales_a_car_that_under_draws` — A car that under-draws its command is scaled up by cur/act: cur 10, act 8 (1840 W), besch 0 → 8 × 1.25 = 10.0, so it keeps its 10 A instead of decaying to 8.
- `k_factor_is_capped_at_1_4` — k is capped at 1.4: cur 10, act 5 → k 1.4, avail = 5 × 1.4 = 7.0.
- `k_factor_needs_drawing_at_least_4_a_and_cur_at_least_act` — k is 1 when act < 4 A, when not drawing, or when the car over-draws (cur < act).
- `a_lower_ranked_batterys_charge_is_added_back` — A lower-ranked battery's charge is added back (it mops up behind the car): battery charging 690 W adds 3 A. A higher-ranked battery's charge is not added.
- `the_reservation_trims_above_six` — The reservation trims avail above 6 A: 15.0 − 700/230 (3.04) = 11.96 → 11.96 (rounded at the end: 12.0).
- `the_reservation_never_stops_a_car_at_the_floor` — The reservation never stops a car at the 6 A floor: avail_raw 6.5 − 3.04 = 3.46 is lifted to 6, and avail_raw 5.9 stays 5.9 (judged unreserved) → stop.
- `reservation_terms_and_soc_100` — `resv_on` (toggle and soc < 100) gates both `resv_target` (phase shortfall and peak formula) and `resv` (solar trim): at SoC 100 nothing is held back (avail 10.0); at 99.5 the 460 W (2 A) trims avail to 8.0.
- `the_reservation_shortfall_never_disarms_a_floor_car_on_the_phase_cap` — `hard_max = [6, hard_max - resv_short_a]|max if hard_max >= 6`, after the gate's cap: the reservation shortfall never stops a car at the floor on a phase-limited import. cur 6, maxphase 24.5 → cap 6 + 2.5 = 8.5; 8.5 − 700/230 = 5.
- `the_reservation_shortfall_trims_the_cap_above_the_floor` — Above the floor the shortfall trims the cap in full: cur 10, ramp cap 13, a 690 W reservation with the battery idle is 3.0 A short → hard_max 10.0; a cap already under 6 (cur 6, maxphase 27.5 → 5.5) is left alone and gates the car
- `the_gates_stop_the_car` — Each gate alone forces 0: not reachable, not plugged, master off, not allowed.
- `a_plug_blip_reads_the_latch` — A plug blip falls back to the latch: plug unavailable but latch Connected still charges.
- `a_higher_ranked_battery_holds_the_car_below_99` — A battery ranked above the car holds it until SoC 99.
- `a_higher_ranked_hungry_car_holds_the_lower_one` — A higher-ranked car that is plugged and not maxed holds the lower one; once maxed the lower car may join (as a secondary car).
- `an_offline_higher_priority_charger_is_transparent` — An offline higher-priority charger is transparent: the EV3 is plugged (car telemetry) but the cable is unreachable, so the Buzz is allowed and primary.
- `a_stalled_higher_car_releases_the_cascade` — A stalled higher-ranked car (`ev_hoger_laadstil`) does not hold the lower one.
- `a_lower_ranked_car_does_not_hold_or_demote` — A lower-ranked plugged car does not make this one secondary.
- `risk_clamps_to_six_at_once_and_stops_only_after_60_s` — Risk clamps to 6 A immediately but only stops after 60 s (strictly more).
- `risk_without_a_timestamp_is_not_long` — Risk without a known `last_changed` never counts as long.
- `grid_max_steers_to_the_quarter_hour_allowance` — 
- `grid_max_falls_back_to_the_month_target` — Without an allowance the flat month target is used: 3.9 kW → (3900 − 1700)/230 = 9.56 → 9.5.
- `grid_max_is_price_gated` — The price gate: at the threshold (not strictly below) grid_max is off and, without sun, the car stops.
- `grid_max_load_excludes_the_battery` — `load` takes the battery out: discharging 460 W adds to the house load (−2 A), charging 460 W is not house load (+2 A).
- `grid_max_yields_the_larger_reservation` — The reservation under the peak: the larger of the manual target and the automatic `batterij_reservering_effectief` comes off. 460 W manual → −2 A; effective 690 W → −3 A.
- `grid_max_needs_ten_to_start_six_to_keep` — Starting needs 10 A of room, keeping a session needs 6.
- `grid_max_cable_load_uses_current_times_voltage` — The cable's own draw in `load` is current × voltage: P1 4000 with the EV3 at 8 A × 240 V → load 2080; (3837 − 2080)/230 = 7.63 → 7.6 A.
- `grid_max_comes_before_the_dump_and_needs_no_sun` — Max tot piek ignores the sun (night charging) and takes precedence over the dump.
- `live_snapshot_grid_max_matches_ha` — Live render of the Buzz `target` on 2026-11-07 ~13:10 (POST /api/template): Max tot piek at 0.3787 < 0.38, allowance 5265 W, P1 4557 W with the Buzz drawing 4646 W (load −89 W), SoC 100 so `resv_target` 0 but `batterij_reservering
- `dump_primary_adds_the_discharge_and_holds_the_floor` — Ontlaad-budget, primary car: at least 6 A, day or night; `avail_dump` adds the battery's discharge back. besch 0, discharging 1150 W: 1150/230 + 10 = 15.0 → ramp cap 13.
- `dump_scales_an_under_drawing_car_by_k` — `avail_dump = (((besch + batt_dis) / 230 + act) * k)|round(1, 'floor')`: the dump scales an under-drawing car by k like `avail` does. cur 10, act 8 (1840 W), besch 0, battery idle → 8 × 1.25 = 10.0.
- `dump_secondary_needs_six_amps` — Ontlaad-budget, secondary car: only on a genuine 6 A of avail_dump. The dump does not apply the reservation trim.
- `grid_bridge_primary_holds_the_floor` — 6A-brug (grid, cheap) as the primary car: the floor is bridged on a passing cloud.
- `bridges_need_the_sun` — The bridge still needs the sun: at night 6A-brug is 0.
- `a_secondary_car_needs_a_genuine_six_amp_surplus` — A secondary car needs a genuine 6 A surplus, even on a bridge level.
- `the_battery_bridge_is_not_price_gated` — The battery 6A-brug is not price-gated.
- `the_target_is_rounded_to_a_tenth` — The final `|round(1)` ties to even: hard_max 10 + 27 − 24.75 = 12.25 → 12.2.
- `cable_follows_the_sun_on_its_measured_current` — The cable on plain solar: act is the measured current. EV3 at 8 A, besch 460 → 10.0.
- `buzz_pause_ok_protects_a_live_session_for_300_s` — BUZZ_PAUSE_OK: not already Pause; and (not charging, or not Normal, or ≥ 300 s).
- `observe_risk_tracks_last_changed` — `observe_risk` follows `last_changed`: set at start, on each flip, and if missing.

### `automations/goedkoop_laden.rs` (7)

- `decides_at_start` — `trigger: homeassistant start`: decides on the first tick.
- `rechecks_every_five_minutes` — `time_pattern minutes: /5`: a change of an unlisted input waits for the next five.
- `listed_state_changes_trigger` — Each listed entity re-triggers on a state change.
- `needs_cheap_max_tot_piek` — `grid_max`: `Max tot piek` and strictly below the threshold; the fallback price.
- `needs_room_and_no_dump` — `batt_wants = soc|float(0) < 99` and `not batt_dump`.
- `yields_the_grid_turn_to_a_wanting_car` — `batt_first or not car_wants`: a wanting car ranked above the battery takes the grid turn, unless it is stalled (`ev_hoger_laadstil`); a battery ranked first always charges. `buzz_wants` is `plug_known == 'Connected' or stateid in
- `refreshes_the_plug_latch` — Plug latch: a known plug state is copied into `input_text.smartevse_plug_laatst_bekend`; `unavailable` keeps the latch, and the latch is read when the plug blips.

### `automations/mod.rs` (1)

- `the_registry_holds_the_29_enabled_live_automations_by_their_ha_ids` — Every enabled live HA automation (the original configuration) is ported exactly once, under its HA `id` (the key of `last_triggered`, which other automations read, e.g. `buzz_mode_stempel`). `vriezer_bewaking` is disabled in HA

### `automations/nachtdekking.rs` (10)

- `the_night_starts_at_22_with_the_meter_readings` — `nachtdekking_start` at 22:00: the three zero points; the EV sum is rounded to 3 decimals (600.4119 + 1030.5872 = 1630.9991…, → 1630.999).
- `unavailable_meters_at_22_store_zero` — With the meters unavailable at 22:00 the zero points are 0, which disables the end.
- `nachtdekking_excludes_ev_grid_energy` — `nachtdekking_eind` imp/imp_ev: grid energy the cars drew overnight is not house demand. Gross import 16.56 kWh, Buzz drew 12.5 kWh of it: imp = 4.06, batt = 3.2, share 3.2 / 7.26 = 44.07 % → 44. Fallback at 13:00 after a 15.0 h n
- `the_variables_floor_at_zero_and_ignore_a_missing_ev_zero_point` — The variables on their own: gross 16.56, cars 12.5, imp 4.06; the cars can never push the house import below 0; without an EV zero point the cars count as 0.
- `the_sun_taking_over_for_15_min_ends_the_night` — `zon` trigger: battery charging > 100 W with a gross surplus for 15 min. A tiny night (batt + imp ≤ 0.1 kWh) reads 0 %, and without car energy the note is left out.
- `the_end_needs_a_running_measurement_and_daylight_hours` — Conditions: no measurement running (zero point 0), or before 05:00 (`hour >= 5`).
- `the_duration_is_real_elapsed_time_from_22_00` — `nachtdekking_eind` `duur` = `(as_timestamp(now()) - as_timestamp(s)) / 3600`: real elapsed hours from 22:00 local. The night into 2026-10-25 (clocks back) reads 22:00 → 13:00 as 16.0 h, the night into 2026-03-29 (clocks forward) 
- `the_sun_trigger_needs_strictly_above_100_w_and_a_positive_surplus` — `nachtdekking_eind` `zon` trigger: `batterij_laadvermogen > 100 and pv_bruto_overschot > 0`, both strict and both needed. Exactly 100 W of charge, or a gross surplus of exactly 0, held for 15 min, ends nothing.
- `the_share_needs_more_than_0_1_kwh` — `nachtdekking` share: `batt / (batt + imp) * 100 if (batt + imp) > 0.1 else 0`. A night of exactly 0.1 kWh (0.05 + 0.05) is still "tiny" and reads 0 %; 1 kWh from the battery and nothing from the grid reads 100 %.
- `during_the_22_hour_the_night_is_today_s` — `duur`: `s if now().hour >= 22 else s - timedelta(days=1)`. During the 22:00 hour the night counts from today's 22:00: 22:30 is 0.5 h.

### `automations/neg_injectie.rs` (5)

- `curtails_three_minutes_into_a_negative_price` — Trigger `price below 0 for 3 min` with the battery already full (full since start, so its own trigger never fires).
- `curtails_two_minutes_after_the_battery_fills` — Trigger `soc above 96 for 2 min` with the price already negative (its own trigger was blocked by the conditions while the battery could still absorb).
- `curtail_conditions` — Each condition: exporting more than 100 W, export limit above 100 W, values available.
- `restores_the_export_limit_three_minutes_after_the_price_recovers` — `price above -0.0001 for 3 min` and `backflow below 100`: back to 5500 W. A price of exactly 0 counts as recovered.
- `herstel_conditions` — Not when the limit is already up; not on `-0.0001` itself; not when unavailable.

### `automations/peakguard.rs` (14)

- `the_peak_guard_sheds_the_buzz_first_after_10_s_of_risk` — `peakguard_afschakelen`: after 10 s of risk the counter goes up, the start note prints the projection in kW (3142 W / 1000 = 3.142 → round(2) 3.14), and the first shed step runs at once: Buzz 14.5 A − 3 = 11.5 A → payload 115.
- `a_disabled_peak_guard_does_nothing` — The `peak_guard_actief` switch gates the whole run.
- `the_peak_guard_walks_down_the_ladder_one_step_per_iteration` — LOGIC.md "Peak guard — Shed": flexible → comfort, re-evaluated each iteration. As each step takes effect the next iteration (20 s later) moves down the ladder: Buzz −3 A (7.0 → 6 A floor, payload 60), Buzz Pause, EV3 −3 A (10 → 7)
- `the_peak_guard_loop_stops_after_15_iterations_or_when_the_risk_clears` — `peakguard_afschakelen` repeat `while risk on and repeat.index <= 15`: with nothing sheddable the loop runs its 15 iterations (14 × 20 s after the first, plus the trailing delay) and ends; while it runs a new trigger is dropped (`
- `the_buzz_is_not_paused_within_300_s_of_a_mode_change` — LOGIC.md "Charge-session protection" via `peakguard_afschakelen` branch 2: a Buzz at its 6 A floor in Normal mode is not paused within 300 s of the last real mode change (`automation.buzz_modewissel_stempel` last_triggered), so th
- `a_trigger_while_the_loop_runs_is_dropped` — `mode: single` + `max_exceeded: silent`: a new trigger while the loop runs is dropped. The risk blips off and back on for 10 s inside one 20 s delay: no second start note.
- `session_protection_boundaries` — Branch 2 boundaries, checked directly on the `choose` step.
- `the_discharge_boost_needs_soc_above_25_and_no_boost_running` — Boost branch: not when the inverter already force-discharges, not at 25 % SoC or below (`> 25`), and an unavailable SoC reads 0.
- `a_cable_at_exactly_0_3_kw_is_left_alone` — `peakguard_afschakelen` EV3 branches: `ev_charging_cable_vermogen > 0.3`, strict. A cable at exactly 0.3 kW is neither stepped down (13 A) nor stopped (6 A): the iteration falls through to the discharge boost.
- `an_unavailable_cable_current_stops_the_cable` — `peakguard_afschakelen` EV3 −3 A branch requires `has_value(number.ev_charging_cable_ charge_current)`: with the number unavailable (and the cable drawing 1.4 kW > 0.3) the iteration goes to the stop branch instead of a skipped `n
- `the_start_note_prints_the_projection_in_kw_with_2_decimals` — The start note rounds the kW to 2 decimals and prints it as Python would (checked in Python): 0 W → `0.0`, 2500 W → `2.5`, 1995 W → 1.995 → `2.0`, 12 345 W → 12.345 (stored just above the tie) → `12.35`.
- `the_boost_is_cancelled_after_5_min_below_2_kw` — `peakguard_herstel`: the projection must sit below 2000 W (2.0 kW) for 5 min with the boost running; then the boost is cancelled.
- `a_projection_of_1500_w_cancels_the_boost_and_2500_w_does_not` — `peakguard_herstel` `below: 2000`: a projection of 2500 W never cancels the boost, one of 1500 W does after its 5 min hold (the old `below: 2.0` read as 2 W).
- `herstel_needs_the_guard_on_and_a_boost_running` — `peakguard_herstel` conditions: the guard switch and a boost actually running.

### `automations/piekhistoriek.rs` (5)

- `piekhistoriek_keeps_12_months_sorted_and_replaces_the_current_month` — `capaciteit_piekhistoriek_bijwerken`: adding November drops the oldest month (2025-11) so 12 remain, sorted; writing November again replaces it instead of adding a 13th.
- `the_month_end_writes_the_current_peak` — `maandeinde` at 23:59:50 on the last day of the month writes the meter's current peak (3892 W → 3.892); on other days nothing happens.
- `the_meter_reset_on_the_1st_records_the_final_peak` — `reset`: the P1 month peak drops to 0 at 00:00:05 on the 1st; the from-state is the final peak of the closing month (a peak in the last 10 s is caught here), stored under that month (now − 1 h = 31 October).
- `only_a_real_reset_counts` — `reset` conditions: only at 00:xx on the 1st, only a decrease from a positive value to a valid one. A rise, a drop to unavailable, a drop at 01:00, or from unavailable: no.
- `an_unavailable_peak_at_month_end_writes_nothing` — The action-level `kw > 0` condition: an unavailable peak at month end runs the automation (last_triggered moves) but writes nothing.

### `automations/smartevse_pilot_arm.rs` (5)

- `arms_the_pilot_after_45_s_on_a` — 
- `the_two_minute_pattern_retries_after_300_s` — The /2 min pattern retries, but not within 300 s of the last run.
- `the_pattern_catches_a_state_present_at_start` — Already stuck at start: the pattern (not the template trigger) picks it up.
- `conditions` — No room (≤ 1500 W), or the pilot not on A: nothing. A plug blip reads the latch.
- `mode_labels` — 

### `automations/smartevse_plug_latch.rs` (3)

- `latches_valid_states` — The latch follows a valid plug state at start and on a change.
- `a_blip_keeps_the_latch` — An unavailable blip never overwrites the latch.
- `the_minute_pattern_resyncs` — The /1 min pattern catches a latch that was out of date with no plug change (e.g. the latch edited by hand); an unchanged, equal state does not run.

### `automations/solis_watchdog.rs` (3)

- `reconnects_after_ten_minutes_offline` — `for: 00:10:00`, then the stamp, a high-priority push, the dashboard note and the reconnect, in HA's order.
- `at_most_one_reconnect_per_hour` — `(now − laatste_herstart) > 3600`: a second outage within the hour does nothing (and the run is not marked); after more than an hour it reconnects again.
- `fires_once_per_outage` — The trigger fires once per outage; it re-arms when the Solis comes back.

### `automations/vriezer.rs` (5)

- `a_freezer_idle_for_4_h_raises_the_alarm` — `vriezer_bewaking` `geen_verbruik`: below 5 W for 4 h, high priority, with the power state and the time in the texts.
- `exactly_5_w_is_not_idle` — `geen_verbruik` is `numeric_state below: 5`, strict: a freezer idling at exactly 5.0 W for 4 h raises nothing.
- `switching_the_plug_off_raises_the_alarm_at_once` — `stekker_uit`: the plug switched off, at once; staying off does not repeat it.
- `a_plug_offline_for_15_min_raises_the_alarm` — `plug_offline`: power unavailable for 15 min; the message prints `unavailable`.
- `absent_entities_never_trigger` — With the entities absent nothing ever transitions and the automation stays silent, even for days of "unavailable".

### `battery/tests.rs` (74)

- `export_is_charged` — Base law `d = batt + p1 + bias` (template "Batterij setpoint"): export is charged. test_decide.py::test_export_charges.
- `import_is_discharged` — Base law: import is discharged. test_decide.py::test_import_discharges.
- `base_law_adds_the_current_battery_power` — Base law adds the battery's own power: charging 2000 with 500 still exported → 2500. test_decide.py::test_tracking_uses_current_battery.
- `base_law_adds_the_bias` — Base law adds the integral: `0 + 0 + 50 = 50`. test_bias.py::test_bias_added_to_setpoint.
- `base_law_uses_the_median_p1` — The base law steers on the 90 s median (`p1`), not on the instantaneous `p1_fast`.
- `never_discharges_while_exporting` — Exporting never discharges. test_decide.py::test_never_discharge_while_exporting.
- `floor_blocks_discharge_at_and_below_vloer` — `{% if soc <= vloer %}{% set d = [0, d]|min %}`: no discharge at or below the floor. test_decide.py::test_reserve_floor_blocks_discharge / test_below_reserve_no_discharge / test_editable_reserve_*.
- `morning_floor_is_inclusive` — The morning floor of 15 %: at 15 % hold, at 16 % discharge. test_decide.py::test_morning_floor_still_15 (the window itself: `vloer_is_15_in_the_morning`).
- `ceiling_blocks_charge_at_100` — `{% if soc >= 100 %}{% set d = [0, d]|max %}`: no charge when full; 96 and 99 still charge. test_decide.py::test_top_blocks_charge_at_100 / test_charges_up_to_100.
- `yield_with_battery_bridge_feeds_the_bridge_but_not_from_surplus` — Yield with `battery_bridges` (`6A-brug`): `[0, d]|max`, the battery covers the car's bridge import but does not grab the surplus, and the floor still wins. test_decide.py::test_batterij_feeds_bridge / test_batterij_yields_surplus_
- `yield_without_bridge_or_reservation_holds_at_zero` — Yield without bridge or reservation: `{% set d = 0 %}`, hold whatever P1 says. test_decide.py::test_net_does_not_feed_car / test_net_holds_while_ev_ramping / test_reservation_off_by_default_still_holds; test_scenarios.py steps 1, 
- `no_hungry_car_is_plain_self_consumption` — No hungry car: plain self-consumption mops up the leftover export. test_decide.py::test_net_mops_up_when_ev_maxed; test_scenarios.py step 3 and test_battery_first_charges_before_cars, step 4 (full) and test_evening_battery_covers_
- `yield_with_reservation_charges_leftover_up_to_the_reservation` — Yield with the surplus reservation on: `[-reserve_surplus_w, [0, d]|min]|max`, charge the leftover export up to the reservation, never discharge, never above 100 %. LOGIC.md "Battery reservation"; test_decide.py::test_reservation_
- `battery_bridge_outranks_the_reservation` — The bridge outranks the reservation (`if battery_bridges ... elif reserve_surplus`). test_decide.py::test_reservation_yields_to_battery_bridge.
- `net_laden_skips_the_yield` — `ev_higher_hungry and not net_laden`: grid-charging owns the battery, the yield is skipped. Phase cap `0 + (27 − 20)·230 = 1610`, law `0 − 1500 − 5829 = −7329` → −1610. test_decide.py::test_reservation_not_applied_while_net_laden 
- `grid_laden_holds_instead_of_feeding_the_car` — `ev_grid_laden`: `[0, d]|min`, never discharge into a car that imports on purpose, but still charge real surplus. test_decide.py::test_grid_charging_car_holds_battery / _still_charges_real_surplus / _flag_off_behaves_normally / te
- `grid_laden_reservation_is_bounded_by_the_import_headroom` — `ev_grid_laden` reservation: `resv = −max(0, min(reserve_grid_w, allowed − p1_fast + max(0, −batt)))`, `d = min(d, resv)`. LOGIC.md "Battery reservation" (peak half). Headrooms: 5829 − 2000 = 3829 (full 300); 5829 − 5829 = 0; 5829
- `grid_laden_reservation_adds_back_the_existing_charge` — The battery's current charge is inside `p1_fast`, so the headroom adds it back: `5829 − 5829 + 500 = 500` → holds its 500 W; `5829 − 2000 + 300` → keeps its 300 W. test_decide.py::test_reservation_under_peak_total_import_reaches_t
- `grid_laden_reservation_stops_at_100` — `reserve_grid_w > 0 and soc < 100`: no reservation when full. test_decide.py::test_reservation_under_peak_respects_ceiling.
- `net_laden_skips_the_grid_laden_hold` — `ev_grid_laden and not net_laden`: net_laden's law runs instead (−1610 as in `net_laden_skips_the_yield`, with `2000 − 5829` instead). Python asserted only `!= −300`. test_decide.py::test_reservation_under_peak_yields_to_net_laden
- `grid_laden_reservation_does_not_cap_solar_absorption` — The reservation is a floor on charging, not a cap on free solar: base law −1269 − 864 + 200 = −1933 beats the 1300 reservation. Importing: headroom 3969 − 2000 = 1969 → −1300; at p1 3500 headroom 469 → −469; full → 0. test_scenari
- `grid_laden_takes_the_effective_reservation` — The effective reservation (`sensor.batterij_reservering_effectief`) takes the leftover headroom: 3525 − 2060 = 1465 ≥ 1056 → −1056; 0 → nothing; 5000 bounded to 3525 − 3400 = 125. test_scenarios.py::test_reserve_grid_takes_leftove
- `deliberate_import_hold_needs_a_drawing_car` — Behind a drawing car the battery holds even at 2709 W import; without one it covers it; the floor wins at 13 %; the clamp caps 9000 at 5000. test_scenarios.py::test_deliberate_import_hold_needs_a_drawing_car.
- `parked_cars_shave_only_the_overshoot` — `ev_grid_parked`: `d = [d, [0, p1_fast − allowed]|max]|min`, discharge only the overshoot. 2710 < 2801 → 0; 2763 − 2735 = 28; 9000 − 2735 = 6265 → clamp 5000; floor at 13 %. test_scenarios.py::test_parked_cars_shave_overshoot_not_
- `parked_cars_still_absorb_export` — `min`, not an assignment: export is still absorbed (−5374 → clamp −5000; −800). test_scenarios.py::test_parked_cars_still_absorb_export (2026-08-30 regression).
- `parked_shave_reads_the_instantaneous_p1` — The shave bound uses `p1_fast` while the base law uses the median: raw 3000 over an allowance of 2800 bounds a discharge at 200 W. Median 0 → `min(0, 200) = 0`; median 1000 → `min(1000, 200) = 200`.
- `parked_cars_take_the_grid_reservation` — The parked branch takes the reservation too (live, not in the Python mirror): headroom 5829 − 2000 = 3829 → −300 instead of 0.
- `a_full_battery_skips_the_reservation_and_keeps_the_shave` — `reserve_grid_w > 0 and soc < 100` (template "Batterij setpoint", parked branch): at exactly 100 % the reservation is skipped, so a parked car's overshoot shave survives the ceiling. P1 6329 W, allowed 5829: shave 500 W. With the 
- `net_laden_at_100_leaves_the_cascade_branches_on` — Template "Batterij setpoint": `net_laden = is_state(batterij_net_laden, 'on') and soc < 100`. At 100 % with the flag still on (until `goedkoop_laden` clears it) `net_laden` is false, so the cascade branches apply and then the ceil
- `parked_shave_yields_to_the_dump` — `ev_grid_parked and ... not ev_batterij_auto_laden`, pinned with hand-built flags. LIVE: this clause is dead: [`Flags::read`] cannot set both (parked needs no car drawing, the dump needs one).
- `dump_forces_a_steady_3000_w` — `{% if ev_batterij_auto_laden and not net_laden %}{% set d = 3000 %}`: a steady 3000 W, independent of P1, blocked by the floor. test_decide.py::test_batterij_auto_forces_full_discharge / _is_p1_independent / _respects_reserve_flo
- `dump_survives_the_grid_laden_hold` — `ev_grid_laden and ... not ev_batterij_auto_laden`: the dump survives the grid-charge hold. test_decide.py::test_batterij_auto_overrides_grid_laden.
- `net_laden_wins_over_the_dump` — net_laden wins over the dump: cap 2000 + 1610 = 3610, law −2000 + 0 − 5829 → −3610. test_decide.py::test_batterij_auto_net_laden_takes_precedence (Python asserted `< 0`).
- `net_laden_is_capped_by_the_worst_phase` — `phase_cap = [0, [5000, max(0, −batt) + (27 − maxphase)·230]|min]|max`, `d = [−phase_cap, batt + p1_fast − allowed]|max`. 2000 + 7·230 = 3610; 2000 − 3·230 = 1310; 3000 + 17·230 = 6910 → 5000. test_decide.py::test_net_laden_dynami
- `net_laden_phase_cap_never_goes_negative` — A phase already past 27 A with an idle battery: `0 + (27 − 31)·230 = −920` → cap 0, hold.
- `net_laden_steers_import_onto_allowed` — The law steers import onto `allowed`: −500 + 6000 − 5829 = −329 (cap 500 + 15·230 = 3950). test_decide.py::test_net_laden_never_charges_past_economic_target / test_budget_absent_falls_back_to_static_law (the fallback itself: `allo
- `net_laden_follows_the_quarter_budget` — A budget credit charges harder one-for-one, a debt backs off (and here even discharges: −500 + 6000 − 4829 = 671), an exhausted budget asks for no import. LOGIC.md "Quarter-hour budget control"; test_decide.py::test_budget_credit_
- `net_laden_throttles_on_the_instantaneous_p1` — net_laden throttles on the instantaneous P1: median 0 → −5000 (cap 2000 + 17·230 → 5000); raw 3000 → −2000 + 3000 − 5829 = −4829. test_decide.py::test_net_laden_fast_p1_throttles_on_import_spike.
- `net_laden_stops_at_100` — `net_laden and soc < 100`: at 100 % the law is off and the ceiling holds. test_decide.py::test_net_laden_stops_at_100; test_scenarios.py::test_netladen_stops_when_full.
- `net_laden_overrides_the_yield` — net_laden replaces the yield. test_decide.py::test_net_laden_overrides_yield.
- `trickle_charges_a_drained_pack` — `soc <= 10`: at least 220 W of charge, whatever else wants (importing, a car on the grid), without throttling a harder charge. test_scenarios.py::test_trickle_charges_a_deeply_drained_pack_whatever_else_wants.
- `trickle_starts_at_10_not_11` — The ON edge: 9 and 10 start it, 11 with an idle battery does not. test_scenarios.py::test_trickle_starts_at_the_on_threshold_not_before.
- `trickle_latches_until_12` — `_trickling and soc < 12`: a battery already charging carries on through 11 and stops at 12. Base law −220 + 2763 = 2543 → −220 at 11, floor → 0 at 12 and 13. test_scenarios.py::test_trickle_keeps_going_through_the_band_once_latch
- `trickle_latch_reads_the_battery_power` — `_trickling = batt <= -110.0`: −2500 holds the latch harmlessly (−5000 stays), −150 and exactly −110 count, −109 and −50 do not. test_scenarios.py::test_trickle_latch_reads_the_plant_not_a_helper.
- `invalid_inputs_hold_at_zero` — `0 if not valid`: a stale critical sensor holds. test_decide.py::test_invalid_inputs_force_hold / test_valid_inputs_behave_normally.
- `output_is_clamped_to_5000_both_ways` — `[5000, [-5000, d]|max]|min`. test_decide.py::test_output_always_within_power_limits.
- `output_is_rounded_half_to_even_without_negative_zero` — `| round(0)`: ties to even (HA's `round` is Python's), and never a negative zero (HA turns a precision-0 round into an int).
- `sweep_respects_limits_floor_and_ceiling` — Every flag combination, SoC and P1: inside the inverter limits, never discharging at or below the floor (40), never charging at 100. test_decide.py::test_full_sweep_invariants (sweeps flags instead of the retired `ev_laadmodus`).
- `bias_integrates_a_tenth_of_p1` — `bias + 0.1 * p1`: 0 + 10 = 10, 10 + 10 = 20, export lowers it. test_bias.py::test_accumulates_slowly_toward_killing_import / test_sign_export_lowers_bias.
- `bias_has_a_20_w_deadband` — `(p1|abs) < 20` freezes it; 20 itself integrates; 30 nudges 25 → 28. test_bias.py::test_deadband_freezes_small_residual / test_corrects_residual_above_deadband.
- `bias_is_clamped_to_200` — `[200, [-200, ...]|max]|min`. test_bias.py::test_clamped_to_limit.
- `bias_is_rounded_to_a_tenth` — `| round(1)` on both paths: 0.1 · 33.33 = 3.333 → 3.3; a frozen 12.34 → 12.3.
- `bias_is_frozen_outside_base_tracking` — `track` is false outside plain self-consumption, so the bias is held: invalid, at the floor, full, yielding (with or without bridge), grid-charging, a car grid-charging, dumping. test_bias.py::test_frozen_outside_base_tracking (+ 
- `bias_is_frozen_while_cars_are_parked` — `soluna_dispatcher.new_bias` `track … and not ev_grid_parked`: with cars parked under the peak the setpoint only shaves and P1 stays non-zero on purpose, so the bias holds at 25 (it would otherwise integrate 25 + 0.1 × 900 = 115).
- `p1_is_the_median_falling_back_to_the_raw_reading` — `p1 = mediaan|float(P1|float(0))`, `p1_fast = P1|float(0)`.
- `valid_needs_p1_soc_and_battery_power` — `valid = has_value(P1) and has_value(soc) and has_value(batt)`; missing values read as 0.
- `vloer_is_15_in_the_morning` — `vloer = 15 if today_at('06:00') <= now() < today_at('10:00') else reserve`. test_decide.py::test_morning_releases_reserve.
- `allowed_falls_back_to_target_minus_100` — `allowed = kwartier_toegestaan if has_value(...) else maandtarget·1000 − 100`: the budget sensor already carries the margin. LOGIC.md "Quarter-hour budget control"; test_decide.py::test_peak_margin_has_one_definition (behavioural 
- `read_carries_memory_settings_and_derived` — Memory, settings and derived inputs land in the right fields.
- `dump_level_without_a_drawing_car_is_self_consumption` — Ontlaad-budget without a drawing car is plain self-consumption: 2000 → 2000, 6000 → 5000, the floor holds. test_decide.py::test_batt_auto_discharges_to_cover_car / _discharge_clamped_to_inverter_max / test_batt_auto_respects_reser
- `every_level_without_a_car_is_self_consumption` — With no car, every grid × battery level is plain self-consumption. test_decide.py::test_no_hungry_ev_is_plain_self_consumption (levels instead of `ev_laadmodus`).
- `hungry_needs_gross_surplus_above_the_yield_threshold` — `_bruto > _yt` with `_yt = 1400` when both levels are `Uit`, else 200 (strictly above). LOGIC.md "The surplus cascade", bootstrap.
- `hungry_only_in_solar_mode` — `_solar_mode = not grid_max and not batt_dump`: no yield under a cheap `Max tot piek` or in `Ontlaad-budget`.
- `a_stalled_higher_car_does_not_make_the_battery_yield` — `and not _stalled`: `ev_hoger_laadstil` releases the battery. LOGIC.md "Cascade-yield liveness".
- `buzz_eligibility` — `_buzz_elig`: ranked above the battery, wants (plug or pilot B/C, latch fallback), reachable, and not within 1.5 A of its max while charging. LOGIC.md "Unavailable components are transparent".
- `ev3_eligibility` — `_ev3_elig`: ranked above the battery, wants (cable status or car plug), reachable, and not drawing (> 0.2 kW) at 15 A or more.
- `grid_flags_split_on_a_drawing_car` — `ev_grid_laden` / `ev_grid_parked`: cheap `Max tot piek`, a car wants; drawing or not. Neither when dear, or when the drawing car does not "want" (cable says `available`).
- `dump_flag_needs_a_drawing_car` — `ev_batterij_auto_laden = batt_dump and (_ev3_drawing or _buzz_drawing)`.
- `off_target_when_more_than_800_w_away_in_the_followable_band` — Template "Dispatcher off-target": `|batt − sp| > 800`, dispatcher on, valid, and `vloer + 2 < soc < 95`.
- `off_target_needs_battery_readings` — `valid` needs the battery power and the SoC (the SoC default 50 never matters then).
- `fill_writes_setpoint_then_off_target` — `fill` writes the setpoint and evaluates off-target against that same setpoint: P1 1500 W with an idle battery → setpoint 1500, |0 − 1500| > 800 → off-target.
- `num_states_print_like_ha` — Integer-typed states print without a decimal; fractions as Python floats.
- `float_states_print_like_python` — Float-typed states print as Python's `repr(float)`.
- `rc_mode_prints_the_select_option` — 

### `cascade.rs` (9)

- `missing_text_states` — 
- `buzz_plug_falls_back_to_latch` — 
- `buzz_wants_on_pilot_b_or_c` — 
- `buzz_flags` — 
- `ev3_wants_from_cable_or_car` — 
- `ev3_flags` — 
- `price_gates` — 
- `phase_views` — 
- `battery_views` — 

### `command.rs` (1)

- `constructors` — 

### `derived.rs` (36)

- `live_snapshot_2026_11_07_reproduces_every_template_state` — Every template sensor of the live snapshot that does not depend on sub-second timing reproduces the state HA recorded at that instant, except the two templates changed by the 2026-11-07 fixes (`Zelfvoorziening vandaag`, `Batterij 
- `projectie_is_stored_in_watts_and_drives_the_risk_flag` — `Kwartiervermogen projectie`: 0.2 kWh used, 2400 W for the 300 s left of the quarter (13:55:00): (0.2 + 2400·300/3 600 000)·4 = (0.2 + 0.2)·4 = 1.6 kW, stored as 1600 W. `Capaciteitspiek risico` compares that W value with the targ
- `the_risk_window_and_threshold_are_strict` — `Capaciteitspiek risico`: `rem > 20 and (900 - rem) > 60 and projectie > target·1000 − 25`, all strict. With 20 s left (13:59:40) or exactly 60 s gone (13:46:00) there is no risk; one second further inside there is. A projection o
- `projectie_ignores_export` — A negative P1 (export) counts as 0 in the projection: `[p1, 0] | max`.
- `maandtarget_takes_the_highest_of_floor_peak_and_setting` — `Capaciteit maandtarget` = max(2.5, month peak in kW, setting), rounded to 3.
- `toegestaan_spends_the_remaining_budget_with_a_65_percent_credit` — `Kwartier toegestaan vermogen`, target 3.9 kW → aim 3800 W, budget 0.95 kWh. At 13:50 (600 s left, 0.2 kWh used): raw = 0.75/(600/3600)·1000 = 4500, credit 700 → 3800 + 0.65·700 = 4255. With 0.5 kWh used: raw = 0.45·6000 = 2700, c
- `without_the_meter_the_allowance_is_unknown_and_the_average_uses_the_clock` — With the quarter meter unavailable its `next_reset` attribute is gone and `Kwartier gemiddelde` falls back to the wall clock: `900 - (minute % 15)·60 - second` (whole seconds), the meter value reading 0 (`float(0)`). `Kwartier toe
- `the_reservation_falls_back_to_the_month_target_without_an_allowance` — `Batterij reservering effectief`: `allowed = toegestaan if has_value(toegestaan) else tgt`. Without the quarter meter (allowance unknown) the target 3900 W is used: P1 1000, worst phase 10 A → auto = 3900 − 1000 = 2900 (< phase bo
- `a_full_battery_reserves_nothing` — `Batterij reservering effectief` is 0 at SoC ≥ 100: a full battery reserves nothing, whatever the manual setting (5000 W here; 3910 W at 99.9 %).
- `kwartier_gemiddelde_uses_live_power_in_the_first_30_seconds` — `Kwartier gemiddelde`: in the first 30 s (`elapsed < 30`) of a quarter it shows the live import (`[0, p1] | max`); after that used energy over elapsed time: 0.1 kWh after 120 s → 0.1/(120/3600)·1000 = 3000 W.
- `a_due_quarter_reset_reads_as_zero_before_the_meter_updates` — The quarter meter rolled over at 14:00 but has not seen a reading yet: templates already read 0 kWh and a full quarter, like HA, whose reset runs on a timer.
- `omnik_counts_only_in_daylight_and_house_load_is_the_balance` — `PV vermogen totaal` adds the Omnik only while the sun is up; `Huisverbruik vermogen` = max(0, pv + batt + p1); battery views split the signed power.
- `battery_energy_above_the_10_percent_floor_and_offline_flag` — `Batterij beschikbare energie` = max(soc − 10, 0)/100·10 kWh; `Solis offline` when the SoC is unavailable (and then the energy reads 0).
- `smartevse_power_and_its_availability` — `EV laadvermogen smartevse` has an availability template: unavailable without a SmartEVSE state, or while Charging without a current. Not charging → 0 kW. Charging at 16 A → 16·230/1000 = 3.68 kW, 3680 W.
- `cable_charging_splits_solar_and_grid_to_the_ev3` — The EV3 cable at 10 A / 232 V = 2.32 kW, 2320 W, importing 820 W with the battery idle: `PV bruto overschot` = max(0, 2320 + 0 − 0 − 820) = 1500 W. Zon = min(2.32, 1.5) = 1.5, net = 0.82. The cable car is the active one (10 A > 0.
- `actieve_laadauto_precedence` — `EV actieve laadauto`: cable current > 0.5 A → ev3; else SmartEVSE Charging → buzz; else EV3 plugged → ev3; else buzz. A cable current on the Buzz's turn (both drawing is impossible in practice) goes to the Buzz totals only when t
- `bruto_overschot_needs_p1_and_battery` — `PV bruto overschot` is 0 unless both P1 and the battery power have a value.
- `price_fallbacks` — `Stroomprijs nu`: the backup price when ≥ 0, else the market price, else unavailable. `Stroomprijs niveau` on the range percentage (default 50): < 30 goedkoop, > 70 duur. `Kwartierprijs volgend` falls back to the current price aft
- `per_100km_and_solar_share_guard_against_zero` — The per-100-km figures are 0 until the odometer passes the start reading, and the solar shares 0 until anything was charged.
- `battery_to_car_counts_only_in_discharge_budget_mode` — `Batterij naar auto geleverd kWh`: only in `Ontlaad-budget`, discharged since the start reading, never negative: 752.0 − 749.5 = 2.5; start above the counter → 0.
- `self_consumption_needs_some_pv` — `Zelfverbruik vandaag` is 0 below 0.1 kWh of PV; otherwise (pv − inj)/pv clamped to 0..100: 4 kWh PV, 1 kWh injected → 75 %, 3 kWh kept.
- `reservation_leaves_room_for_an_eligible_ev3` — `Batterij reservering effectief` with the EV3 eligible and drawing, the Buzz idle: allowed (meter at 0 kWh, target 3.9 kW, 13:50) = 5035 W; p1 1000, cable 8 A·230 = 1840 W, battery charging 500 W → load_nonsteer = 1000 − 1840 − 0 
- `gefactureerde_piek_rolls_twelve_months` — `Gefactureerde piek` averages the 11 previous months (each at least 2.5 kW) with this month: for March 2026, entries 2025-04 (age 11) and 2026-02 (age 1) count, 2025-03 (age 12) and 2026-03 (age 0) do not; "xx" parses as 2.5; entr
- `str_int_follows_jinja_int` — 
- `days_in_month_handles_february_and_december` — The month length from the `replace(day=28) + 4 days` trick.
- `per_100km_counts_from_the_odometer_start` — `Buzz laadkost per 100km`: km = odometer − `input_number.buzz_odometer_start`. 10 100 − 10 000 = 100 km, € 5 of grid charging → € 5.00 per 100 km.
- `meerkost_prices_the_target_above_the_peak` — `Capaciteitspiek meerkost` = `([0, tgt − peak]|max · 56.04 / 12 · 1.06)|round(2)`: a 5 kW target over a 2.5 kW peak costs 2.5 · 56.04 / 12 · 1.06 = 12.3755 → € 12.38.
- `self_consumption_counts_from_exactly_0_1_kwh` — `Zelfverbruik vandaag`: `if pv < 0.1` is strict, so exactly 0.1 kWh of PV with 0.05 injected already reads 50 %.
- `zelfvoorziening_is_own_solar_over_own_solar_plus_import` — `Zelfvoorziening vandaag`: `eigen = max(0, pv_dag − injectie_dag)`, `use = eigen + netimport_dag`, `eigen / use · 100`. PV 10, injection 4, import 2 kWh → 6 / 8 = 75 %; no import → 100 %; more injected than produced → eigen 0 → 0 
- `the_month_is_a_two_character_slice` — `k[5:7]` is a two-character slice: a key "2026-10x" still reads month 10, so it is one of the 11 previous months.
- `reservation_subtracts_only_an_eligible_car_s_headroom` — `Batterij reservering effectief` eligibility: `ev3_elig = wants and soc < ac_limit and ctrl`, `buzz_elig = wants and soc < laaddoel and ctrl`; an eligible car's headroom `[mx, cur + (27 − maxphase)]|min · 230` comes off `auto`. Wo
- `reservation_phase_bound_leaves_each_drawing_car_its_ramp` — `phase_bound = [0, batt_chg + (27 − maxphase − ev3_need_a − buzz_need_a) · 230]|max`, `need = [[mx − cur, 0]|max, 4]|min if drawing`. Both cars drawing at 14 of 16 A (need 2 each, neither eligible), battery charging 500 W, worst p
- `a_buzz_at_200_w_is_not_drawing_for_the_reservation` — `bz_drawing = ev_laadvermogen_smartevse_w > 200`, strict: a Buzz at 0.87 A (200 W) is not drawing and needs no ramp, so phase_bound = 17 · 230 = 3910; its 200 W still comes off the load: auto = 5035 − (1000 − 200) = 4235.
- `reservation_counts_the_discharge_as_house_load_and_caps_the_manual_one` — `load_nonsteer` adds the battery's discharge back: discharging 500 W with P1 1000 → load 1500, auto = 5035 − 1500 = 3535 (< phase bound 3910). A manual 5000 W is capped by `manual_bound = batt_chg + (27 − maxphase) · 230` = 3910.
- `toegestaan_keeps_its_render_until_an_input_changes` — `Kwartier toegestaan vermogen` reads `now()`, so HA re-renders it on a change of the quarter meter or the month target and at the top of each minute — not every second. Recorder 2026-11-06 16:29:30: HA still showed 5265 W (the 1.3
- `gemiddelde_stops_reading_p1_after_30_s` — `Kwartier gemiddelde` reads P1 only in its first 30 s; after that a P1 change does not re-render it (recorder 2026-11-03 02:15:33: 2936 W held while P1 went 504 → −927 W, until 02:16:00). Quarter 0.027 kWh at 02:15:33 (elapsed 33 

### `engine.rs` (3)

- `first_tick_is_start_then_prev_is_carried` — 
- `the_real_registry_runs` — 
- `p1_meters_update_before_the_templates` — HA's quarter meter updates on the P1 event itself, before the templates that read it re-render; the engine updates the P1-fed meters before computing the templates, so the tick that brings a new import reading already sees it (0.0

### `jinja.rs` (6)

- `round_is_ties_to_even_like_python` — 
- `round_uses_the_exact_binary_value_like_cpython` — Python `round()` works on the exact binary value: 2.675 is 2.67499999… so it gives 2.67, and 0.285 is 0.28499999… → 0.28; 1.000015 → 1.00002 (1.0000150000000000589…, above the half). These are CPython's results; a multiply-then-ro
- `round_floor_floors_the_product_like_ha` — HA `forgiving_round(x, n, 'floor')` is `math.floor(x * 10**n) / 10**n`; it keeps the representation error of the product (CPython: `math.floor(2.3 * 100) / 100 == 2.29`). The modulator's `avail` hits it: (2300 / 230 + 920 / 230) ×
- `int_truncates_toward_zero` — 
- `min_max_clamp` — 
- `py_float_prints_like_python_repr` — `{{ x }}` of a float prints Python's `repr`: the digits HA stores, `.0` on whole numbers, scientific notation outside [1e-4, 1e16). Reference values from CPython.

### `memory.rs` (1)

- `last_triggered_roundtrip_and_serde_default` — 

### `meters.rs` (21)

- `utility_meter_starts_at_zero_and_adds_deltas` — utility_meter: the first reading starts a fresh meter at 0, then every increase of the source is added: 100 → 100.5 → 101.25 gives 0, 0.5, 1.25.
- `utility_meter_without_net_consumption_ignores_decreases` — `net_consumption: false` drops a decrease (`adjustment >= 0` guard) and counts on from the lower value: 100 → 99 → 99.5 gives 0, 0, 0.5.
- `utility_meter_with_net_consumption_subtracts` — `Energiekost vandaag` (`net_consumption: true`) follows the cumulative cost down while exporting: 10 → 9.5 → 9.75 gives 0, −0.5, −0.25.
- `a_periodically_resetting_meter_rebaselines_after_an_unavailable_source` — `periodically_resetting: true` (the EV meters): after an unavailable source the first reading only re-baselines. 100, unavailable, 105, 106 → the 5 kWh across the gap are lost, the final 1 counts; `always_available: false`, so it 
- `a_non_resetting_meter_counts_the_gap_against_the_last_valid_reading` — `periodically_resetting: false` (`Kwartier import`, the daily energy meters since 2026-11-07): the first reading after the gap is diffed against `last_valid_state`. 100, 100.5, unavailable, 105, 106 → 0, 0.5, –, 5.0, 6.0: the 4.5 
- `a_decrease_across_the_gap_is_dropped_without_net_consumption` — `periodically_resetting: false`, a source lower after the gap: the adjustment 99 − 100 is negative and dropped (`net_consumption: false`), and 99 becomes `last_valid_state`, so 99.5 adds 0.5. With `net_consumption: true` the −1 wo
- `utility_meter_holds_on_an_unknown_source` — An `unknown` source (a helper without a value yet) leaves the meter available and unchanged. Periodically resetting it breaks the delta chain (1, unknown, 3, 4 → 0, 0, 0, 1); otherwise 3 is diffed against 1 (→ 2, then 3).
- `quarter_meter_resets_on_the_boundary` — `Kwartier import` resets at the quarter: the reading before 14:00 is 0.2, at 14:00:00.5 other templates already read 0, and the reading at 14:00:01 (+0.01 kWh) counts entirely in the new quarter. `last_period` keeps 0.2.
- `missed_boundaries_and_unknown_last_reset` — A meter that missed several boundaries (app down) resets once per boundary, like HA's timer catching up: the value ends at 0 and `last_period` holds the last (empty) cycle. A meter seeded without `last_reset` adopts the current cy
- `cycle_boundaries_are_local` — Cycle boundaries in local time, across DST and the year end.
- `trapezoidal_integration_of_watts_to_kwh` — Trapezoidal W → kWh: 1000 W then 2000 W 36 s later is 36·1500 = 54 000 Ws = 0.015 kWh. The first event after start integrates nothing (state stays unknown).
- `left_integration_uses_the_previous_value` — Left Riemann kW → kWh (EV integrals): 3.6 kW then 7.2 kW 10 s later uses the old value: 10·3.6/3600 = 0.01 kWh. No rounding on these.
- `max_sub_interval_integrates_the_held_value` — `max_sub_interval` 1 min: 1200 W held, next event 150 s later at 2400 W. HA integrates the held 1200 W at +60 and +120 (72 000 Ws each), then the event integrates the last 30 s trapezoidally: 30·(1200 + 2400)/2 = 54 000 Ws. Total 
- `unavailable_source_breaks_the_integral` — An unavailable source stops the integral: nothing for the gap, no timeouts while unavailable, and the first reading after it only re-baselines. 1000 W for 10 s, a 120 s gap, then 10 s at 1000 W: 2·10 000 Ws = 0.00556 kWh → 0.006 r
- `median_of_odd_and_even_counts_rounds_half_to_even` — statistics median, `precision: 0`: odd count takes the middle sample, even count the mean of the middle two, rounded half to even: (100 + 101)/2 = 100.5 → 100, (101 + 102)/2 = 101.5 → 102.
- `median_window_is_ten_seconds_and_keeps_the_last_sample` — `max_age` 10 s: a sample older than 10 s is dropped, at the next event or when read; with `keep_last_sample` the newest sample survives on its own forever.
- `median_buffer_holds_sixty_samples` — `sampling_size` 60: the 61st sample pushes out the oldest.
- `median_follows_source_availability` — An unavailable P1 makes the median unavailable; the next reading brings it back.
- `update_drives_every_meter_from_world_and_derived` — The whole set, one tick at a time: integrals feed their daily meters (by rounded state), the P1 registers feed the quarter/import/export meters, the car rates feed the car meters. Two ticks 36 s apart at a steady 1000 W PV and hou
- `integral_events_are_source_changes_not_ticks` — `Batterij geladen energie` (integration, trapezoidal, source `sensor.batterij_laadvermogen`, a template): HA only writes a template when its value changes, so the integral's events are the source's changes and a step is a trapezoi
- `median_samples_per_p1_reading_not_per_tick` — `P1 mediaan 90s` samples per P1 *event*: HA's HomeWizard entity reports every ~5 s (recorder 2026-11-03 02:15:23/28/33/38). Ticked every second, a reading held for 9 s is one sample, not nine: 100 W from 0 s, 200 W at 9 s, 300 W a

### `render.rs` (2)

- `renders_on_input_change_and_each_minute` — First call renders; same inputs within the minute keep the value; a changed input or the next minute renders again.
- `renders_round_trip_through_json` — The renders survive a JSON round trip (they are skipped in `Memory`, but the type keeps its serde shape for diagnostics), and missing fields default.

### `settings.rs` (4)

- `zonprioriteit_rank_and_label_roundtrip` — 
- `zonprioriteit_rejects_bad_strings` — 
- `niveau_labels_roundtrip` — 
- `defaults_are_the_live_values` — 

### `sun.rs` (11)

- `sunrise_and_sunset_are_astrals` — The event solver finds astral's sunrise and sunset (the anchors of HA's sampling) to within 2 s, including on both DST-change days and at the solstices.
- `flips_on_the_sampled_instants` — Up on the first 2-minute sample after civil dawn whose refracted elevation, rounded to 0.01°, is above −0.833°; down on the first sample after sunset that is not. Regression values for the default site.
- `flips_match_has_rules_on_astral_across_the_year` — HA's sampling rules give these flip instants on the 1st and 15th of every month of 2026. Regression values for the default site.
- `a_dropped_sample_delays_the_flip_to_sunrise` — A 2-minute update is not scheduled when it would land within 1.25 × 2 min of the next event, so on some mornings HA flips at the sunrise event itself.
- `day_and_night_need_no_sampling` — Outside twilight the state follows the sun: deep night is down, midday is up; between sunrise and sunset it is up without sampling, from civil dusk to civil dawn it is down.
- `refraction_is_astrals_piecewise_formula` — Refraction is astral's piecewise formula: 0 from 85°, the polynomial near the horizon, `−20.774/tan` below −0.575°, the `58.1/tan…` series above 5°.
- `noon_midnight_and_live_elevation` — Midday is up, midnight is down, and astral's elevation at a reference instant agrees.
- `solstice_noon_elevations` — At the summer solstice the sun peaks near 90 − 50.85 + 23.44 ≈ 62.6°, and at the winter solstice near 15.6°.
- `horizon_is_astrals_not_the_textbook_value` — Astral's horizon: 16′ radius + 1880.8″ refraction = 0.2667 + 0.5224 = 0.7891°. The reference is astral 3.2's `-(SUN_APPARENT_RADIUS + refraction_at_zenith(90 + 16′))`.
- `elevation_matches_astral_term_for_term` — The NOAA series matches astral's term for term: the elevation agrees to 1e-7° (well under a millisecond of sun motion) at every reference instant.
- `polar_day_and_night` — The pole gets polar day and night: the zenith clamp keeps acos defined.

### `time.rs` (7)

- `epoch_and_secs` — 
- `today_at_and_morning_window` — 
- `today_at_inside_a_dst_gap_falls_back_to_utc_reading` — 
- `quarter_boundaries` — 
- `time_pattern_crossings` — 
- `serde_roundtrip_keeps_brussels_time` — 
- `daily_crossings` — 

### `trigger.rs` (4)

- `already_true_at_start_does_not_fire` — 
- `fires_once_after_hold_and_rearms_after_false` — 
- `a_dip_resets_the_hold` — 
- `changed_compares` — 

### `world.rs` (1)

- `evse_mode_labels_are_the_ha_options` — The `select.smartevse_1234_mode` options, as `input_text.smartevse_modus_laatst_bekend` stores them.
