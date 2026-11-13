# energie-drivers

The device layer: one driver per protocol that turns wire traffic into the `World` sub-structs
of `energie-core` and carries out the `Command`s meant for it, plus an in-process simulator per
protocol that speaks the real wire format on `127.0.0.1:0`.

Facts below were taken from the live installation on 2026-11-07: the HA config entries, the
custom components of a live Home Assistant install, one read-only GET per HTTP device, a
passive MQTT subscription, and energy-charts/ENTSO-E GETs. Nothing was written to any device.

## Interface

```rust
#[async_trait]
pub trait Source: Send {
    fn name(&self) -> &'static str;
    async fn read(&mut self) -> Result<Reading>;   // Err = nothing trustworthy was read
    fn unavailable(&self) -> Reading;              // HA `unavailable`: every field None
    fn entities(&self) -> Vec<Entity> { vec![] }   // values beyond the reading (optional)
}

#[async_trait]
pub trait Actuator: Send {
    async fn apply(&mut self, cmd: &Command) -> Result<Applied>;
}

pub enum Reading { P1(P1), Solis(Solis), Omnik(Omnik), Evse(Evse), Cable(Cable), Prices(Prices),
                  Ev3(Ev3), Buzz(Buzz), Airco(Airco), Vriezer(Vriezer), Fluvius(Fluvius), Empty }
pub enum Applied { Done, Unchanged, Scheduled, NotMine }
```

* `Reading::apply_to(&mut World)` writes a reading into its slot.
* A failed read is the caller's choice: keep the last reading, or write `unavailable()`
  (`read_or_unavailable(&mut source)` does the latter and hands back the error).
* Every driver implements both traits; commands for other devices return `Applied::NotMine`.
  `Unchanged` means HA would not have written either (number already at that value, bit
  already set). `Scheduled` is the cable's start sequence.
* Errors are one `DriverError` enum (`thiserror`): `Http`, `Status`, `Unauthorized`, `Io`,
  `Timeout`, `Malformed`, `ModbusException(code)`, `Modbus`, `BadHmac`, `Tuya`, `Mqtt`,
  `NotConnected`, `OutOfRange`, `Config`.

| Driver | Constructor | Kind |
| --- | --- | --- |
| `homewizard::HomeWizard` | `HomeWizard::new(HomeWizardConfig::p1(addr, token, serial))` | poll on `read` |
| `solis::Solis` | `Solis::tcp(SolisConfig::new(addr, slave))` (or `Solis::new(transport)`) | poll on `read` |
| `omnik::Omnik` | `Omnik::new("http://192.168.1.40", timeout)` | poll on `read` |
| `prices::PriceFeed` | `PriceFeed::new(PricesConfig::belgium(entsoe_token))` | cached, fetches when due |
| `smartevse::SmartEvse` | `SmartEvse::spawn(EvseConfig { credentials, ..EvseConfig::new(host, 1883, "SmartEVSE/1234") })` | background MQTT task |
| `tuya::CableHandle` | `CableHandle::spawn(CableConfig::new(addr, device_id, local_key))` | background session worker |

Secrets (tokens, keys, passwords) are only ever passed in through these configs; the tests use
made-up ones.

To be configurable as a `[[device]]`, a driver also has a **kind** (`plugin::DriverKind`: its
config table, the role slots it fills, whether it takes commands, its poll period, a
constructor); `kinds.rs` holds the kinds of the drivers above, `http_json.rs` and `fixed.rs`
(`static`) are two small generic ones, and `entity.rs` is the entity model. See
[`docs/PLUGINS.md`](../../docs/PLUGINS.md).

## HomeWizard P1 — local API v2 (`homewizard`)

`GET https://<meter>/api/measurement`, `Authorization: Bearer <token>`, `X-Api-Version: 2`.
The meter's certificate is issued by HomeWizard's "Appliance Access CA" (an X.509 v1 root,
pinned as `HOMEWIZARD_CA_PEM`, the same PEM the `homewizard_energy` library ships) for the
SAN `<serial>.p1dongle.device.homewizard.energy`. The client connects to the IP but verifies the
chain and that name (`reqwest` with `resolve()` and only that root). Missing/wrong token: `401
{"error":"user:unauthorized"}` → `Unauthorized`. Polled; the v2 websocket push is not used.

