# Replay: does the port decide what HA decided?

`energie replay` runs the engine over a Home Assistant recorder export and compares, second by
second, every derived value and every actuation with what the live HA recorded. This page has
the method, the numbers for 2026-11-03 00:00 → 2026-11-07 14:03 (4.6 days, 386 331 steps, 1.53 M
rows, 14 s on the laptop) and the explanation of every remaining disagreement.

> **Since the 2026-11-07 fixes** the engine ports the original configuration, while this
> history was recorded under the config before them (`the earlier configuration`). The numbers below
> are from the engine as it was then. Replaying the same history with today's engine no longer
> matches where behaviour changed (`FINDINGS.md`, "Fixed in HA on 2026-11-07"): the peak-guard
> stop, the breaker guard with an idle Buzz, the bias freeze with parked cars, the reservation
> at SoC 100 and on the phase cap, the EV3 start retries, the unplug triggers, the mode stamp
> (its helper is not in the old history), the cable auto-heal retries, `Zelfvoorziening
> vandaag`, `Kwartier toegestaan vermogen` without the quarter meter, and the meters across P1
> gaps. That is expected and not chased; a replay of history recorded after the fixes is the
> next parity check.

## Running it

```
# 1. Export the recorder (on the Home Assistant host; columns entity_id, state, last_updated_ts,
#    shared_attrs — attributes only for the five entities whose attributes the port reads):
psql … -c "\copy (SELECT sm.entity_id, s.state, s.last_updated_ts, CASE WHEN sm.entity_id IN (…) THEN sa.shared_attrs END
           FROM states s JOIN states_meta sm USING (metadata_id) LEFT JOIN state_attributes sa USING (attributes_id)
           WHERE sm.entity_id IN (…) AND s.last_updated_ts >= … ORDER BY s.last_updated_ts) TO STDOUT WITH CSV" | gzip > energie-history.csv.gz
# 2. Prepend every entity's state at the window start (read-only GET on HA's history API):
deploy/replay-history.py --sql export.sql --start 2026-11-03T00:00:00+01:00 \
    --export energie-history.csv.gz --out history-full.csv.gz
# 3. Replay:
energie replay --history history-full.csv.gz --report replay.json --summary replay.md
#    [--from T] [--to T] [--step-ms 1000] [--no-lockstep] [--trace <field or actuator key>]
```

Step 2 matters: the export only holds rows written inside the window, so an entity that did not
change early on has no state for days (every `input_*` helper until HA's restart on 11-05).

## Method

- **Clock and table.** The CSV is streamed in time order into an entity-state table; a virtual
  clock steps every `--step-ms`. Each step applies the rows up to that instant, builds the
  `World` with `ha::mapping::world_from` and runs `Engine::tick`.
- **Lockstep** (default). Before each step Rust adopts HA's Settings and Memory helpers, as the
  table stood *before* the step's rows (those rows hold the last second's changes *and* HA's
  automations' reactions to them; adopting a latch HA wrote 50 ms after the plug changed would
  leave Rust nothing to decide). `--no-lockstep` seeds them once.
- **Meters.** `Memory.meters` is seeded from HA's meter entities (`ha::mapping::seed_meters`:
  value, `last_reset`, `last_valid_state`) at the start, after every gap, and again at the end of
  that settle window (by then HA has restored its meters after a restart, and the export's first
  midnight-reset rows are in). In lockstep a meter that differs from HA's by more than
  max(0.1, 0.2 %) for 120 s is re-adopted and counted (`ha::mapping::MeterAdopter`, also used by
  the live shadow). Final run: **0 adoptions**.
- **Gaps.** No row for more than 60 s is HA down. The clock jumps, the engine restarts
  (`homeassistant: start`, `for:` timers and delays start over) and nothing is compared for
  120 s. Five gaps: 11-05 10:41 → 13:19 (the restart, 2 h 38 min), 11-05 13:20 → 13:21,
  11-06 05:33 → 05:34, 11-06 05:58 → 05:59, 11-07 08:08 → 08:11.
