# Plugging in devices

A device type is a **driver kind**: implemented once, registered once, and then used as any
number of **instances** in the config. An instance fills one or more **roles** of the control
logic and exposes **entities**; the dashboard, `/api/entities`, the event stream and the
MQTT bridge pick those up without further code.

```
 [[device]] kind = "homewizard" role = "grid_meter"          config
        │
        ▼  registry (crates/app/src/registry.rs): kind → Factory
 Factory::build(table) → Box<dyn Driver>                     energie-drivers / crates/cloud
        │
        ▼  one tokio task per instance (crates/app/src/drivers.rs)
 Driver::read() → Reading::P1(..)  ──► World.p1 ──► Engine (29 automations, unchanged)
 Driver::entities() ─────────────┐
        │                         ▼
        └──► entity registry (crates/app/src/entities.rs) ──► /api/entities, SSE, MQTT
```

## Roles and slots

The control logic reads a typed `World` (`crates/core/src/world.rs`) and was ported from HA
as-is, so roles are bound to the World section the logic already reads. A **slot** is a role
as the logic consumes it; each slot carries one typed `Reading` variant:

| Slot (`role = …`) | World section | `Reading` | Takes commands | Several? |
| --- | --- | --- | --- | --- |
| `grid_meter` | `p1` | `P1` | | |
| `battery_inverter` | `solis` | `Solis` | `Solis*` | |
| `pv` | `omnik` | `Omnik { power_w }` | | yes, summed |
| `charger:buzz` | `evse` | `Evse` | `Evse*` | |
| `charger:ev3` | `cable` | `Cable` | `Cable*` | |
| `car:buzz` | `buzz` | `Buzz` | | |
| `car:ev3` | `ev3` | `Ev3` | | |
| `load:airco` | `airco` | `Airco` | `Airco` | |
| `load:vriezer` | `vriezer` | `Vriezer` | | |
| `price_feed` | `prices` | `Prices` | `RefreshPrices` | |
| `capacity_peak` | `fluvius` | `Fluvius` | | |
| `solar_forecast` | — | `Empty` (entities only) | | |

So swapping the P1 meter brand means a kind that returns `Reading::P1`; adding a third PV
inverter means another `role = "pv"` instance (the sum goes where the Omnik went: the
AC-coupled PV the logic adds to the hybrid's DC PV, counted only while the sun is up). Neither
touches the logic.

What still needs code, honestly:

- **A third charger or car.** The cascade, the modulator and the handshakes are written for two
  named chargers (SmartEVSE-style for the Buzz, cable-style for the EV3). A third needs a slot,
  a World section and cascade logic that decides between three; a different *brand* of charger
  for one of the two slots only needs a driver that maps its data to `Evse` or `Cable` and
  carries out the same commands.
- **A different battery.** The battery brain speaks the Solis remote-control model (`RcMode`,
  charge/discharge W, timeout). Another inverter can fill `battery_inverter` if its driver maps
  onto that model.
- **The solar forecast** has no World field: the logic does not use one yet. A forecast driver
  exposes entities and its `power_w` forecast (`Source::forecasts`), which the history store
  shows as the dashboard's forecast line; feeding it into a decision is a core change (append a
  field to `World`).
- **More switchable loads** than the airco and the freezer: the peak guard and the airco dump
  only know those two.

## Instances in the config

```toml
[[device]]
id = "pv_garage"          # [a-z0-9_]; driver name `driver:pv_garage`, entity group `pv_garage`
kind = "http_json"
role = "pv"               # a slot, a list of slots, or a bare role when the kind fills one slot of it
url = "http://192.168.1.70/status"
pointer = "/meters/0/power"
```