| Field (`/api/measurement`) | `world::P1` | HA entity |
| --- | --- | --- |
| `power_w` | `power_w` | `sensor.p1_meter_vermogen` |
| `current_l1_a` .. `current_l3_a` (signed) | `phase_a` | `sensor.p1_meter_stroom_fase_1..3` |
| `energy_import_kwh` | `import_kwh` | `sensor.p1_meter_energie_import` |
| `energy_export_kwh` | `export_kwh` | `sensor.p1_meter_energie_export` |
| `monthly_power_peak_w` | `month_peak_w` | `sensor.p1_meter_piekverbruik_huidige_maand` |

Fixture: `fixtures/homewizard_measurement.json` (HWE-P1, firmware 6.0503, API 2.3.0). The
example `probe_homewizard` does one read-only GET against a real meter (`HW_TOKEN` env).

## Solis S6-EH1P — Modbus TCP via Waveshare gateway (`solis_modbus`)

Gateway `192.168.1.20:502`, unit 1. Register numbers are the Modbus addresses; 3xxxx are input
registers (FC 4), 4xxxx holding registers (FC 3, written with FC 6). Source:
`custom_components/solis_modbus/sensor_data/{hybrid,switch,select}_sensors.py` and
`sensors/solis_{derived_sensor,number_sensor,select_entity,binary_sensor}.py`.

| Entity (`_2` suffix) | Register | Encoding |
| --- | --- | --- |
| `sensor.solis_s6_eh1p_total_pv_power` | in 33057-33058 | S32 high word first, W |
| `sensor.…_battery_soc` | in 33139 | %, spike filter: 0/100 only after 3 polls in a row |
| `sensor.…_battery_power_net` | in 33149-33150, in 33135 | S32 W; 33135 = 0 (charging) → negative |
| `select.…_rc_force_charge_discharge` | hold 43135 | 0 `None`, 1 `Solis RC Force Battery Charge`, 2 `…Discharge`; other → `None` reading |
| `number.…_rc_force_battery_discharge_power` | hold 43129 | ×10 W |
| `number.…_rc_force_battery_charge_power` | hold 43136 | ×10 W |
| `number.…_rc_timeout` | hold 43282 | min, 1..30 |
| `switch.…_grid_feed_in_power_limit_switch` | hold 43073 bit 4 | |
| `number.…_backflow_power` | hold 43074 | ×100 W, 0..20000 |
| `switch.…_allow_grid_to_charge_the_battery` | hold 43110 bit 5 | set = allowed |

Reads, paced like HA's polling (fast 15 s / slow 300 s): input 33057×2 and 33133×18 on every
poll (15 s); holding 43073×2, 43110×1, 43129×8, 43282×1 (and the extra 43011×14, 43483×1,
43487×2) every `settings_every` polls (4: 60 s) and on the poll after a write; the extra input
33029×7 and 33161×16 every fifth settings read (300 s). A failed core block fails the read; a
failed extra block only blanks its entities (`solis::extra`: lifetime and daily energies,
battery voltage/current/SOH, SoC limits, work mode, peak shaving, with their HA entity ids).
Writes, as the component does them:

* RC mode select: write 43135, then re-write 43136, 43129, 43282 from the register cache
  (`companion_writes`, so setpoints and timeout latch); a power companion with no cached value is
  skipped; a timeout that is not cached or outside 1..30 is written as `rc_timeout_fallback_min`
  (10; HA would skip it). By default every command selects, as HA. Opt-in `rc_keepalive_s > 0`: a
  mode the inverter already holds (with a valid timeout) is not selected again, `None` writes
  nothing, a forced mode only re-writes 43135 as its keepalive, at most every `rc_keepalive_s` and
  at least every half RC timeout. The house's S6-EH1P ignores that keepalive and drops to
  self-use (`docs/CONTROL.md`).
* Numbers: skipped (`Unchanged`) when the cached value already equals the request; otherwise
  `round(value / step)` with Python's ties-to-even; outside the entity's min/max → `OutOfRange`.
