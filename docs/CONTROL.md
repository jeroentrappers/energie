# Control mode

How `energie` carries out its decisions once it, not Home Assistant, is in charge, and why that
is the same thing HA does today. The decisions themselves are unchanged (see the replay
comparison in `REPLAY.md`); this page is about the wire.

Sources, as copied from the live install: `solis_modbus` (`sensor_data/hybrid_sensors.py`,
`switch_sensors.py`, `select_sensors.py`, `sensors/solis_*.py`, `modbus_controller.py`,
`client_manager.py`, `data_retrieval.py`), `tuya_local` `devices/kolanky_evcharger.yaml`, the
scripts `laadkabel_start_laden` / `laadkabel_stop_laden`, the original automations, and the SmartEVSE discovery configs of the passive
MQTT capture of 2026-11-07.

## Solis S6-EH1P (Modbus TCP, Waveshare gateway, unit 1)

Register numbers are Modbus addresses; 3xxxx input registers (FC 4), 4xxxx holding registers
(FC 3, written with FC 6, one register per write) — `data_retrieval.py:558`
(`is_holding = start_register >= 40000`), `modbus_controller.py` `write_register`.

### What the logic reads and writes

| HA entity (`…_2`) | Register | Decode / encode (HA) | energie | HA source |
| --- | --- | --- | --- | --- |
| `sensor.solis_s6_eh1p_total_pv_power` | in 33057-33058 | S32 high word first, `multiplier 0` = as is, W | same | `hybrid_sensors.py:355`, `solis_base_sensor.py` `_convert_raw_value` (2 registers → `split_s32`) |
| `sensor.…_battery_soc` | in 33139 | U16 %, spike filter: 0 or 100 only on the 3rd poll in a row | same | `hybrid_sensors.py:727`, `data_retrieval.py:608` |
| `sensor.…_battery_power_net` | in 33149-33150, in 33135 | S32 ×0.1, then `round(p × 10)`, negated when 33135 = 0 (charging) | same (S32 W, sign from 33135) | `hybrid_sensors.py:823, 4690`, `solis_derived_sensor.py` (`len == 3` branch) |
| `select.…_rc_force_charge_discharge` | hold 43135 | `None` 0, `Solis RC Force Battery Charge` 1, `…Discharge` 2; select writes the value, then **re-writes 43136, 43129, 43282 from the register cache** (skipped when not cached); written even when unchanged | same order and values; one deviation, below | `select_sensors.py:27-40`, `solis_select_entity.py:98, 127` |
| `number.…_rc_force_battery_charge_power` | hold 43136 | ×10 W; `round(value / 10)` (Python, ties to even); skipped when the entity already shows the value; 0 .. protocol ceiling | same | `hybrid_sensors.py:2484`, `solis_number_sensor.py:145-176` |
| `number.…_rc_force_battery_discharge_power` | hold 43129 | ×10 W, as above | same | `hybrid_sensors.py:2383` |
| `number.…_rc_timeout` | hold 43282 | minutes, 1..30 | same | `hybrid_sensors.py:4225` |
| `number.…_backflow_power` | hold 43074 | ×100 W, 0..20000 | same | `hybrid_sensors.py:2304` |
| `switch.…_grid_feed_in_power_limit_switch` | hold 43073 bit 4 | read-modify-write from the cache (a live read when nothing is cached, never from 0, issue #402); written only when the register changes; other bits kept | same | `switch_sensors.py:26-28`, `solis_binary_sensor.py:163-225` |
| `switch.…_allow_grid_to_charge_the_battery` | hold 43110 bit 5 (set = allowed) | as above; bits 0-4, 6-11 (storage mode, reserve, peak shaving) kept | same | `switch_sensors.py:40-54` |

The number's "already shows the value" is `_attr_native_value`, which after HA's own write is
the register echoed back × multiplier (`notify_register_update` after `cache_save`); energie
compares the cached register × step, the same number. A request that does not land on the step
(1505 W) is written again on every call in both.

### Reads beyond the logic

Read-only device entities (`solis_<key>`) for what HA's Energy dashboard and the dashboards show
today: lifetime and daily PV, battery charge/discharge and grid import/export energies, battery
voltage, current and SOH, the SoC limits, the work mode (the Storage Mode select's option) and the
peak-shaving settings. Map, decodes and HA entity ids: `crates/drivers/src/solis/extra.rs`
(`HA_ENTITIES`). A block the inverter refuses blanks only its own entities.

### Pacing, sessions

| | HA (`solis_modbus`, this install) | energie |
| --- | --- | --- |
| Masters | one (HA) | one: HA's entry must be disabled first |
| Gap between frames | 310 ms, reads and writes (`client_manager.py:16, 112`) | 310 ms (`frame_gap_ms`) |
| Timeout, retries | 5 s, 1 (`client_manager.py:41`) | 5 s; a reused session that fails is re-opened once |
| Measurements | fast groups every 15 s (8 groups) | every poll, 15 s (2 blocks) |
| Settings (43xxx) | slow, 300 s; normal 60 s for 43007.. | every 4th poll (60 s, `settings_every`) and on the poll after a write (the read-back) |
| Energies | slow, 300 s | every 5th settings read (300 s) |
| Requests | ≈ 0.65 / s | ≈ 0.26 / s |
| Writes | queued, one at a time, 310 ms apart (`modbus_controller.py:146`, `PARALLEL_UPDATES = 1`) | one device task, one write at a time, 310 ms apart |

HA's dispatcher selects the RC mode every 30 s, and the select writes four registers each time
(43135 and three companions): 12 999 register writes on the simulated day. Solis does not document
which of these registers end up in flash, so energie does not select a mode the inverter already
holds (next section). The same day then takes 2 144 writes:

| Register | energie | HA |
| --- | --- | --- |
| 43135 RC mode | 206 | 2 919 |
| 43136 RC charge power | 685 | 3 399 |
| 43129 RC discharge power | 890 | 3 604 |
| 43282 RC timeout | 205 | 2 919 |
| 43110 storage control | 158 | 158 |

What is left are real changes: a new mode (with its companions, which the firmware needs right
after the mode to latch the setpoints and the timeout, issue #352 of the integration) and new
power setpoints. In the house a held forced mode adds one 43135 keepalive every 5 minutes
(at most 288 a day).

### Deviation: an RC mode already held is not selected again

The RC registers fall back to self-use after the RC timeout (43282) unless 43135 is written again,
so a forced mode needs a keepalive, but not the three companions every 30 s. When the register
cache shows the requested mode and a valid timeout:

- `None`: nothing is written (there is nothing to keep alive);
- a forced mode: only 43135 is written, at most every `rc_keepalive_s` (300 s) and at least every
  half RC timeout, so it never lapses;
- a different mode, or an invalid timeout: the full select as HA does it.

The cache is the last read (every 60 s, and on the poll after any write, so a failed write is read
back) or our own last write. `rc_keepalive_s = 0` restores HA's behaviour (every command selects).
To check on the real inverter (checklist item 9): after a keepalive, 43136, 43129 and 43282 still
hold their values and the forced mode keeps running past the RC timeout.

### Deviation: the RC timeout always goes with a forced mode

HA skips a companion it has no cached value for, so a select right after a reload can enable a
forced mode without re-writing 43282. energie writes `rc_timeout_fallback_min` (10, what the
automations use) as the third companion when 43282 is not cached or outside 1..30, so a forced
mode always times out to self-use. With a valid cached timeout (every normal case) the writes
are identical to HA's.

### Before and after this change

| Was (shadow-only driver) | Now |
| --- | --- |
| 300 ms frame gap | 310 ms, as HA |
| Every block every 5 s (1.2 requests/s, twice HA's load) | 15 s / 60 s / 300 s as above |
| A forced mode could be selected without a timeout write | the fallback timeout, above |

No register, bit, encoding, rounding or skip rule differed from HA.

## Kolanky cable (Tuya 3.4, `tuya_local` `kolanky_evcharger`)

| HA | DP | energie |
| --- | --- | --- |
| `select.ev_charging_cable_charging_operation` `Start charging` / `Stop charging` / `No operation` | 124 `OpenCharging` / `CloseCharging` / `WaitOperation` | same |
| `number.ev_charging_cable_charge_current` | 114, 6..16 A (115, 8..32 A when DP 113 = `Max32A`) | same; out of range is refused like `number.set_value` |
| `script.laadkabel_start_laden` (`mode: restart`) | 124 `OpenCharging`, `delay: 3 s`, then the current | same; a new start during the wait replaces the pending one; a stop does not cancel it (the stop script does not either); `start_delay_ms` 3000 |
| `script.laadkabel_stop_laden` | 124 `CloseCharging` | same |
| `homeassistant.reload_config_entry` (autoheal) | session | `CableReconnect`: the session is dropped and renegotiated |

Sets go as `CONTROL_NEW` with `{"protocol":5,"t":…,"data":{"dps":{…}}}` (tinytuya's 3.4 payload).
One local session (the cable takes one client: HA's `tuya_local` entry must be disabled first),
heartbeat every 10 s, the link counts as lost after 3 silent periods, reconnect after 5 s.
Extra read-only entities (`cable_<key>`): temperature, session energy, charging time, the raw
status and operation, the fault bitfield, delay and duration (`tuya/cable.rs` `HA_ENTITIES`).

No discrepancy with HA was found.

## SmartEVSE (Buzz, MQTT)

| HA | Topic | Payload | QoS / retain | energie |
| --- | --- | --- | --- | --- |
| automations `mqtt.publish` | `SmartEVSE/1234/Set/CurrentOverride` | tenths of A (`145`; `60` for 6 A) | 0 / false (`mqtt.publish` defaults) | same |
| `select.smartevse_1234_mode` | `SmartEVSE/1234/Set/Mode` (discovery `command_topic`, no `command_template`) | the option: `Off` `Normal` `Smart` `Solar` `Pause` | 0 / false (MQTT select defaults) | same |

The number entity's `command_template` (`{{ value | int * 10 }}`, whole amps) is not used, as in
HA. The harness checks that a new subscriber to `Set/#` gets nothing retained.

## Safety

| Mechanism | What it does |
| --- | --- |
| Fail-safe | The Solis RC timeout is always set with a forced mode (above), so after a crash, a stop or a lost link the inverter returns to self-use within 10 min by itself. At a clean shutdown `release_on_shutdown` (default `["solis"]`) selects RC mode `None` at once; `evse` (override 0: the charger's own current) and `cable` (stop) can be added. The EV chargers otherwise keep their last setting, as when HA stops. |
| Start-up | The first tick waits (up to `startup_wait_s`, 60 s) until every controlled device has been read; a device without a good reading gets no command (`blocked: no reading of … yet`). The drivers' skip rules then compare against the device's actual state, so a start writes only what differs. |
| Rate limit | Per device, a token bucket of `max_commands_per_min` (30). The simulated day's busiest minute is 15 commands for one device, so it never alters behaviour; it stops a runaway loop. |
| Kill switch | `control_enabled` (entity, dashboard *Systeem → Sturing*, `POST /api/control {"enabled": false}`) stops all actuation at once: new commands are blocked and jobs already queued are dropped by the device task before they are written. Per device: `control_<evse|cable|solis|airco|prices>` or `{"device": "solis", "enabled": false}`. Persisted in `<state_dir>/control.json`, so a stop survives a restart (the file wins over the config). |
| Audit | Every real command: the driver's wire writes (register / DP / topic, value, the value it replaced), the World field that must show it, its value before, wanted and read back, and the result (`verified`, `unchanged`, `sent`, `superseded`, `failed`, `error`). JSONL, `<state_dir>/control-audit.jsonl`, rotated to `.1` past `log_max_bytes` (4 MiB); the latest 50 on the dashboard and in `/api/control`. |
| Read-back | A command whose field (`control/verify.rs`) does not show the value within `verify_s` (60 s) is `failed` and raises a high-priority notification (at most one per field per `alarm_interval_s`, 30 min). The same command re-sent keeps the first deadline, so a write that never takes still alarms. A driver error or refused queue is an `error` and alarms per device. |

## Dry run

```toml
mode = "control"
[control]
dry_run = true
```

Native reads from every device, full decisions, and every command handed to its driver's `plan`:
the driver works out the exact writes against the device's real state (the Solis cache, the
cable's DPs, the charger's topics) and nothing is sent — not the release at shutdown either. The
`ha` actuator reports the service call it would make. Output: `<state_dir>/control-dryrun.jsonl`
(`result: dry_run`, the `writes` with `before`), the *Sturing* table on the dashboard, and
`/api/control`. Because nothing is written, a write that control mode would make once (a new RC
power) shows up again on every cycle.

A dry run needs the native drivers to be the only Solis master and the only cable client, so
HA's `solis_modbus` and `tuya_local` cable entries are disabled for it (and HA's energy
automations, which would fail without them). During the dry run nobody steers: the inverter is in
self-use within 10 min, the chargers keep their last setting. The SmartEVSE can stay in HA in
parallel (MQTT, and a dry run publishes nothing).

## The end-to-end harness

`crates/app/src/control/e2e.rs`: a June day (sun; the Buzz plugs in at 09:30, the EV3 at 11:00;
both aircos on the 17:30-19:30 evening peak), 5 s ticks. The house model and the engine make the
decisions; the runtime in control mode gets the same Worlds and must make the same commands, tick
by tick (17 280 ticks, all equal). Its commands go through the gate, the device queues and the
native drivers to the Solis, Tuya and SmartEVSE simulators. `HaModel` is a transliteration of
the HA side (`select.py` companions from the cache, `number.py` skip and rounding, `switch.py`
bit RMW, the cable scripts with their 3 s and `mode: restart`, `mqtt.publish`). Result: 12 999
Modbus writes, 584 DP sets and 368 publishes, identical in order and value to `HaModel`; none
retained; 0 commands blocked; every command kind the logic sends to these devices was exercised
(`SolisFeedInLimit` / `SolisBackflowW` need negative prices and are checked by hand).
`control/live.rs` runs the whole binary on the wall clock against the simulators: start-up wait,
a command from the API verified by its read-back, the kill switch, the release, the dry run.

## To check on the real hardware (dry run, then the first hour of control)

1. **Solis reads**: `world_battery_inverter_*` against HA's entities from just before the entry
   was disabled (SoC, battery power sign: charging negative, PV power, RC mode/powers/timeout,
   the two switches, backflow 5500 W). `driver:solis` health: no read errors at 15 s.
2. **Solis extra blocks**: every `solis_*` entity has a value (43011-43024, 43483, 43487-43488,
   33029-33035, 33161-33176, 33133-33134 are readable as blocks on this firmware); the lifetime
   energies equal HA's last `sensor.solis_s6_eh1p_*_total_*` states; `solis_work_mode` equals
   `select.solis_s6_eh1p_work_mode_2` (energie does not hide the V1 TOU options).
3. **Dry-run writes**: in `control-dryrun.jsonl` the `before` of each register is what HA showed
   (43110 = 17, 43073 = 16, 43282 = 10, …) and only bit 5 of 43110 / bit 4 of 43073 ever change.
4. **Gateway**: no timeouts or reconnect storms with energie as the only master
   (`driver:solis` `read_errors`, `consecutive_errors`).
5. **RC timeout fail-safe**: after the cutover, switch control off and confirm 43135 reads 0
   (`rc_mode` `None`) within 10 min.
6. **Read-back timing**: `verified` entries for the Solis within one poll (≤ 15 s); a `failed`
   one means the inverter did not take a write.
7. **Cable**: energie gets the session once `tuya_local` is disabled; DP 114 is pushed back after
   a set (verification); what DP 124 reads after `OpenCharging` (`cable_operation`); a start
   charges (the 3 s rule).
8. **SmartEVSE**: the broker lets energie's client publish on `SmartEVSE/1234/Set/#`;
   `ChargeCurrentOverride` echoes the published tenths and `Mode` the option.
9. **RC keepalive**: with a forced mode held for over 10 min, `control-audit.jsonl` shows a 43135
   write every 5 min and nothing else for the RC; 43136, 43129 and 43282 keep their values and
   the mode never drops to `None`. If it does, set `rc_keepalive_s = 0` (HA's behaviour).
10. **HA**: its energy automations are off (two controllers fighting over the Buzz can latch the
   charger's "Laadstation controleren" fault).

## Config

| Key | Default | |
| --- | --- | --- |
| `[control] enabled` | `true` | actuation at the first start (afterwards `control.json`) |
| `dry_run` | `false` | plan and log only |
| `devices` | all `true` | per-device enable at the first start, `{ solis = false }` |
| `verify_s` | `60` | read-back window |
| `max_commands_per_min` | `30` | per device |
| `startup_wait_s` | `60` | wait for the first readings |
| `release_on_shutdown` | `["solis"]` | `solis`, `evse`, `cable` |
| `audit_file`, `dry_run_file` | `<state_dir>/control-audit.jsonl`, `…/control-dryrun.jsonl` | |
| `log_max_bytes` | `4194304` | rotation |
| `alarms`, `alarm_interval_s` | `true`, `1800` | read-back notifications |
| `[[device]] kind = "solis_modbus"`: `frame_gap_ms`, `timeout_ms`, `settings_every`, `rc_timeout_fallback_min` | `310`, `5000`, `4`, `10` | |
| `[[device]] kind = "tuya_cable"`: `start_delay_ms` | `3000` | |

The Solis poll default is now 15 s (HA's fast group).