Common keys: `id`, `kind`, `role`, `control` (the slots whose commands it carries out; needs
`read_only = false`), `read_only` (default `true`), `poll_ms` (default: the kind's), `grace_polls`
(default 3), `name`. Every other key is the kind's own and is checked by it: an unknown key, a
missing key, an unknown kind, a slot the kind cannot fill, two readers of one non-PV slot, two
controllers of one device, a `control` without `read_only = false` all fail `--check-config`
with the device named. `ha` and `sim` (the Home Assistant mirror and the house model) are
kinds too and take `role = "*"` / `control = "*"` for "everything no other device does". A slot
nobody reads comes from HA (the model in simulate mode; without an HA bridge it stays unavailable and `--check-config` says so); a device nobody controls gets the log
actuator. Shadow mode never builds an actuator.

The old `[sources]`, `[actuators]` and `[devices.<name>]` tables still work: they are
translated into the same instances (ids `homewizard`, `solis`, `omnik`, `smartevse`, `cable`,
`prices`, so the driver names and the dashboard stay the same) and `--check-config` prints the
equivalent `[[device]]` tables with a deprecation warning. A file uses one form or the other.

## Adding a driver kind, step by step

The worked example is `crates/drivers/src/http_json.rs` (about 130 lines without the tests): one
power value from any JSON endpoint, as `pv` or `grid_meter`.

1. **The driver.** Implement `energie_drivers::Source` (and `Actuator`; a read-only device answers
   every command with `Applied::NotMine`):

   ```rust
   #[async_trait]
   impl Source for HttpJson {
       fn name(&self) -> &'static str { "http_json" }
       async fn read(&mut self) -> Result<Reading> {
           let w = Some(self.power().await?);
           Ok(match self.slot {
               Slot::GridMeter => Reading::P1(P1 { power_w: w, ..P1::default() }),
               _ => Reading::Omnik(Omnik { power_w: w }),
           })
       }
       fn unavailable(&self) -> Reading { self.slot.unavailable() }
       // Optional: values beyond the typed reading.
       fn entities(&self) -> Vec<Entity> {
           vec![Entity::sensor("raw", "Ruwe waarde", Value::from(self.last_raw))]
       }
   }
   ```

   An actuator may also implement `execute` (as `apply`, reporting its wire writes:
   register / DP / topic, value, the value it replaces) and `plan` (the same writes worked out
   without sending anything: the dry run of control mode, `docs/CONTROL.md`). Without them the
   audit shows the command instead of its writes.

   `read` returns the slot's `Reading` variant (`Slot::unavailable()` names it) and must not
   invent values: an error makes the app keep the last reading for `grace_polls` polls and then
   write `unavailable()`. Commands arrive in `Actuator::apply` as core `Command`s; return
   `Applied::Done`/`Unchanged`/`Scheduled`, or `NotMine` for other devices' commands.

2. **The kind.** Its config table and constructor, in the same file:

   ```rust
   #[derive(Deserialize)]
   #[serde(deny_unknown_fields)]
   pub struct HttpJsonTable { pub url: String, pub pointer: String, #[serde(default = "unit_scale")] pub scale: f64, .. }

   impl DriverKind for HttpJsonKind {
       type Config = HttpJsonTable;
       const NAME: &'static str = "http_json";
       const SLOTS: &'static [Slot] = &[Slot::Pv, Slot::GridMeter];
       const POLL_MS: u64 = 5000;             // WRITABLE defaults to false
       fn check(t: &HttpJsonTable) -> Result<()> { /* no I/O: addresses, ranges */ }
       fn build(t: HttpJsonTable, cx: &BuildCx) -> Result<Box<dyn Driver>> { /* may read secrets, spawn tasks */ }
   }
   ```

   `check` runs at `--check-config` (no network, no secret files); `build` runs at start-up in a
   tokio runtime. `cx.slots` says which slots the config bound (a kind that can fill several
   picks its reading by it); `cx.id` is the instance id. Read secrets with
   `plugin::read_secret`, parse addresses with `plugin::socket`. Set `WRITABLE = true` if
   `apply` does something.

3. **Register it.** One line in `crates/drivers/src/kinds.rs` (`all()`), or in the `kinds()`
   of a crate of its own such as `crates/cloud`, which `Registry::builtin()` in
   `crates/app/src/registry.rs` adds:

   ```rust
   for f in energie_cloud::kinds() { r.add(f) }      // kinds() -> Vec<Box<dyn Factory>>, each factory::<K>()
   ```

4. **Configure it** as a `[[device]]` with its `kind` and `role`. Its typed reading fills the
   role's World section; its fields appear as entities `<id>_<field>` (and `world_<slot>_<field>`
   for the role), its extras as `<id>_<key>`, its health under `drivers` in `/api/state`, and in
   the dashboard's *Apparaten* table.