* Switches: read-modify-write of one bit, starting from the cache or a live read (never from 0,
  issue #402); written only if the register changes.
* `SolisReconnect` drops the TCP session.

`execute` reports each register write with the cached value it replaced; `plan` (control mode's
dry run) works out the same writes and writes nothing.

Transport: `TcpModbus` (tokio-modbus) connects lazily, keeps ≥ `frame_gap` (310 ms, HA's
`MIN_INTER_FRAME_MS`) between frames, drops the session on a transport error or timeout (5 s)
and retries once on a reused connection. Exception responses become `ModbusException(code)` and
keep the session. The `ModbusTransport` trait lets tests inject a fake transport. Every value
against HA's integration, with source lines: `docs/CONTROL.md`.

## SmartEVSE v3 — MQTT (`SmartEVSE/1234`)

Discovered by subscribing (read-only) to `SmartEVSE/1234/#` and `homeassistant/#` on the home
broker for 70 s. Firmware v3.11.2. State topics are published retained, re-sent about every
11 s; availability `SmartEVSE/1234/connected` = `online` (will `offline`).

| Topic | `world::Evse` | Encoding (HA discovery template) |
| --- | --- | --- |
| `EVPlugState` | `plug` | `Connected` / `Disconnected` |
| `State` | `state` | `Charging`, ... |
| `StateID` | `state_id` | `A`..`F` |
| `Mode` | `mode` | `Off` `Normal` `Smart` `Solar` `Pause` |
| `ChargeCurrentOverride` | `override_a` | `value \| int / 10 if value \| is_number else none` |
| `homeassistant/number/SmartEVSE/1234-ChargeCurrentOverride/config` `max` | `override_max_a` | `"29"`; falls back to `MaxCurrent / 10` |
| `ChargeCurrent` | `charge_current_a` | `value \| int / 10` |

Commands: `EvseCurrentDeciA(n)` → `SmartEVSE/1234/Set/CurrentOverride` payload `n` (tenths, as
the automations publish); `EvseMode(m)` → `SmartEVSE/1234/Set/Mode` payload `Normal` etc. (the
select's `command_topic`). QoS 0, not retained. The read is `NotConnected` while the broker link
is down or the charger is not `online`; commands are refused (not queued) while the link is down.
The rumqttc event loop runs in a task that re-subscribes after every reconnect.

## Tuya EV charger (`qccdz`) — Tuya local protocol 3.5

`tuya::evcharger`, kind `tuya_evcharger`: the EV3's charger (product `dsmsam7xpb3ht7rl`). The
session worker (`tuya::charger`) is the cable's, generic over a `Profile` (DP mapping and
command DPs); the protocol version is a setting of the session (`tuya::wire`).

Protocol 3.5 (`tuya::v35`): frame `00006699 0000 seq cmd len IV(12) ciphertext tag(16) 00009966`,
AES-128-GCM with the 14 header bytes after the prefix as additional data and a fresh random IV
per frame. Device frames carry the 4-byte return code at the start of the plaintext. The
handshake is 3.4's with these frames under the local key; the session key is the GCM encryption
of `nonce ⊕ nonce` under the local key with the client nonce's first 12 bytes as IV, tag dropped.
Known-answer tests against tinytuya 1.21.

| DP | Code | `world::Cable` | Encoding |
| --- | --- | --- | --- |
| 3 | `work_state` | `status` | `charger_free` available; `charger_insert`, `charger_wait`, `charger_pause` plugged_in; `charger_charging` charging; `charger_end` charged; `charger_fault`, `charger_free_fault` fault |
| 6 | phase meter (no cloud code) | `voltage_v`, `current_a`, `power_kw` while `charger_charging` | base64 of 8 bytes: V ×10 (u16), mA (u24), W (u24); only sent on `UPDATEDPS` (0x12, `{"dpId":[6]}`), requested every 5 s |
| 9 | `power_total` | without DP 6: `power_kw`, `current_a` (W / 230); below 0.1 kW while charging, the set current at 230 V | kW ×1000, in practice whole kW |
| 4 | `charge_cur_set` | `set_current_a` | A; written 6..16 |
| — | | `set_current_max_a` | 16 |
| 18 | `switch` | (start / stop) | bool |
| 14 | `work_mode` | (a start selects `charge_now`) | `charge_now`, `charge_pct`, `charge_energy`, `charge_schedule` |

Extra entities: total energy (1, kWh ×100), session energy (25, kWh ×100), phase power (5, kW
×1000), temperature (24), mode (14), switch (18), raw state (3), fault bitfield (10), firmware
(23), with the Tuya cloud integration's entity ids where it has one (`HA_ENTITIES`).

## Kolanky EV cable — Tuya local protocol 3.4 (`tuya_local`, `kolanky_evcharger`)

`192.168.1.60:6668`. Frame: `000055AA seq cmd len [retcode] payload HMAC-SHA256 0000AA55`;
AES-128-ECB with PKCS#7; device frames carry a return code (detected as `body % 16 == 4`);
control payloads carry the `3.4` + 12 zero bytes version header. Handshake: `SESS_KEY_NEG_START`
(client nonce) → `SESS_KEY_NEG_RESP` (device nonce + HMAC of ours, checked) →
`SESS_KEY_NEG_FINISH` (HMAC of theirs); session key = `AES-ECB(local_key, nonce ⊕ nonce)`.
Then `DP_QUERY_NEW` (`{}`), `CONTROL_NEW` (`{"protocol":5,"t":..,"data":{"dps":{..}}}`), `STATUS`
pushes, `HEART_BEAT`. Source for the DPs: `devices/kolanky_evcharger.yaml`.

| DP | `world::Cable` | Encoding |
| --- | --- | --- |
| 101 | `status` | `no_connet` available, `connect` plugged_in, `charing` charging, `wait_rfid` waiting_for_authorization, `finish` charged, `wait_charing` waiting, `error` fault |
| 105 | `current_a` | A ×10 |
| 102 | `voltage_v` | V |
| 109 | `power_kw` | kW ×10 |
| 114 (115 if DP 113 = `Max32A`) | `set_current_a` | A |
| 113 | `set_current_max_a` | 16, or 32 with `Max32A` |
| 124 | (select) | `OpenCharging` Start charging, `CloseCharging` Stop charging, `WaitOperation` No operation |

Extra entities from the DPs already reported: temperature (110), session energy (112),
charging time (120), the operation select (124), the raw status (101), the fault bitfield (10),
delay and duration (118, 119), with their HA entity ids (`tuya::cable::HA_ENTITIES`). `execute` and
`plan` report the DP sets with the DPs they replace.

Commands: `CableStart{amps}` = `124 OpenCharging`, then after `start_delay` (3 s) the current on
DP 114 — a new start during the wait restarts the sequence (`mode: restart`); `CableStop` =
`124 CloseCharging` (does not cancel a pending start, as the HA stop script does not);
`CableSetCurrent` on DP 114/115 within 6..16 (8..32) A, else `OutOfRange`; `CableReconnect`
drops and renegotiates the session (`start_delay_ms`, 3000). A bad current at the end of a start is dropped like the HA
script's failing `number.set_value`. The worker sends a heartbeat every 10 s, treats 3 silent
periods, a read error or a bad HMAC as a lost link, and reconnects after `reconnect_delay`;
while disconnected `read` is `NotConnected` and commands fail fast.

## Omnik — `js/status.js` (`omnik_inverter`, source `javascript`)

`GET http://192.168.1.40/js/status.js` (`application/x-javascript`), parsed like
`omnikinverter.models.Inverter.from_js`: `webData="serial,fw,fw_slave,model,rated W,current W,
today ×10 Wh,total ×100 Wh,alarm,…";` (or `myDeviceArray[0]=`). Field 5 is
`sensor.omnik_hoofddak_inverter_omnik_hoofddak_current_power_production`; an empty field is
`None`, a missing one or a non-integer is an error; the content type must be JSON, JS or HTML.
Fixture: `fixtures/omnik_status.js`.

## Prices — energy-charts, ENTSO-E fallback (`price_backup.py`, `entsoe`, `entsoe_qh`)

* energy-charts `GET /price?bzn=BE&start=<today>&end=<tomorrow>` (end inclusive: both days once
  tomorrow is out). Fixture `fixtures/energy_charts_be_2026-11-07.json`.
* ENTSO-E `GET /api?documentType=A44&contract_MarketAgreement.type=A01&in_Domain=10YBE----------2&out_Domain=…&periodStart&periodEnd&securityToken`,
  local midnight to +2 days. A03 gaps repeat the previous price up to the last point (as the
  integration's `process_points`); PT60M is split into quarters; other resolutions and
  duplicate periods are skipped; an `Acknowledgement_MarketDocument` is an error. Fixture
  `fixtures/entsoe_a44_be_2026-11-07.xml` decodes to the same series as the energy-charts one.

| `world::Prices` | How |
| --- | --- |
| `afname_now`, `backup_now` | offtake tariff of the slot containing now: `round((raw/1000 × 1.02 + 0.1215031) × 1.06, 5)` |
| `afname_slots` | every slot from today 00:00 (today + tomorrow) with the offtake tariff |
| `afname_has_tomorrow`, `injectie_has_tomorrow` | any slot on tomorrow's date |
| `injectie_now` | `round(raw/1000 × 0.98 − 0.015, 5)` |
| `epex_now` | `raw / 1000` to 4 decimals, half up (`entsoe_qh`) |
| `afname_pct_in_range` | `entsoe` integration (`calculation_mode: publish`): `round((now − min)/(max − min) × 100, 1)` over the slots above, 100 if flat. `price_backup.py`'s `positie` uses the same window but answers 0 when flat. |

`read` fetches when due: no cache, a new day, no slot for now, an hour since the last fetch,
every 15 min after 13:00 while tomorrow is missing, or after `RefreshPrices` (which also fetches
immediately). A failed fetch keeps serving the cache while it has a slot for now. Checked
against HA on 2026-11-07 13:47: 0.37692 offtake, 0.2099 injection, 18.6 % (with tomorrow).

## Simulators (`sim`)

| Simulator | Protocol | Knobs |
| --- | --- | --- |
| `sim::homewizard::HomeWizardSim` | HTTPS (throwaway CA from `rcgen`), bearer check | `set_measurement`, `set_status`, `set_raw_body`, `last_api_version`, `config(token)` |
| `sim::solis::SolisSim` | Modbus TCP server with the register map, RC model | `set_input/holding`, `remove_holding`, `set_battery`, `set_pv`, `writes`, `fail_next(exception)`, `set_stall` |
| `sim::mqtt::Broker` + `SmartEvseSim` | embedded `rumqttd` broker; charger client with will | `set(topic, payload)`, `get`, `received`, `kill` |
| `sim::tuya::TuyaSim` | Kolanky cable (3.4, `start`) or `qccdz` charger (3.5, `start_evcharger`), one connection at a time | `set_dp` (pushes), `dps`, `sets`, `handshakes`, `refreshes`, `drop_connection`, `answer_heartbeats`, `corrupt_next_frame`, `set_query_reply` |
| `sim::omnik::OmnikSim` | HTTP `js/status.js` | `set_power`, `set_body`, `set_status`, `set_content_type` |
| `sim::prices::PriceSim` | energy-charts JSON + ENTSO-E XML from one per-day table | `set_day`, `remove_day`, `set_energy_charts_status`, `set_entsoe_status`, `hits` |
| `sim::proxy::TcpProxy` | TCP pass-through | `drop_all` (cut every connection) |

Every driver test runs the driver against its simulator end to end.

## Checks

```sh
cargo test -p energie-drivers
cargo llvm-cov -p energie-drivers --text --show-missing-lines   # 100 % lines
cargo clippy -p energie-drivers --all-targets
```

## Not exercised against real hardware

`docs/CONTROL.md` lists what the dry run must check on the hardware. Only the HomeWizard client
was run against the real device (one GET). The Solis driver never
talked to the gateway (a second Modbus master would collide with HA's polling), the cable
driver never to the cable (it takes one local client and would knock HA off), and the MQTT
driver never published; their wire formats come from the component sources and the passive
capture, and are exercised only against the simulators. The Omnik and price parsers ran on
real captured responses.