- **Fields.** Every `Derived` field against its output entity (`ha::outputs`), with the shadow
  tolerances; a Rust sample at `t` agrees when it matches any value HA showed in `[t − 5 s,
  t + 5 s]`.
- **Actuators.** Per key (`shadow::commands`: the SmartEVSE override and mode, the cable, the
  Solis RC mode/powers/timeout/switches/backflow, the aircos, and the latches
  `batterij_net_laden`, `soluna_absorptie_bezig`, `ev_hoger_laadstil`,
  `smartevse_plug_laatst_bekend`, `batterij_setpoint_bias`):
  - *timeline*: % of seconds where Rust's intended value matches a value HA showed within ±5 s.
    Devices: the last value Rust commanded; an HA change Rust did not ask for becomes Rust's
    intent after 40 s of Rust silence (in lockstep it sees HA's value, and its controllers run
    at least every 30 s). Latches: Rust's after-tick value;
  - *HA changes* matched by a Rust act with that value within ±40 s;
  - *Rust acts* matched by HA showing that value within ±40 s, and the subset that are *edges*
    (a new value — re-sends of the current value are not edges).
  The Solis RC power numbers show HA's writes (`3331.0`) and, in between, the integration's
  register read-backs (`3330`, `0`: 10 W resolution and the 5 min cache in FINDINGS); only the
  writes count as HA's.
- **Probes.** The Buzz's `ev_laden_buzz` runs on a 25 s debounce whose phase is
  `last_triggered`, which the recorder does not have (automation entities are not recorded), so
  Rust and HA run seconds apart. The probe removes the phase: at every HA change of the override
  (and mode) it evaluates the Rust law on the state HA decided on — every row up to the change,
  with the charger's own answer to the command (charge current and power, a few ms before the
  override echo) rolled back.

## Results

Field agreement over all compared samples: **99.74 %** (90 fields compared, 67 at 100 %).

### Fields below 100 %

| field | agreement | mean err | why |
| --- | --- | --- | --- |
| `batterij_geladen_dag_kwh` | 92.72 % | 0.0046 kWh | (b) integration timing: Rust 0.03–0.06 kWh low after hours of night grid charging (11-06 21:45 → 11-07 08:08); the source steps land on whole-second ticks |
| `kwartier_gemiddelde_w` | 95.84 % | 1.2 W | (b) `e / elapsed` right after the 30 s branch switch: one 1 Wh register step seen a fraction of a second apart is 120 W at elapsed 30 s (e.g. 11-07 06:15:35) |
| `ev_laadvermogen_smartevse_kw` / `_w` | 97.60 % | 0.0001 kW | (b) HA renders `unavailable` during the 2 min SmartEVSE dropouts (e.g. 11-07 06:32:23); the port shows 0 and carries availability in `ev_laadvermogen_smartevse_beschikbaar`, which agrees 100 % |
| `kwartier_toegestaan_vermogen_w` | 97.97 % | 1.2 W | (b) sub-second render timing: late in the quarter the value moves 5–20 W/s and HA renders at the P1 event, Rust at the tick after; 3–4 s episodes at hh:mm:30 (the `rem <= 30` clamp lands one render apart) |
| `kwartierprijs_volgend` | 99.07 % | 0.0002 € | (b) the recorder drops attributes over 16 KiB: once tomorrow's prices are published the `prices` list is stored as `{}`; the replay keeps the last recorded list, so the 23:45 quarter has no "next" price (Rust falls back to the current price) |
| `batterij_reservering_effectief_w` | 99.25 % | 0.7 W | follows `kwartier_toegestaan_vermogen` (same 3–4 s episodes) |
| `batterij_ontladen_dag_kwh` | 99.42 % | 0.0023 kWh | (b) as `batterij_geladen_dag` |
| `batterij_setpoint_w` | 99.54 % | 0.15 W | (b) follows the toegestaan/median timing; worst 2442 W for one sample at a branch flip |
| `sun_above_horizon` | 100 % | – | fixed: HA's `sun.sun` is `round(refracted elevation, 2) > −0.833`, re-evaluated only at solar events and every 2 min in twilight (`sun/entity.py`), so it flips ~1½ min before astral's sunrise and ~4 min after sunset. `core/src/sun.rs` reproduces the schedule; every recorded transition matches to the second (`sun::tests::flips_when_ha_recorded_it`) |
| `zelfvoorziening_vandaag_pct` | 99.70 % | 0.003 % | ratio of two small daily meters just after midnight |
| `p1_mediaan_w` | 99.77 % | 0.09 W | (b) sample timing inside the 10 s window |
| `zelfverbruik_vandaag_pct`, `_kwh` | 99.82 / 99.87 % | – | as above |
| `huisverbruik_*`, `pv_vermogen_totaal_w`, `kwartiervermogen_projectie_w`, `kwartiervermogen_vastgelegd_kw`, `pv_overschot_beschikbaar_w` | ≥ 99.83 % | < 0.5 W | (b) sub-second timing; the sunset episodes of `sun.sun` |

