# energie — the binary

The application layer around `energie-core`: a 1 s tick loop, the Home Assistant bridge, the
shadow comparator, the Dutch dashboard, notifications and a simulated house.

```
energie --config /etc/energie/energie.toml        run
energie --check-config /etc/energie/energie.toml  validate and exit (0 ok, 2 bad)
energie import-settings --from-ha --config <file> [--force]
                                                 HA's helpers into the state dir, read-only, once
energie replay --history <export.csv[.gz]> [--from T] [--to T] [--step-ms 1000]
               [--report out.json] [--summary out.md] [--lockstep | --no-lockstep] [--trace <field|key>]
energie --version | --help
```

`replay` runs the engine over an HA recorder export and compares every derived field and
actuation with what HA recorded; see `docs/REPLAY.md` for the method and the latest numbers.

Logs go to stderr (`RUST_LOG`, default `info`; colours only on a terminal).

## Modes

| Mode | Reads | Actuates | Notifies | Clock |
| --- | --- | --- | --- | --- |
| `shadow` | per `[[device]]` `role` (unbound: `ha`) | never — every command is logged, whatever `control` says | log only (HA still sends the real ones) | wall clock |
| `control` | per `[[device]]` `role` (unbound: `ha`) | per `[[device]]` `control` (`log` by default), through the gate of `[control]` (kill switch, start-up wait, rate limit, audit, read-back; `dry_run` writes nothing), see `docs/CONTROL.md` | `[notify]` | wall clock |
| `simulate` | the house model (default) | the house model (default) | `[notify]` | virtual: `tick_ms` of model time per tick, `sim.speed` ticks per `tick_ms` of wall time |

**Standalone** (control mode without `[ha]`, or with `[ha] import_only = true`): every World
section comes from a `[[device]]`; a section no device reads is `unavailable` (every field
`None`, as HA's entities without their integration) instead of falling back to HA, and
`--check-config` warns about each one (`no device reads load:vriezer ...`). No HA WebSocket
session exists, settings are local, notifications go through `[notify]` (ntfy), and HA, if
kept, is a view over MQTT (`docs/MQTT.md`). `deploy/energie.standalone.example.toml` is this
house in that form.

**Shadow lockstep** (`shadow.lockstep = true`, the default): before every tick Rust adopts HA's
Settings and Memory (helpers, latches, counters, automation `last_triggered`), so the comparison
isolates the decision logic from drift in hidden state. The dashboard controls are read-only
then. Meters (`Memory.meters`) integrate on Rust's own; a meter that differs from HA's entity
by more than max(0.1, 0.2 %) for 120 s (HA restart, manual reset, drift) is re-adopted from HA
(`ha::mapping::MeterAdopter`), logged (`kind: meter`) and counted (`meter_adoptions` in
`/api/shadow`).

**Settings ownership** (`settings_source`): `ha` (the default in shadow mode) keeps today's
behaviour: HA's `input_*` helpers are adopted every tick (shadow lockstep also adopts the
Memory) and the local controls are read only (`409`); outside shadow mode `ha` adopts only the
helpers. `local` (the default otherwise) makes the binary the owner of the settings and of the
Memory it used to mirror (latches, counters, the night measurements,
`capaciteit_piekhistoriek`, the meters): kept in `<state_dir>/settings.json` and `memory.json`
(atomic writes, at most once per `persist_interval_s` and at once on a settings change),
changed from the dashboard, `POST /api/settings`, `POST /api/entities/<id>` and MQTT
`/set`. The Memory fields are read-only entities (group `memory`).

On first start (no `settings.json`/`memory.json` in the state dir) and with `ha.seed = true`,
Settings and Memory are seeded from HA's helpers, and every utility meter / integral from its
HA entity (`ha::mapping::seed_meters`: value, `last_reset`, `last_valid_state`); with
`[ha] import_only = true` that is one read-only `get_states` and nothing else. `energie
import-settings --from-ha --config <file>` does the same on demand (it refuses to replace
existing files without `--force`; stop the service first). After that the state dir is the
truth.

## Architecture

