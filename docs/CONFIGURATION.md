# Configuration

energie reads one TOML file, given with `--config`. Unknown keys are errors, so a typo won't
pass silently. Always validate after a change:

```sh
energie --check-config /etc/energie/energie.toml
```

Two annotated samples are in [`deploy/`](../deploy/):
`energie.standalone.example.toml` and `energie.example.toml`. Secrets are never written in this
file: every secret is the path of a file that holds it.

## Top-level keys

These must appear before the first `[section]`.

| Key | Default | Description |
| --- | --- | --- |
| `mode` | required | `simulate`, `shadow` or `control`. See [modes](OPERATIONS.md#modes). |
| `listen` | `127.0.0.1:8080` | Address of the dashboard and API. |
| `web_password_file` | none | File holding the dashboard password (HTTP Basic, any user name). Strongly recommended if `listen` is not loopback. |
| `state_dir` | `/var/lib/energie` | Settings, memory, logs and history. |
| `tick_ms` | `1000` | How often the control logic runs. |
| `persist_interval_s` | `60` | The most often that state is written to disk. |
| `settings_source` | `ha` in shadow, otherwise `local` | Who owns settings: Home Assistant helpers or the state directory. |

## `[site]`

```toml
[site]
latitude = 50.85
longitude = 4.35
```

Used for the sun position and as the default location of the solar forecast. Without it energie
calculates for central Brussels and warns at start in control mode.

Local time is currently always Europe/Brussels, whatever `[site]` says.

## `[[device]]`

One table per device. `kind` selects the driver, and `role` the slot of the control logic it
fills.

```toml
[[device]]
id = "p1"                 # a-z, 0-9, _; also the prefix of its entities
kind = "homewizard"
role = "grid_meter"
address = "192.168.1.30"
serial = "aabbccddeeff"
token_file = "/run/credentials/energie.service/homewizard-token"
```

| Common key | Default | Description |
| --- | --- | --- |
| `id` | required | Unique name. |
| `kind` | required | The driver. See [supported hardware](HARDWARE.md). |
| `role` | required | `grid_meter`, `battery_inverter`, `pv` (several are summed), `charger:buzz`, `charger:ev3`, `car:buzz`, `car:ev3`, `load:airco`, `load:vriezer`, `price_feed`, `capacity_peak`, `solar_forecast`, or `*` for the `ha` kind. |
| `control` | none | The slots whose commands this device carries out. Requires `read_only = false`. |
| `read_only` | `true` | Safety default: a device is never written to unless you set this to `false`. |
| `poll_ms` | per kind | Time between reads. |
| `grace_polls` | `3` | Failed reads that keep the last value before the device counts as unavailable. |
| `name` | none | Display name. |

Other keys depend on the kind: see [supported hardware](HARDWARE.md) and
[`PLUGINS.md`](PLUGINS.md). Addresses are IP addresses, as `"ip"` or `"ip:port"`.

A slot no device fills comes from Home Assistant if you configured one, from the house model in
simulate mode, and otherwise stays unavailable. `--check-config` lists these.

## `[control]`

Applies in control mode.

| Key | Default | Description |
| --- | --- | --- |
| `dry_run` | `false` | Plan and log each write, send none. |
| `enabled` | `true` | Master switch at the **first** start. Afterwards `control.json` in the state directory wins. |
| `devices` | all on | Per-device enable at the first start, e.g. `{ solis = false }`. |
| `verify_s` | `60` | How long a write has to show up in a device's readings. |
| `max_commands_per_min` | `30` | Rate limit per device, against runaway loops. |
| `startup_wait_s` | `60` | How long the first decision waits for first readings. |
| `release_on_shutdown` | `["solis"]` | What to release at a clean stop: `solis` (RC mode off), `evse` (override 0), `cable` (stop). |
| `audit_file` | `<state_dir>/control-audit.jsonl` | Log of every real write. |
| `dry_run_file` | `<state_dir>/control-dryrun.jsonl` | Log of planned writes. |
| `log_max_bytes` | `4194304` | Size at which those logs rotate. |
| `alarms` | `true` | Send a notification when a write fails. |
| `alarm_interval_s` | `1800` | The most often one alarm repeats. |

## Notifications

```toml
[notify]
kind = "ntfy"                        # or "log", the default
url = "https://ntfy.sh/a-long-random-topic-name"
token_file = "/run/credentials/energie.service/ntfy-token"   # optional
```

Anyone who knows a public ntfy topic can read it, so use a long random name or your own server.

## `[mqtt_publish]`

The MQTT bridge, off by default. Details are in [`MQTT.md`](MQTT.md).

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `false` | Turn the bridge on. |
| `broker`, `port` | none, `1883` | The broker. |
| `username`, `password_file` | none | Broker login. |
| `client_id` | `energie` | MQTT client id. |
| `prefix` | `energie` | Topic prefix. |
| `discovery` | on outside shadow | Home Assistant discovery. |
| `discovery_prefix` | `homeassistant` | Discovery topic prefix. |
| `include`, `exclude` | `["*"]`, `[]` | Globs on entity ids. |
| `summary_interval_s` | `5` | How often `<prefix>/summary` is sent. |

`[[publish]]` tables tune individual entities. Later rules win:

```toml
[[publish]]
match = "p1_*"
min_interval_s = 5
deadband = 10
```

## `[history]`

| Key | Default | Description |
| --- | --- | --- |
| `enabled` | `true` | Record the dashboard charts. |
| `dir` | `<state_dir>/history` | Where to store it. |
| `minute_days` | `14` | Retention of 1-minute data. |
| `quarter_days` | `430` | Retention of quarter-hours, 13+ months for the capacity tariff. |
| `day_days` | `1100` | Retention of daily totals. |
| `seed_from_ha` | `false` | Fill an empty store from Home Assistant's statistics at start. |
| `extra_series` | `[]` | Globs of additional entities to record. |

At the defaults this takes about 10–15 MB of disk and 18 MB of memory.

## `[ha]`

Connection to Home Assistant, for shadow mode or a one-time import.

| Key | Default | Description |
| --- | --- | --- |
| `url` | required | `http://host:8123` or `ws://…`. TLS is not supported. |
| `token_file` | required | File holding a long-lived access token. |
| `seed` | `true` | Seed settings and memory from HA helpers when the state directory is empty. |
| `start_wait_s` | `30` | How long to wait for HA's first snapshot. |
| `import_only` | `false` | Use HA only for the first-start import, then never again. |

## `[shadow]`

| Key | Default | Description |
| --- | --- | --- |
| `lockstep` | `true` | Adopt HA's settings and memory each tick, so only the decision logic is compared. |
| `time_tolerance_s` | `5` | A derived value matches if HA showed it within ± this. |
| `command_window_s` | `40` | A command matches an HA action within ± this. |
| `log_file` | `<state_dir>/shadow-mismatches.jsonl` | Log of mismatch onsets. |
| `log_max_bytes` | `262144` | Size at which that log rotates. |

`[shadow.tolerances]` overrides per field, for example `batterij_setpoint_w = { abs = 25 }`.

`[mqtt]` optionally subscribes to a charger's set topics so that shadow mode can see HA's MQTT
commands. It never publishes: `host`, `port`, `username`, `password_file`, `topic`, `client_id`.

## `[sim]`

The house model for simulate mode: `seed`, `speed`, `start`, `solis_pv_peak_w`,
`omnik_pv_peak_w`, `cloudiness`, `battery_kwh`, `battery_soc`, `buzz_plugged`, `buzz_soc`,
`ev3_plugged`, `ev3_soc`.

## Control settings

Thresholds, priorities and the charging levels aren't in this file. Change them on the
dashboard, with `POST /api/settings`, or over MQTT. They persist in the state directory.
[`spec/LOGIC.md`](../spec/LOGIC.md) explains what each one does.

## Logging

Set `RUST_LOG`, for example `RUST_LOG=debug` or `RUST_LOG=info,energie=debug`, in the
environment. Logs go to stderr, and so to the journal under systemd.