Never compared: `kwartier_next_reset` — the recorder does not store `next_reset`.

### Actuators

| key | timeline | HA changes matched | Rust edges matched | Rust acts matched |
| --- | --- | --- | --- | --- |
| `evse.override_a` | 86.52 % of 57 652 s | 158/384 | 152/292 | 172/319 |
| `evse.mode` | 93.21 % of 83 122 s | 148/239 | 145/158 | 155/171 |
| `solis.rc_charge_w` | 98.50 % of 389 280 s | 2329/2488 | 1881/2020 | 9889/10243 |
| `solis.rc_discharge_w` | 99.34 % of 387 480 s | 2362/2409 | 1576/1616 | 9889/10138 |
| `solis.rc_mode` | 99.89 % of 395 485 s | 25/25 | 23/26 | 12855/12858 |
| `solis.allow_grid_charge` | 99.98 % of 395 467 s | 216/217 | 215/215 | 12856/12856 |
| `latch.batterij_setpoint_bias` | 99.99 % | 848/878 | 852/881 | 852/881 |
| `latch.soluna_absorptie_bezig` | 100.00 % | 228/230 | 229/229 | 229/229 |
| `latch.batterij_net_laden` | 100.00 % | 4/4 | 4/4 | 4/4 |
| `latch.ev_hoger_laadstil` | 100.00 % | 2/2 | 2/2 | 2/2 |
| `latch.smartevse_plug_laatst_bekend` | 100.00 % | 5/5 | 5/5 | 5/5 |
| `solis.rc_timeout_min` | 100.00 % | – | – | 12856/12856 |
| cable, aircos, feed-in, backflow | – | 0 | 0 | 0 |

**Buzz override at HA's decisions** (probe): 384 HA changes, **90.1 % within 0.05 A, 100 %
within 0.5 A**, mean error 0.010 A. The remaining 0.1–0.2 A are the `|round(1, 'floor')` of the
modulator law on inputs a few W apart (e.g. 11-07 11:40:00: HA 18.0, Rust 18.2).

Why the rest disagree:

- `evse.override_a` timeline and edges: (b) the 25 s debounce phase (`last_triggered` not
  recorded). During a ramp (11-06 21:50 → 22:00 at night under `Max tot piek`, 11-07 late
  morning on solar) both sides step every 30 s, seconds apart, so each value differs by
  0.1–0.3 A for part of every cycle. The probe shows the decisions themselves agree.
- `evse.mode`: (b) the same phase, plus the `Connected to EV` refusal cycles of 11-06 22:2x–23:4x
  (the car at 43 % does not draw: Normal → Pause → Normal every 1–4 min). The pattern is the
  same on both sides — Rust's handshake recovery reproduces HA's at 22:20:53/22:21:23 to the
  second — but which run lands first depends on the phase. (The mode probe only models
  `ev_laden_buzz`; most HA mode changes come from the handshake recovery and `peakguard`, so its
  50.6 % is not an agreement figure.)
- `solis.rc_charge_w` / `_discharge_w`: (b) the setpoint inherits the sub-second
  `toegestaan`/median timing (6–20 W apart late in a quarter, 11-03 01:53–01:58); and 206
  Rust-only zeros 11-03 00:02–01:45 while HA's numbers were still `unknown` (first written
  01:45:30).