5. **Test it** against a simulator on `127.0.0.1:0` (see `crates/drivers/src/sim/`; never a real
   device). `fixed.rs` (`kind = "static"`: fixed values for any slot) is the test double for a
   role whose driver does not exist yet.

6. **Settable values the logic has no command for** (optional). An entity from `entities()`
   with a `control` is settable through `POST /api/entities/<id>` while the instance has
   `read_only = false` (outside shadow mode, no `control` binding needed); the value reaches
   `Actuator::set(key, value)`, through the same queue as the commands. Give it the key of one of
   the slot's fields to make that field settable (it replaces the field's entity).

## The cloud kinds (`crates/cloud`)

`crates/cloud/src/kinds.rs` adapts every `CloudSource`/`CloudActuator` client to this contract
(`CloudDriver<C>`), without depending on the app. Credentials files hold the HA config
entry's `data` and are only read; tokens a service rotates go to `state_dir` (refused under
`/etc` already at `--check-config`). The app polls every minute (`POLL_MS = 60_000`); each
client fetches at its own interval and answers from its cache in between.

`kia_connect` and `cupra_data_act` sign in with the account's password by themselves when their
session or refresh token is rejected, through a login gate (one login per 30 min, none after
three rejections until the credentials change). New credentials are set at run time, in every
mode, with `energie credentials set <id>` on the host: the CLI posts them to
`POST /api/cloud/<id>/credentials` (loopback peers only), the device task stores them in
`<state_dir>/credentials/<id>.json` (0600; they win over `credentials_file`) and logs in at once.
A driver kind takes part by implementing `Source::auth` and `Source::set_credentials`
(`energie_drivers::AuthStatus`); the app then gives the device a credentials queue of its own,
the sign-in state in its health and an `auth` entity (`ev3_auth`: `ok`, `aanmelden nodig`,
`geblokkeerd`, `wacht`, `onbekend`). `energie credentials status` and `GET /api/cloud` list
them. Details: `crates/cloud/README.md`.

| Kind | Slot / reading | Writable | Own keys | Extra entities |
| --- | --- | --- | --- | --- |
| `kia_connect` | `car:ev3` / `Ev3` | the `ac_limit` entity | `credentials_file`, `state_dir`, `vehicle`, `poll_interval_s` (≥ 1800), `endpoints` | `ac_limit` (50..100 step 10), `charging`, `dc_limit`, `last_updated` |
| `cupra_data_act` | `car:buzz` / `Buzz` | | `credentials_file`, `poll_interval_s` (≥ 300), `endpoints` | `captured_at` |
| `lg_thinq` | `load:airco` / `Airco` | `Command::Airco` | `credentials_file`, `beneden`, `zolder`, `poll_interval_s` (≥ 60), `base_url` | |
| `fluvius` | `capacity_peak` / `Fluvius` | | `credentials_file`, `state_dir`, `ean`, `poll_interval_s` (≥ 3600), `endpoints` | `peak_start`, `month_MM_peak_kw` (one per calendar month, `Piek YYYY-MM`) |
| `forecast_solar` | `solar_forecast` / `Empty` | | `latitude`, `longitude`, `declination`, `azimuth`, `kwp`, `damping`, `api_key_file`, `base_url`, `poll_interval_s` | `power_now_w`, `energy_today_kwh`, `energy_today_remaining_kwh`, `energy_tomorrow_kwh`, `energy_current_hour_kwh`, `energy_next_hour_kwh` |

```toml
[[device]]
id = "ev3"
kind = "kia_connect"
role = "car:ev3"
credentials_file = "/etc/energie/cloud-kia.json"
state_dir = "/var/lib/energie"     # kia-token.json
read_only = false                  # ev3_ac_limit becomes settable

[[device]]
id = "airco"
kind = "lg_thinq"
role = "load:airco"
control = "load:airco"             # the airco dump switches the units through ThinQ
read_only = false
credentials_file = "/etc/energie/cloud-lgthinq.json"
```

