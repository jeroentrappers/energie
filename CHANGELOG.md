# Changelog

This project follows [Semantic Versioning](https://semver.org). While it is at 0.x, a minor
version may include breaking changes.

## Unreleased

- The dashboard is available in English as well as Dutch, following the browser's language, with
  a button to switch.
- Installations without a home battery: leave out the `battery_inverter` device, and the solar
  surplus goes to the cars and the battery controls disappear. See
  `deploy/energie.solar-ev.example.toml`. Charger slots without a device are hidden too.
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
