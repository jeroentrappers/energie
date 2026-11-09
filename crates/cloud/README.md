# energie-cloud

Native clients for the cloud integrations that today reach the stack only through Home
Assistant: Kia Connect (EV3), the VW Group EU Data Act portal (ID.Buzz), LG ThinQ Connect (the
two aircos), Fluvius (capacity peak) and forecast.solar. With these the binary can run without
HA. The crate does not depend on `crates/app`; `kinds` makes each client a driver kind of the
plug-in contract of `crates/drivers`, which the app registers (see below).

Protocol facts come from the integrations installed in the live HA on 2026-11-07
(`kia_uvo` 3.21.0 with `hyundai_kia_connect_api` 4.41.3, `cupra_eu_data_act` 0.6.17,
`thinqconnect`, `fluvius` v1.3.0, `forecast_solar`), its config entries (key names and shapes
only) and its entity states.

## Interface

```rust
#[async_trait]
pub trait CloudSource: Send {
    fn name(&self) -> &'static str;
    fn entities(&self) -> Vec<EntityDescriptor>;   // HA entity ids, kind, unit, World field, writable
    async fn read(&mut self) -> Result<CloudReading>;   // cache inside the poll interval, Backoff after errors
    fn unavailable(&self) -> CloudReading;         // HA `unavailable`: every field None
    fn auth(&self) -> Option<AuthStatus> { None }   // the password-login clients: sign-in state
    async fn set_credentials(&mut self, fields: &Map<String, Value>) -> Result<AuthStatus>;   // store + log in
}

#[async_trait]
pub trait CloudActuator: Send {
    async fn apply(&mut self, cmd: &CloudCommand) -> Result<Applied>;
}

pub enum CloudReading { Ev3(Ev3Reading), Buzz(BuzzReading), Airco(AircoReading), Fluvius(FluviusReading), Forecast(SolarForecast) }
pub enum CloudCommand { Ev3AcLimit(u8), Airco { unit: AircoUnit, on: bool } }   // from_command(&Command)
pub enum Applied { Done, Unchanged, NotMine }
```

* `CloudReading::apply_to(&mut World)` fills `world.ev3`, `.buzz`, `.airco` or `.fluvius`;
  the forecast has no `World` slot (display only). `entity_states()` gives the reading as
  `(entity_id, Option<EntityValue>)` pairs.
* `read_or_unavailable(&mut source)` returns the `unavailable` reading on error, with the error.
* Every client also has an unthrottled typed method (`Kia::status`, `Cupra::fetch`,
  `Thinq::states`, `Fluvius::peaks_at`, `ForecastSolar::forecast_at`) used by the probe.
* Rate limits and backoff live in `poll::Poller`: within the interval `read` answers from the
  cache; after a failure it returns `CloudError::Backoff` until the backoff (doubling, capped)
  has passed. Credential errors (`Unauthorized`, `Config`) wait the maximum at once.
* Credentials are JSON files holding the HA config entry's `data` (`store::load_json`, never
  written). Tokens a service rotates go to a state file (`store::StateFile`, 0600, written
  atomically, refused under `/etc`). Passwords and tokens are `store::Secret`, whose `Debug`
  prints `<redacted>`.

## Password logins and captured credentials (`login`)

The Kia and VW clients sign in with the account's password by themselves, so HA is not needed
to re-authenticate:

* **Login gate** (`login::LoginGate`): at most one password login per 30 min, and after three
  rejected logins in a row (`Unauthorized`: wrong password, terms to accept, …) none at all
  until the credentials change. A server fault or a WAF block counts as an attempt, not a
  rejection. The account is never hammered into a lock.
* **Captured credentials** (`login::CredentialStore`): `energie credentials set <device>` posts
  them to the running service, which keeps them in `<state_dir>/credentials/<device-id>.json`
  (0600, atomic, never under `/etc`) and logs in at once (the gate and the backoff start over).
  They win over the `/etc` credentials file from then on. The fields are checked by name
  (`kia_connect`: `username`, `password`, `pin` optional; `cupra_data_act`: `email`,
  `password`, and `vin`, `identifier`, `brand`, `nickname` kept unless given); errors name the
  field, never a value.