The core has no command for the EV3's AC charging limit, so the logic never sets it; it is a
settable entity only. `forecast_solar` without plane keys is HA's entry for the house.

## Entities

Every value is an `energie_drivers::entity::Entity`:

| Field | |
| --- | --- |
| `id` | `<group>_<key>`, `[a-z0-9_]`, stable |
| `group`, `key` | the MQTT topic is `<group>/<key>`; groups are device ids, `world`, `derived`, `setting`, `memory`, `control` (reserved) |
| `name`, `class`, `unit` | HA-style device class (`power`, `energy`, `battery`, ...) and unit |
| `value` | `null` (unavailable), bool, number or text |
| `control` | when settable: `{"type": "number", "min", "max", "step"}`, `{"type": "select", "options"}` or `{"type": "switch"}` — enough for an HA `number`/`select`/`switch` |
| `publish` | `{enabled, min_interval_s?, deadband?}`, from the `[[publish]]` rules; `Publish::due` decides per value |

| Group | Keys | Settable |
| --- | --- | --- |
| `world` | `<slot>_<field>`: the World as the logic sees it, by role (stable when a brand changes) | the command fields of a role that has a controller (not in shadow mode) |
| `<device id>` | `<field>` of its slot, plus the driver's `entities()` | the driver's entities with a `control`, while `read_only = false` (role commands go through `world`) |
| `derived` | every `Derived` field: template sensors and the meters | no |
| `setting` | every `Settings` field (selects by their label) | while the settings are editable (`settings_source = "local"`, not in shadow lockstep) |
| `memory` | the `Memory` fields HA kept in hidden helpers (latches, counters, measurements) | no |
| `control` | `enabled` (the kill switch), `dry_run`, `<device>` (`evse`, `cable`, `solis`, `airco`, `prices`) | the switches, in control mode |

Every entity that stands for an HA entity names it in `ha_entity` (the World by the HA
bindings, `derived` by the outputs, settings and memory by their helpers, a driver's extras by
`Entity::with_ha`); a driver may also name the HA `state_class` (`Entity::with_state_class`,
for a kWh total `TotalIncreasing`). The MQTT bridge's discovery uses both
(`docs/MQTT.md`).

- `GET /api/entities` — the list with values.
- `POST /api/entities/<id>` `{"value": …}` — a setting becomes a settings change, a World field
  the command HA's entity would get (`world_charger_buzz_override_a` = 14.5 →
  `EvseCurrentDeciA(145)`), routed like the engine's own; `404` unknown, `409` not settable now,
  `400` a value the control does not accept.
- `GET /api/events` — each `state` event carries `entities`: the values that changed since that
  stream's previous event (all of them in the first).

`[[publish]]` rules (glob on the id, later rules win) set what a publisher sends:

```toml
[[publish]]
match = "world_grid_meter_*"   # the 1 s P1 values
min_interval_s = 5
deadband = 10
[[publish]]
match = "derived_kwartier_index"
enabled = false
```

### History of entities

The history store records its curated series (`history::series::SERIES`) and, on top, every
entity picked by a glob in `[history] extra_series`, under the entity id:

```toml
[history]
extra_series = ["fluvius_month_*", "ev3_soc", "airco_*"]
```

A matching entity becomes a series the first time it has a number (a bool counts as 1/0), so a
new device needs no code to be charted: `/api/history?series=ev3_soc&step=300`. Entity series
are kept in the recent, minute and day tiers. A device filling `solar_forecast` that returns a
`power_w` forecast from `Source::forecasts()` (`[(unix s, W)]`) feeds the dashboard's
`pv_forecast_w` line.

The MQTT bridge (`crates/app/src/bridge`, `docs/MQTT.md`) consumes `Snapshot::entities` every
tick (via `Runtime::snapshot_rx`) with `Publish::due` for throttling: a new device's entities
get their topics, `/set` (when settable) and HA discovery without code.
