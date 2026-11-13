# Changelog

This project follows [Semantic Versioning](https://semver.org). While it is at 0.x, a minor
version may include breaking changes.

## 0.5.4 - 2026-11-13

- Tuya chargers: after energie's own stop, a session the charger ends (`charger_end` on the
  `qccdz`, read as `charged`) reads `plugged_in` until the next start or until the charger
  charges again. The logic treats `charged` as a full car and retries it only every 30 minutes,
  which held the EV3 back after energie had paused it for lack of headroom. An end the charger
  reaches by itself still reads `charged`.
- The EV charger simulator ends the session on a switch-off, as the real charger does.

## 0.5.3 - 2026-11-13

- `tuya_evcharger` reads the charger's phase meter: DP 6, eight bytes of voltage, current and
  power that the charger only sends when asked. The driver asks for it every 5 s (`UPDATEDPS`,
  as the Smart Life app does), and while charging the reading is the measured voltage, current
  and power. The set-current estimate is left as the fallback for when DP 6 is not there.
- A Tuya profile can list DPs to refresh (`Profile::REFRESH`, `ChargerConfig::refresh`); the
  Kolanky cable lists none. The simulator models DP 6 and answers `UPDATEDPS`.

## 0.5.2 - 2026-11-13

- `tuya_evcharger`: a power below 0.1 kW while charging counts as not metered too. The EV3's
  charger also reports 1 W while the car draws 1.2 kW, which made the modulator think the car
  had stopped and send a start every 26 s.

## 0.5.1 - 2026-11-13

- `tuya_evcharger`: the EV3's charger turns out not to meter (0 W and no energy while the car
  draws 2.2 kW). While it reports `charger_charging` without power, its set current now stands
  for the current, and the power is that at 230 V, so the EV3 modulator sees the car drawing.
  A charger that does report power is read as measured. Start and current changes were checked
  with the car connected.

## 0.5.0 - 2026-11-13

- New driver kind `tuya_evcharger` for Tuya EV chargers of category `qccdz` (the EV3's new
  charger, which replaced the Kolanky cable), with Tuya local protocol 3.5 (AES-GCM). It fills
  `charger:ev3` and carries out the same start, stop, current and reconnect commands. The charger
  reports no current, so `current_a` is its power over 230 V. Checked against the real charger:
  reads, the session and setting the current. Starting and stopping with a car connected is not
  checked yet; use `dry_run` and a supervised session first.
- `scripts/tuya-local-key.py` fetches a Tuya device's local key through Home Assistant's Tuya
  integration. [docs/HARDWARE.md](docs/HARDWARE.md#finding-the-local-key) describes the whole
  procedure, from scan to credential.
- The Tuya session worker is shared by both chargers; the Kolanky cable behaves as before.
- The Tuya simulator speaks 3.5 too and models the new charger.

## 0.4.2 - 2026-11-10

- Signing out also signs you out at the OIDC provider, so the next sign-in asks for your password
  again. Before, the provider's session stayed and you were signed straight back in. Register
  `https://<energie>/auth/logged-out` as a post-logout redirect URI at the provider.

## 0.4.1 - 2026-11-10

- Cheap grid-charging no longer flip-flops at the top. Once the battery is full in a cheap
  `Max tot piek` window, it holds instead of discharging into the house, and charging restarts
  only below 95 % until the window ends. Before, it charged at 98 % and discharged at 99 %, every
  minute or two.
- The Solis RC mode is selected on every dispatcher run again, as Home Assistant does
  (`rc_keepalive_s` now defaults to 0). The mode-only keepalive let an S6-EH1P fall back to
  self-use every 10 minutes, briefly halving the charge power. It stays available as an opt-in.
- Simulation tests for both: the Modbus inverter simulator keeps the RC timeout as the S6-EH1P
  does, and a closed-loop cheap night checks that a full battery holds instead of cycling.

## 0.4.0 - 2026-11-09

- Optional off-device history in VictoriaMetrics, or any database that accepts InfluxDB line
  protocol (`[history.victoriametrics]`), with the dashboard reading back what the device doesn't
  hold.
- `[history] storage = "memory"` keeps the history off the device's storage.
- `require_state = true` starts with control off when the saved state is gone.

## 0.3.0 - 2026-11-09

- The dashboard is available in English as well as Dutch, following the browser's language, with
  a button to switch.
- Installations without a home battery: leave out the `battery_inverter` device, and the solar
  surplus goes to the cars and the battery controls disappear. See
  `deploy/energie.solar-ev.example.toml`. Charger slots without a device are hidden too.
- The dashboard installs as an app on phones (needs HTTPS). Its service worker caches nothing.
- Drag across the price chart to choose the hours for grid charging.
- `demo = true`, a read-only mode for a public demonstration of the simulator.

## 0.2.0 - 2026-11-09

- `energie credentials` now uses its own loopback-only port (`cli_listen`, default `listen` + 1).
  The main port no longer trusts loopback peers for `/api/cloud`, so a reverse proxy on the same
  machine cannot expose it. Pass `--url` if you used a custom `listen`.
- Multi-architecture container image at `ghcr.io/jeroentrappers/energie`, a Compose setup in
  `deploy/docker`, and `energie --healthcheck` for container health checks.
- Sign in through any OpenID Connect provider (`[oidc]`), for browsers and for API clients with
  bearer tokens, as an alternative to `web_password_file`.

## 0.1.0 - 2026-11-09

First public release.

- Solar-surplus cascade across the home battery and two EV chargers.
- Quarter-hour capacity budget, peak guard and per-phase breaker guard.
- Drivers for HomeWizard P1, Solis Modbus, Omnik, SmartEVSE, Tuya EV cable, generic HTTP JSON.
- Clients for Kia Connect, the VW Group EU Data Act portal, LG ThinQ, Fluvius, forecast.solar
  and day-ahead prices.
- Built-in dashboard, HTTP API, history store and MQTT bridge with Home Assistant discovery.
- `simulate`, `shadow` and `control` modes, dry run, kill switch, audit log and write
  verification.
- Enforced authentication: a password for any dashboard reachable from the network, a login for
  any network MQTT broker, and publish-only MQTT unless `allow_set = true`.
- Configurable site location (`[site]`).
- Static Linux binaries and builds for macOS and Windows.