* **Sign-in state** (`energie_drivers::AuthStatus`): `ok`, `aanmelden nodig` (it cannot sign in
  by itself), `geblokkeerd`, `wacht` (a login is due after a failure) or `onbekend`, with where
  the credentials come from, the last login and its result, the rejections in a row and the
  next allowed login. The app shows it in the device's health, the dashboard tile, the device's
  `auth` entity (MQTT, HA discovery) and `GET /api/cloud`. It never holds a secret.

## Kia Connect EU (`kia`)

| Field | `Ev3Reading` | HA entity |
| --- | --- | --- |
| `Green.BatteryManagement.BatteryRemain.Ratio` | `soc` | `sensor.ev3_ev_battery_level` |
| `Green.ChargingInformation.ConnectorFastening.State != 0` | `plugged` | `binary_sensor.ev3_ev_battery_plug` |
| `Green.ChargingInformation.Charging.RemainTime > 0` | `charging` | `binary_sensor.ev3_ev_battery_charge` |
| `Green.ChargingInformation.TargetSoC.Standard` / `.Quick` | `ac_limit` / `dc_limit` | `number.ev3_ac/dc_charging_limit` |
| `Drivetrain.Odometer` | `odometer_km` | `sensor.ev3_odometer` |
| `Date` (UTC) | `last_updated` | `sensor.ev3_last_updated_at` |