- `solis.rc_mode`: (c) the Solis RC select does not always show HA's write: at 11-03 09:20:00
  HA's `peakguard_herstel` did run (it wrote discharge 0.0 at 09:20:00.006) and selected
  `None`, the dispatcher re-selected Discharge 80 ms later, and the select never showed
  `None`; at 11-06 18:09:30 HA wrote charge 390 W while the select kept showing Discharge
  (FINDINGS: `solis_modbus` RC select and its register cache).
- `solis.allow_grid_charge`: one HA-only `off` 0.2 s before the dispatcher's `on`
  (11-06 14:37:00.264).
- `latch.batterij_setpoint_bias`: 30 of 878 HA values 0.5–2.6 W apart (the P1 median at the
  instant of the dispatcher run).
- `latch.soluna_absorptie_bezig`: 2 HA-only `off` (11-03 11:17:30, 11-06 12:04:30), dispatcher
  runs at a setpoint within ±30 W of the threshold.

Without lockstep the decision logic drifts from HA's state as designed: the bias integral is a
free-running integral (18 % timeline), and everything that reads it follows (RC charge timeline
72 %). The Buzz probe still agrees 85.9 % within 0.05 A.

## Porting bugs found and fixed (core)

| What | HA behaviour | Test |
| --- | --- | --- |
| Integrals integrated every engine tick as a source event | HA writes a template sensor only when its rendered value changes (`async_track_template_result` drops identical results), so `integration` integrates between *changes*: a step is a trapezoid back to the previous change, with the `max_sub_interval` timer in between. Recorder 11-07 01:26:06, `batterij_laadvermogen` 0 → 2126 W | `meters::tests::integral_events_are_source_changes_not_ticks` |
| `now()` templates recomputed every tick | A state-based template re-renders when an entity it read last time changes, and at the top of each minute. `Kwartier toegestaan vermogen` held 5265 W for 25 s after `rem <= 30` (11-06 16:29:30) while the P1 register stood still; `Kwartier gemiddelde` stops reading P1 after 30 s (11-03 02:15:33: 2936 W held while P1 went 504 → −927 W) | `derived::tests::toegestaan_keeps_its_render_until_an_input_changes`, `derived::tests::gemiddelde_stops_reading_p1_after_30_s`, `render::tests::renders_on_input_change_and_each_minute` |
| Minute re-render visible to the automations of second 0 | HA's top-of-minute refresh lands after the time-pattern automations: `soluna_dispatcher` wrote 3331 W at 01:46:00.081 from the old render, `toegestaan` re-rendered at 01:46:00.352 (11-03) | `render::tests::renders_on_input_change_and_each_minute` (16:30:00 keeps, 16:30:01 renders) |
| P1-fed meters updated after the templates | HA's quarter/import/export meters and the P1 median update on the P1 event itself, before the templates that read them; the engine updated them at the end of the tick, one tick late | `engine::tests::p1_meters_update_before_the_templates` |
| P1 median sampled every tick | `statistics` takes one sample per P1 report (~5 s); a reading held for 9 s was nine samples | `meters::tests::median_samples_per_p1_reading_not_per_tick` |

Effect on the replay (first full run → final): field agreement 96.7 → 99.7 %;
`kwartier_toegestaan_vermogen` 67 → 98 %, `kwartier_gemiddelde` 75 → 96 %,
`batterij_reservering_effectief` 80 → 99 %, `huisverbruik_dag` 63 → 100 %,
`batterij_geladen_dag` 83 → 93 %, `batterij_setpoint` 97 → 99.5 %; RC charge power timeline
93 → 98.5 %, discharge 97 → 99.3 %, bias edges 640 → 848 of 878.

## Replay artefacts handled in the replay

- Initial states (step 2 above).
- `{}` attributes: the recorder dropped them (over 16 KiB); the last recorded ones are kept.
- `next_reset` is not recorded: an output compared on a missing attribute has no HA value.
- Solis RC power read-backs are not HA actuations.
- Helpers adopted from before the step's rows (lockstep).
- Meters re-seeded at the end of every settle window.

## Not explained / open

- (resolved) `sun.sun` sampling: see the field table.