```
            ┌──────────── sources (per World section) ────────────┐
 HA WS ───► │ HaSource (store mirror)   SimHandle (house model)   │  DriverSource (device task cache)
            └───────────────┬──────────────────────────────────────┘
                            ▼ Sources::assemble(now) → World
 tick (1 s) ─► Runtime::step ─► [lockstep: Settings/Memory ← HA] ─► Engine::tick
                            │
          commands ─────────┼──► Router → Actuator (log | ha | sim | driver)   [shadow: log only]
          Notify/Persistent ┼──► Notifier (log | ntfy) + dashboard list
          derived, commands ┼──► Shadow comparator (fields + actuations, JSONL log)
          Settings/Memory ──┼──► Persister (atomic JSON, ≤ 1×/60 s, at once on a UI change)
          series registry ──┼──► History (10 s / 1 min / 15 min / day tiers, JSONL segments)
                            ▼
                   Snapshot (watch channel) ──► axum: /, /api/*, SSE, /healthz
```

| Module | |
| --- | --- |
| `plug` | `Section`, `Device` and their role slots, the **`WorldSource`** / **`Actuator`** / **`DriverFactory`** / **`DeviceView`** traits, `Sources::assemble`, `PvSum`, `Router`, `DriverHealth` |
| `registry` | the driver kinds by name: the `ha` and `sim` bridges and every `energie-drivers` and `energie-cloud` factory |
| `plan` | `[[device]]` tables → checked instances and role bindings (who reads each slot, who carries out each device's commands) |
| `legacy` | the deprecated `[sources]` / `[actuators]` / `[devices.*]` tables → `[[device]]` instances |
| `drivers` | `Drivers`, the `DriverFactory` over the native instances (one task per instance) |
| `entities` | the entity registry: World by role, device instances, `Derived`, `Settings`; `[[publish]]` rules; entity → command |
| `config` | the TOML file |
| `runtime` | `Runtime::step` (synchronous, no network), `Snapshot` |
| `app` | wiring: start the HA client, MQTT listener, tick loop and web server; stop them |
| `ha::ws` | HA WebSocket client: auth, `subscribe_events(state_changed)`, `get_states`, `call_service`, ping, reconnect with backoff |
| `ha::entity` | HA state objects, typed parsing, the store mirror |
| `ha::mapping` | declarative tables: HA entity → `World` field, → `Settings`, → `Memory` |
| `ha::outputs` | every `Derived` field ↔ the HA entity that shows it, with its default tolerance |
| `ha` | the `ha` source and actuator (command → the service call HA's automations use) |
| `shadow` | field parity, actuation parity, the mismatch log |
| `mqtt` | a passive MQTT 3.1.1 subscriber for the shadow comparison (no PUBLISH code exists) |
| `bridge` | the MQTT bridge: `Publisher` (topics, throttling, `/set` parsing), `summary`, `discovery`, `ha_tables` (the cutover's takeover table, HA's units and state classes), `client` (rumqttc: last will, reconnects); see `docs/MQTT.md` |
| `import` | the one-shot settings/memory import from HA (`import-settings`, `[ha] import_only`) |
| `notify` | `Notifier`, `LogNotifier`, `NtfyNotifier` (ntfy JSON API) |
| `persist` | atomic writes, throttled saves |
| `sim::house` | the physics model; `sim::scenario` fast-forwards it with the engine on a virtual clock |
| `history` | the history store behind the charts: tiers, segment files, queries (`tier`), the series registry (`series`), HA recorder seeding (`seed`) |
| `web` | the dashboard (`page.html` + `assets/`, server-rendered controls), the JSON API, the price curves (`prices`) |
| `replay` | `energie replay`: the recorder export reader, the field/actuator comparison over a virtual clock, the decision probes, the report |

### Plugging in drivers

See [`docs/PLUGINS.md`](../../docs/PLUGINS.md) for roles, slots, the entity model and how to add
a driver kind. In short: a kind implements `energie_drivers::plugin::DriverKind` (its config
table, the slots it fills, a constructor), is registered in `registry::Registry::builtin`, and
is used as `[[device]] kind = "…" role = "…"`. `plan::Plan::resolve` binds every World section
to one reader (several for `pv`, summed by `plug::PvSum`) and every command device to at most
one controller; `app::start` asks the `DriverFactory` for the native instances by id and wraps
the `ha` and `sim` bridges itself:

```rust
pub trait DriverFactory {
    fn source(&self, id: &str) -> Option<Arc<dyn WorldSource>>;
    fn actuator(&self, id: &str) -> Option<Arc<dyn Actuator>>;
    fn devices(&self) -> Vec<Arc<dyn DeviceView>>;   // entities and health
}
```

`main` passes `drivers::Drivers` (tests may pass `NoDrivers`, which makes every native instance
fail with "no driver available for …"). Commands map to devices with `Device::of` (`Evse`,
`Cable`, `Solis`, `Airco`, `Prices`) and devices to slots with `Device::slot`; notifications go
to the notifier. `read` is called once per tick per distinct source.

### Native drivers (`drivers::Drivers`)

| Kind | Slots | Writable | Device |
| --- | --- | --- | --- |
| `homewizard` | `grid_meter` | | HomeWizard P1, HTTPS API v2 |
| `solis_modbus` | `battery_inverter` | yes | Solis S6 over the Modbus TCP gateway |
| `omnik` | `pv` | | Omnik `js/status.js` |
| `smartevse_mqtt` | `charger:buzz` | yes | SmartEVSE over MQTT |
| `tuya_cable` | `charger:ev3` | yes | Kolanky cable, Tuya 3.4 |
| `energy_charts` | `price_feed` | yes (refresh) | energy-charts, ENTSO-E fallback |
| `http_json` | `pv`, `grid_meter` | | one power value from any JSON endpoint |
| `static` | any | | fixed values from the config (stand-in, tests) |
| `kia_connect` | `car:ev3` | yes (`ac_limit` entity) | Kia Connect EU (`crates/cloud`) |
| `cupra_data_act` | `car:buzz` | | VW Group EU Data Act portal |
| `lg_thinq` | `load:airco` | yes (`Airco`) | LG ThinQ Connect |
| `fluvius` | `capacity_peak` | | Mijn Fluvius, monthly peaks |
| `forecast_solar` | `solar_forecast` | | forecast.solar (entities only) |

`load:vriezer` has no native kind yet; HA (or `static`) fills it.

`Drivers::new(&cfg)` starts one tokio task per native instance. The task owns the driver: it polls it every `poll_ms` and, between polls,
applies the queued commands, so a device is never read and written at the same time.

- **Reads**: `WorldSource::read` clones the task's cached section; the tick never waits on a
  device. A failed poll keeps the last good reading through `grace_polls` consecutive failures
  (default 3); the next failure writes the driver's `unavailable` reading (every field `None`,
  HA's `unavailable`) until a poll succeeds again. The first failure of a streak and the switch
  to unavailable are logged as warnings, recovery as info.
- **Commands**: `Actuator::submit` puts the command in a bounded queue (16) and returns at once;
  a full queue or a stopped task is an error on the command record. The task logs every outcome;
  device errors and commands the driver calls `NotMine` are counted.
- **Entities**: after every good poll the task also stores the driver's extra entities
  (`Source::entities`), `null` while the instance is unavailable.
- **Settable entities**: an extra entity with a `control` (the EV3's `ac_limit`) can be set
  through `POST /api/entities/<id>` while the instance has `read_only = false` and the mode is
  not shadow; the value goes through the same queue to the driver's `Actuator::set`. An extra
  with the key of a slot field replaces that field's entity.
- **Health**: `/api/state` → `drivers` maps `driver:<id>` to `{available, last_ok, reads_ok,
  read_errors, consecutive_errors, last_error, writable, commands_applied, command_errors,
  last_command_error}`.
- **Safety**: an actuator is built only outside shadow mode, for an instance with a `control`
  binding and `read_only = false`. The default is `read_only = true`; a `control` on a
  read-only instance is rejected by `--check-config`, and the factory refuses it as well. In
  shadow mode no command queue exists at all.
- Secrets (token, password, local key) are only ever read from the files named in the config.
- Dropping `Drivers` stops the device tasks.

## Configuration reference

See `deploy/energie.example.toml` for a commented file.

| Key | Default | |
| --- | --- | --- |
| `mode` | — | `shadow`, `control` or `simulate` |
| `listen` | `127.0.0.1:8080` | dashboard/API address (`:0` picks a port; the log says which) |
| `state_dir` | `/var/lib/energie` | `settings.json`, `memory.json`, the shadow log |
| `tick_ms` | `1000` | tick period |
| `persist_interval_s` | `60` | at most one state write per interval (unchanged files are never rewritten) |
| `[ha] url` | — | `http://host:8123` or `ws://…`; TLS is not supported (use the LAN/tailnet address) |
| `[ha] token_file` | — | file with the long-lived token |
| `[ha] seed` | `true` | seed Settings/Memory from HA when the state dir is empty |
| `[ha] start_wait_s` | `30` | wait this long for HA's first snapshot at start |
| `[ha] import_only` | `false` | HA only for the first-start import (one read-only `get_states`); no fallback, no WebSocket session, no actuators: standalone |
| `settings_source` | `ha` in shadow, else `local` | who owns Settings and Memory: `ha` adopts the helpers every tick, `local` keeps them in the state dir |
| `[[device]] id kind role control read_only poll_ms grace_polls name` + the kind's keys | `read_only = true`, `poll_ms` per kind, grace 3 | one device instance; see `docs/PLUGINS.md`. Unbound slots read from `ha` (`sim` in simulate; unavailable without an HA bridge); uncontrolled devices log (`sim` in simulate) |
| `[[publish]] match enabled min_interval_s deadband` | all published, no throttle | per-entity publish settings, glob on the entity id, later rules win |
| `[shadow] lockstep` | `true` | adopt HA's Settings and Memory each tick |
| `[shadow] time_tolerance_s` | `5` | a derived value matches if HA showed it within ±N s |
| `[shadow] command_window_s` | `40` | actuation matching window |
| `[shadow] log_file` | `<state_dir>/shadow-mismatches.jsonl` | mismatch onsets, JSONL |
| `[shadow] log_max_bytes` | `262144` | rotate to `.1` past this size |
| `[shadow.tolerances] <field>` | by unit suffix | `{ abs = …, rel = … }`: match if `|rust − ha| ≤ max(abs, rel·|ha|)` |
| `[mqtt] host`, `port`, `username`, `password_file`, `topic`, `client_id` | port `1883`, topic `SmartEVSE/1234/Set/#`, id `energie-shadow` | passive subscription for the SmartEVSE set topics |
| `[mqtt_publish] enabled` | `false` | the MQTT bridge (`docs/MQTT.md`); nothing is published without it |
| `[mqtt_publish] broker`, `port`, `username`, `password_file`, `client_id` | —, `1883`, —, —, `energie` | the broker |
| `[mqtt_publish] prefix`, `discovery`, `discovery_prefix` | `energie`, on outside shadow mode, `homeassistant` | topics and HA discovery |
| `[mqtt_publish] include`, `exclude` | `["*"]`, `[]` | globs on entity ids |
| `[mqtt_publish] summary_interval_s` | `5` | `<prefix>/summary` period |
| `[control] enabled`, `dry_run`, `devices` | `true`, `false`, all on | the kill switch and per-device enable at the first start (then `<state_dir>/control.json`), the dry run |
| `[control] verify_s`, `max_commands_per_min`, `startup_wait_s` | `60`, `30`, `60` | read-back window, per-device rate limit, wait for the first readings |
| `[control] release_on_shutdown` | `["solis"]` | at a clean stop: `solis` RC mode `None`, `evse` override 0, `cable` stop |
| `[control] audit_file`, `dry_run_file`, `log_max_bytes`, `alarms`, `alarm_interval_s` | `<state_dir>/control-audit.jsonl`, `…/control-dryrun.jsonl`, 4 MiB, `true`, `1800` | the JSONL logs, read-back notifications |
| `[notify] kind` | `log` | `log` or `ntfy` |
| `[notify] url`, `token_file` | — | ntfy topic URL (`https://ntfy.sh/topic`), optional bearer token file |
| `[sources]`, `[actuators]`, `[devices.<name>]` | — | deprecated, still accepted: translated into `[[device]]` instances (ids `homewizard solis omnik smartevse cable prices`), printed by `--check-config` with a warning; not together with `[[device]]` |
| `[history] enabled` | `true` | record the dashboard series |
| `[history] dir` | `<state_dir>/history` | segment files |
| `[history] minute_days`, `quarter_days`, `day_days` | `14`, `430`, `1100` | retention of each tier |
| `[history] seed_from_ha` | `false` | seed an empty store from HA's recorder statistics at start (read-only; needs `[ha]`) |
| `[history] extra_series` | `[]` | globs on entity ids (`fluvius_month_*`, `ev3_*`) recorded besides the curated series |
| `[sim] seed speed start solis_pv_peak_w omnik_pv_peak_w cloudiness battery_kwh battery_soc buzz_plugged buzz_soc ev3_plugged ev3_soc` | `1 1.0 now 5000 3000 0.3 10 50 true 40 true 55` | the house model |

Default field tolerances: `_w` 5 W, `_kw` 0.005, `_kwh` 0.01 (+0.1 %), `_eur` 0.01 (+0.1 % for
cumulative costs), `_pct` 0.5, timestamps 1 s, everything else 0.001; text compares
case-insensitively, booleans exactly.

## HTTP endpoints

| Route | |
| --- | --- |
| `GET /` | dashboard (Dutch, mobile-friendly, light/dark), the views of HA's `energie-beheer` dashboard: Overzicht, Capaciteitstarief, Prijzen, Batterij & Solis, EV laden, plus Systeem (commands, notifications, shadow parity) |
| `GET /assets/app.css`, `app.js`, `charts.js` | the page's stylesheet and scripts, compiled in (no CDN, works offline; the charts are hand-rolled SVG) |
| `GET /api/state` | the latest snapshot: world, derived, settings, memory summary, commands, notifications, HA/MQTT link, sources, actuators, shadow stats |
| `GET /api/settings` | `{editable, values, fields}` — `fields` is the control metadata (label, kind, unit, options) |
| `POST /api/settings` | partial change in the serde shape of `Settings`, e.g. `{"ev_net_niveau": "Uit"}` → `200` + new settings; `400` unknown field/bad value; `409` in shadow lockstep |
| `GET /api/shadow` | per-field and per-actuation agreement; `404` outside shadow mode |
| `GET /api/control` | control mode: the switches, the dry run, counters and the latest 50 audited commands; `404` otherwise |
| `POST /api/control` | `{"enabled": false}` stops all actuation, `{"device": "solis", "enabled": true}` one device; `409` outside control mode |
| `GET /api/entities` | every entity: `{id, group, key, name, class, unit, value, control, publish}` |
| `POST /api/entities/<id>` | `{"value": …}`: a setting → settings change, a World field → its command; `404` unknown, `409` not settable now (shadow, lockstep, no controller), `400` bad value |
| `GET /api/events` | Server-Sent Events: one `state` event per tick (the snapshot without the price slots, plus `entities`: the entity values changed since the stream's previous event) |
| `GET /api/history` | the history store: `{series, tiers, stats}`; `404` with `[history] enabled = false` |
| `GET /api/history?series=p1_w,soc_pct:last&from=…&to=…&step=300` | the series re-bucketed to `step` s over `[from, to)` (Unix seconds or RFC 3339; defaults: `to` = the last tick, `from` = `to` − 24 h, step 300); per series `:mean` (default), `:min`, `:max`, `:last`; a step that is a multiple of a day buckets by local day; at most 5000 buckets. `{from, to, step, t: [bucket starts], series: [{id, label, unit, agg, v: [value or null]}]}`; `400` for an unknown series, aggregate or time |
| `GET /api/prices` | the offtake quarter prices of today (and tomorrow), the injection prices recomputed from them, the lowest and highest quarter |
| `GET /healthz` | `200 {"status":"ok"}` while ticking; `503` `starting` / `stale` (no tick for 10 s) |

`web_password_file` turns on HTTP Basic authentication, and is required when `listen` can be reached from another machine. See `SECURITY.md`.

## Shadow comparison

- **Fields**: every `Derived` field against its HA entity (`ha::outputs`). Each tick's sample is
  judged `time_tolerance_s` later; it matches if any Rust value within ±N s is within tolerance.
  An entity HA does not have counts as *missing*, not as a mismatch.
- **Actuations**: Rust's commands and latch flips (`batterij_net_laden`,
  `soluna_absorptie_bezig`, `ev_hoger_laadstil`, `smartevse_plug_laatst_bekend`,
  `batterij_setpoint_bias`) against HA's state changes of the output
  entities (`number.smartevse_1234_charge_current_override`, `select.smartevse_1234_mode`,
  the cable's number/select, the Solis RC select/numbers/switches, the airco switches, the
  latch helpers) and, with `[mqtt]`, `SmartEVSE/1234/Set/CurrentOverride|Mode`. A Rust act
  matches if HA shows that value or made the same act within ±window (so re-sending the
  current value agrees); an HA act matches if Rust made it within ±window. Dropouts to/from
  `unavailable` are not actuations. Reconnects and price refreshes are not observable.
- **Log**: one JSONL line per mismatch *onset* (`kind`, `at`, `field`/`key`, `rust`, `ha`,
  `err`/`side`), rotated at `log_max_bytes`.

## Dashboard

The views of HA's `energie-beheer` dashboard (Warmtepompen and Aanwezigheid excepted), with
its Dutch labels, colours and chart series, on the port's own derived sensors, meters and
settings. Live values come from the SSE stream; charts from `/api/history` and `/api/prices`; a cloud
device's values beyond the World from `/api/entities`, found through the device that fills the
section (`driver:<id>` in `sources`).

| View | Contents |
| --- | --- |
| Overzicht | badges; power flow (grid, PV, battery with setpoint, house, EV3 cable, Buzz/SmartEVSE, airco state); Laaddoelen; *Energiestroom* chart with *Grafiekperiode* (6 h / 12 h / 24 h / gisteren / 7 dagen); *Zon & prijs* with *Voorspellingsperiode* (vandaag / vandaag + morgen / nu ±12u, the PV forecast of a `forecast_solar` device); Bediening (Modus & prioriteit, Sturing, Instelbare drempels with their → readouts, Auto's); Batterij, Net & fasebelasting, Zonnepanelen, EV's (with the cars' AC limit, charge target and odometer), Dagbalans, Nacht & overig tiles; Aircos (state and on/off switches, enabled while `world_load_airco_*_on` is settable) |
| Capaciteitstarief | *Dit kwartier: gemiddelde vs target* (30 s, from the 10 s tier); Mag nu / Projectie / Verbruikt dit kwartier; the ⏱️ Kwartier pace text; % van target per kwartier (12 h); maandpiek gauge; Maandpiek × tarief, Gefactureerde piek, Op factuur; Piekhistoriek table with a Fluvius column; Kwartiervermogen 24 h with target; Dagpieken 14 d; Fluvius-controle with the peak quarter and *Maandpieken — Fluvius vs lokaal* (the `capacity_peak` device's `month_MM_peak_kw` entities against the P1 piekhistoriek) |
| Prijzen | Ecopower kwartierprijzen today & tomorrow (threshold colours, extremes, injection line); Nu en vandaag tiles (lowest/highest quarter and time); Vaste kosten; Geldverbruik per kwartier; Energiekost per dag (14 d); Toelichting |
| Batterij & Solis | SoC gauge, energy, power, setpoint, charged/discharged today; the Solis RC registers (read-only); battery power & SoC 24 h; PV production 24 h (Solis + Omnik) |
| EV laden | Laadsturing text and the Modus & prioriteit controls; EV3, Buzz, laadkabel and SmartEVSE tiles (from the car slots; with a `kia_connect`/`cupra_data_act` device also charging, DC limit and last update, and an *AC-laadlimiet instellen* select while `<id>_ac_limit` is settable); solar/grid split; costs, zon-aandeel, per-100 km figures per car |
| Systeem | commands, notifications, sources/actuators/drivers, history tiers, *Apparaten* (every device instance's entities, live from `/api/entities` and the event stream), shadow parity |

Not reproduced, for want of data in the port: Solis voltage/current/SOH/work mode and its
inverter settings, the Solis and Omnik *today* registers (the port's own `pv_energie_dag` is
shown), the Kia/VW range, charging power and remaining time, cable temperature and session
energy, SmartEVSE max current/temperature/error, the Fluvius high/low tariff registers, and
the EMHASS plan. Forecast.Solar comes from a `forecast_solar` device (see below). *Energiekost per dag* takes each day's
last value of `energiekost_vandaag` (HA's chart took the day's max).

## History store (`history`)

| Tier | Bucket | Kept | On disk | Series |
| --- | --- | --- | --- | --- |
| recent | 10 s | 2 h | — (memory only) | every sampled series |
| minute | 1 min | `minute_days` | `minute/YYYY-MM-DD.jsonl` | every sampled series |
| quarter | 15 min | `quarter_days` | `quarter/YYYY-MM.jsonl` | the power and price series, the quarter peak and target |
| day | local day | `day_days` | `day/YYYY.jsonl` | daily meters, self-consumption, quarter peak (= dagpiek), month peak, SoC, nachtdekking |

- Each bucket keeps mean, min, max and last per series (`f32`; `null` when there was no sample).
- Every tick is a sample. A closed minute is appended to its day's segment: one ~300–500 byte
  append a minute, no fsync, no rewrites. Quarter and day rows are *derived from the minute
  rows* when a quarter closes and at start (every finished bucket with minute rows but no row
  of its own), so a restart or crash loses at most the open minute; the open minute is written
  at a clean stop.
- Segment format: a header line `{"series":[ids…]}` (written by each process before its first
  row in a segment, so the series list may change between versions), then `[t, v, v, …]` rows,
  each `v` `null`, one number (all four equal) or `[mean,min,max,last]`, rounded to the series'
  decimals. Loading skips lines that do not parse, cuts a torn last line off, sorts and merges
  duplicate buckets. Segments past the retention are deleted whole.
- Bounded: at the defaults about 6–10 MB of minute segments, 3 MB of quarter segments and well
  under 1 MB of day segments on disk; about 12 MB (minute) + 5 MB (quarter) of RAM.
- Queries pick, per day/quarter/minute cell and series, the tier that covers most of it (ties
  to the finer), so a 13-month range reads the quarter and day tiers where the minutes are gone.
- The store is keyed by series id. `history::series::SERIES` is the one list of curated series:
  id, Dutch label, unit, decimals, tiers, how it is read from a tick, and the HA entity with the
  same value. `History::record_by_id` takes values by id.
- **Entity series** (`[history] extra_series`, globs on entity ids, empty by default): every
  tick, each entity of the registry whose id matches and whose value is a number (a bool counts
  as 1/0) is recorded under its entity id, with the entity's name and unit; the series is added
  the first time the entity has a value, so a device whose entities appear after its first read
  is picked up too. Entity series live in the recent, minute and day tiers (not the quarter
  tier, which stays small); a curated id is never taken over. Segments get a fresh header when
  series are added, and at start the ids in the segment headers that match a glob are loaded
  again. Query them like any series: `/api/history?series=fluvius_month_11_peak_kw:max&step=86400`.
- **Forecast**: `pv_forecast_w` is not sampled; every tick the `power_w` forecast of the
  `solar_forecast` device (`forecast_solar`: the API's points as quarter-hour steps) goes to
  `History::set_forecast("pv_forecast_w", points)`, and the *Zon & prijs* chart and the
  *Forecast nu* tile show it (without such a device the page says there is no forecast source).
- **Seeding** (`seed_from_ha`, only when the store is empty): one read-only WebSocket session
  with three `recorder/statistics_during_period` requests — `day` statistics into the day tier
  (complete days), `hour` into the quarter tier (older than the 5-minute range), `5minute`
  (HA keeps ~10 days) into the minute tier, which then derives its quarters and days. Only
  entities with statistics (a `state_class`) get rows.

## Simulated house

`sim::house::House`: PV from the solar elevation at the home's coordinates with a drifting
cloud cover; a base load with morning/evening bumps, random kettle spikes and the airco units;
the Solis battery follows `RcMode` (self-consumption, force charge — from the grid only with
*allow grid charge* — and force discharge), drops back to self-consumption after the RC
timeout, respects the SoC limits and the export limit; the SmartEVSE ramps to the override
current; the Kolanky cable does nothing after *Start charging* until a current is set (the start
script sets it 3 s later); P1 sums it all, integrates the import/export registers and keeps the
month's highest quarter-hour average. `sim::scenario::Scenario` steps the model and the engine
on a virtual clock (a simulated day runs in well under a second).

## Deployment

See [`docs/INSTALL.md`](../../docs/INSTALL.md).