* Region 1 (Europe), brand 1 (Kia): `https://prd.eu-ccapi.kia.com:8080`. Vehicles from
  `GET /api/v1/spa/vehicles` (picked by nickname or VIN, looked up once); state from
  `GET /api/v1/spa/vehicles/{id}/ccs2/carstatus/latest`, the cached state. No force refresh:
  the car is never woken. Poll interval at least 30 min (HA's `scan_interval`).
* Headers: `Authorization` (the token's `"Bearer …"`), `ccsp-service-id`,
  `ccsp-application-id`, `ccsp-device-id`, `Stamp` (base64 of `"<app id>:<unix time>"` XOR the
  app's key), `Ccuccs2protocolsupport`, `User-Agent: okhttp/3.12.0`.
* Token (`Token.to_dict()` in the entry's `token`): used while `valid_until` is 10 s away;
  then refreshed. Legacy token (the home's: 48-character refresh token, no CCI fields):
  `POST idpconnect-eu.kia.com/auth/api/v2/user/oauth2/token`, form
  `grant_type=refresh_token, refresh_token, client_id=<CCSP service id>, client_secret=secret`;
  a 48-character password is tried as a refresh token too. OneApp/CCI token set:
  `POST cci-api-eu.kia.com/auth/v2/token-refresh` with `X-Fingerprint` (HMAC-SHA256 over the
  OneApp identity, `X-Timestamp` and the whitespace-free body), then
  `POST /auth/v1/token-exchange?serviceType=CCS`. The new token goes to the state file; at
  start the token with the later `valid_until` (state file or credentials) wins.
* `retCode F` / `resCode`: `4002` re-registers the device (`POST /api/v1/spa/notifications/register`)
  and retries once; `7501`, `"Token is expired"` or HTTP 401 refresh and retry once; `5091` is
  `RateLimited`.
* `set_ac_limit(pct)` (`number.ev3_ac_charging_limit`, 50..100 step 10; the `kia_connect` kind's
  settable `ac_limit` entity):
  `POST /api/v1/spa/vehicles/{id}/charge/target` with
  `{"targetSOClist": [{"plugType": 0, "targetSOClevel": dc}, {"plugType": 1, "targetSOClevel": ac}]}`,
  keeping the DC limit from the last reading like HA; `Unchanged` when already set. Tested only
  against the mock. No other command (charging, locks, climate) exists in the client.
* Password login (`Kia::login`, `_login_with_password_cci` of the library): when the refresh is
  rejected (or there is no refresh token) and the credentials hold a plaintext password, through
  the login gate. A fresh device id (`notifications/register`; a random UUID if that fails);
  `GET idpconnect-eu.kia.com/auth/api/v2/user/oauth2/authorize` with the OneApp client
  (`01b36c86-…`, redirect `https://oneapp.kia.com/redirect`, `state=ccsp`; an "abusing" page is
  the WAF, not the password); the RSA key from `GET /auth/api/v1/accounts/certs` (a JWK); `POST
  /auth/account/signin` with the password encrypted (PKCS#1 v1.5, hex, `encryptedPassword=true`,
  `kid`), expecting a `302` with `code`; `POST cci-api-eu.kia.com/auth/v1/token?code=…` (signed
  like the refresh) for the CCI token set; the CCS exchange. Redirects back to the login page,
  to `error_description`, or to `/web/v1/user/authorization` (terms to accept in the app) are
  `Unauthorized` with the reason. The new token set goes to the state file (no credentials in
  it). A 48-character password is a legacy refresh token and is only refreshed with.
  Non-CCS2 (legacy `status/latest`) vehicles are refused.

Fixture: `fixtures/kia_ccs2_status.json`, the EV3's state tree as HA held it on 2026-11-07
(`sensor.ev3_data` → `vehicle_data`) in the API's envelope.

## VW Group EU Data Act (`cupra`)

| Field | `BuzzReading` | HA entity |
| --- | --- | --- |
| `battery_state_report.soc` (flat: `state_of_charge`) | `soc` | `sensor.de_buzz_accu` |
| `settings.target_soc` | `target_soc` | `sensor.de_buzz_laaddoel` |
| `mileage.value` (flat: `mileage`), miles converted | `odometer_km` | `sensor.de_buzz_kilometerstand` |
| newest `*car_captured_time` | `captured_at` | `sensor.de_buzz_laatste_voertuigupdate` |

* Login at `identity.vwgroup.io` as the integration does it since v0.6.45 (latest checked:
  v0.6.53), with a cookie jar: `GET /oidc/v1/authorize` (client id and `state=de__de__<BRAND>`
  per brand; the Buzz is `volkswagen_commercial`). Then either the classic flow (POST the
  identifier form with the email; `hmac`, `_csrf`, `relayState` from the inputs, the
  `templateModel` script object and `csrf_token`; POST the password form) or, when the IdP
  serves Auth0 Universal Login (`/u/login?state=…`, CUPRA/SEAT since 2026-10-26), one POST of
  `username`, `password`, `state` and `action=default` to that same URL.
* The credential POST's redirects are followed by hand (at most 10 hops, each with `Referer`)
  and stop at the portal's `/services/callbacklogin`: the login is the `access_token` cookie it
  sets. Its own redirect to a CMS page (`/de/en/user.html`, which answers 406 since
  2026-10-08 and broke the old follow-everything login) is never fetched. A marketing-consent
  page is skipped (classic `/consent/marketing/…?callback=`: GET the callback, which must stay
  on the portal or the IdP; v2 `…/login/ui/marketing-consent`: POST `relayState`, `state` to
  `…/skip`), the IdP terms page is accepted once (its form: `_csrf`, `relayState`, `hmac`);
  a second terms page, or one without that form, is `Unauthorized` ("log in once at the portal
  in a browser and accept the terms"). The `User-Agent` is `energie/<version>` with browser
  `Accept`/`Accept-Language` on the login hops, and the portal is not primed first (both as VW
  asked the integration). 401/403, or a redirect away from the portal, logs in again once
  through the login gate.
* `GET /proxy_api/euda-apim/datadelivery/vehicles/{vin}/{identifier}/list` (`type: partial`,
  404 = empty); the newest entry with content (`createdOn`, else the timestamp in the name;
  `*_no_content_found.zip` skipped) is downloaded from `…/download` (`filename`, `type`),
  falling back to two older ones on errors. An empty listing re-reads the identifier from
  `…/datarequest/vehicles/{vin}/metadata/partial`.
* Field names come from `dataFieldName` or the data dictionary
  (`data/euda_fields.json`, the 80 dictionary UUIDs of the fields above). Duplicates: usable
  values first (not 65535 / 2³¹−1 / 2³²−1), then the freshest `timestampUtc`, then the last
  in the file. Fields a snapshot lacks keep their previous value.
* Polling every 15 min (the snapshot cadence), at least 5 min, backoff 5 → 60 min. HA polls
  every minute while a snapshot is overdue; this client does not.

## LG ThinQ Connect (`lgthinq`)

* `https://api-eic.lgthinq.com` for BE (region from the SDK's country table). Headers:
  `Authorization: Bearer <PAT>`, `x-country`, `x-client-id` (`connect_client_id`),
  `x-message-id` (22-char URL-safe base64), `x-api-key` (the SDK's public key),
  `x-service-phase: OP`. Body `{"response": …}`, errors `{"error": {"code", "message"}}`
  (`1103`/`1218`/`1302`/`1303` → `Unauthorized`, `1306` → `RateLimited`).
* `GET /devices` once (units by alias, default `Airco beneden` / `Airco zolder`);
  `GET /devices/{id}/state` → `operation.airConOperationMode` `POWER_ON`/`POWER_OFF` →
  `switch.airco_beneden_power` / `switch.airco_zolder_power`.
* `set_power(unit, on)`: `POST /devices/{id}/control`, `x-conditional-control: true`,
  `{"operation": {"airConOperationMode": "POWER_ON"}}`; like HA, `2302` on power-on and `2304`
  on power-off count as done. Skipped (`Unchanged`) when the last poll already shows it.
* The PAT does not rotate; nothing is persisted. Polled every 5 min (at least every minute);
  HA's MQTT push is not used.

## Fluvius (`fluvius`)

* `GET https://mijn.fluvius.be/verbruik/api/meter-measurement-spikes/{ean}` with
  `historyFrom` = 1 January 00:00:00.000 local, `historyUntil` = today 23:59:59.999 local
  (ISO with offset), `asServiceProvider=false`, `meterSerialNumber`. Months
  `{"d", "de", "v": [{"v": kW, "sst", "set"}]}`; the reading is the latest month's peak
  (`sensor.fluvius_meter_<serial>_peak_power`, `world.fluvius.peak_kw`) plus the last 12 months.
* Azure B2C with PKCE: MSAL config (`/api/global/msal/config`), the authorize page's
  `var SETTINGS` / `var SA_FIELDS`, `POST {tenant}/SelfAsserted`, redirects from
  `{tenant}/api/CombinedSigninAndSignup/confirmed` followed by hand to the code,
  `POST {authority}/oauth2/v2.0/token`. Then the refresh token (`invalid_grant` → full login,
  other token errors transient). Tokens in the state file, the integration's
  `{refresh_token, access_token, access_expires_at}` shape. A 401 on the data call gets one new
  token.
* The credentials file is the two config entries as a JSON array; the first electricity entry
  is used (or `ean`). Polling every 6 h by default, at least hourly; backoff 15 min → 6 h.

Fixture: `fixtures/fluvius_spikes.json`, the monthly peaks of HA's `monthly_peaks_kw`
attribute (2026-11-07) in the API's shape; only November's quarter hour is real.

## forecast.solar (`forecast_solar`)

* `GET https://api.forecast.solar/estimate/{lat}/{lon}/{dec}/{az}/{kwp}?time=utc&damping=0`,
  public (12 calls/h per IP), polled hourly like HA without a key. `ForecastSolarConfig::home()`
  is the HA entry: one plane 30° / 180° / 8800 W at the HA location, i.e. API azimuth 0 and
  8.8 kWp (HA subtracts 180 from its azimuth and divides W by 1000).
* `Estimate::at(now)` computes what the HA sensors show, with the library's rules:
  `sensor.power_production_now` (last point at or before now, 0 outside the series),
  `sensor.energy_production_today` / `_tomorrow` (`watt_hours_day`, in the API's timezone),
  `_today_remaining` (periods from now to local midnight), `sensor.energy_current_hour`,
  `sensor.energy_next_hour`; plus the `watts` and `watt_hours_period` series for charts.
* 429 waits for `X-Ratelimit-Retry-At` (else an hour); 401/403 `Unauthorized`;
  400/404/422 `Config`.

Fixture: `fixtures/forecast_solar_estimate.json` (hand-made, values worked out in the tests).

## Driver kinds (`kinds`)

`energie_cloud::kinds()` returns the five kinds; `Registry::builtin` in the app adds them.
`CloudDriver<C>` adapts any `CloudSource + CloudActuator` to `energie_drivers::{Source,
Actuator}`: the `CloudReading` becomes the slot's typed `Reading` (the forecast `Reading::Empty`),
what the World has no field for becomes entities, a core `Command` goes through
`CloudCommand::from_command`, and errors become `DriverError::Cloud` with the client's message.
The read-only clients answer every command with `NotMine`. `Source::auth` and
`Source::set_credentials` pass the password-login clients' sign-in through; the app hands
`BuildCx::state_dir` (its own state dir) for the credentials store.

| Kind | Slot | Keys (besides the common `[[device]]` ones) | Entities beyond the World fields |
| --- | --- | --- | --- |
| `kia_connect` | `car:ev3` | `credentials_file`, `state_dir` (→ `kia-token.json`; captured credentials go to the app's `state_dir`), `vehicle`, `poll_interval_s` (1800), `endpoints` | `ac_limit` (settable 50..100 step 10 with `read_only = false`: `Kia::set_ac_limit`), `charging`, `dc_limit`, `last_updated` |
| `cupra_data_act` | `car:buzz` | `credentials_file`, `poll_interval_s` (900), `endpoints` (captured credentials: the app's `state_dir`) | `captured_at` |
| `lg_thinq` | `load:airco` | `credentials_file`, `beneden`, `zolder`, `poll_interval_s` (300), `base_url` | — (`control = "load:airco"` carries out `Command::Airco`) |
| `fluvius` | `capacity_peak` | `credentials_file`, `state_dir` (→ `fluvius-token.json`), `ean`, `poll_interval_s` (21600), `endpoints` | `peak_start`, `month_MM_peak_kw` per month (name `Piek YYYY-MM`) |
| `forecast_solar` | `solar_forecast` | `latitude`, `longitude`, `declination`, `azimuth` (API convention), `kwp`, `damping` (default: HA's entry), `api_key_file`, `base_url`, `poll_interval_s` (3600) | `power_now_w`, `energy_today_kwh`, `energy_today_remaining_kwh`, `energy_tomorrow_kwh`, `energy_current_hour_kwh`, `energy_next_hour_kwh` |

The app calls `read` every 60 s (`POLL_MS`); within its interval a client answers from its cache,
and while it backs off the read fails, so after `grace_polls` such polls the slot is
`unavailable` until the service answers again. `check` (at `--check-config`) refuses a
`state_dir` under `/etc` and a bad forecast URL without touching files or the network. The
core has no command for the EV3's AC charging limit, so it is only a settable entity: the
control logic never sets it.

## Live check: `energie-cloud-probe`

One read-only fetch, the parsed reading and the entity states printed, never a token:

```sh
cargo build --release --target aarch64-unknown-linux-gnu -p energie-cloud --example energie-cloud-probe
# on the server, as the user that owns the state dir:
energie-cloud-probe kia     /etc/energie/cloud-kia.json --state-dir /var/lib/energie
energie-cloud-probe cupra   /etc/energie/cloud-cupra.json
energie-cloud-probe lgthinq /etc/energie/cloud-lgthinq.json
energie-cloud-probe fluvius /etc/energie/cloud-fluvius.json --state-dir /var/lib/energie
energie-cloud-probe forecast-solar
```

The Kia probe may refresh the access token (it prints whether it did) and saves it to
`<state-dir>/kia-token.json`; the Fluvius probe logs in and saves `fluvius-token.json`.

## Checks

```sh
cargo test -p energie-cloud
cargo llvm-cov -p energie-cloud --text --show-missing-lines   # 100 % lines
cargo clippy -p energie-cloud --all-targets -- -D warnings
```

Every client test runs against a local mock HTTP server (`http::mock`, axum on `127.0.0.1:0`)
with recorded or hand-made payloads; no test touches the network.
