# Supported hardware and services

energie has so far been used in one installation, so other firmware versions, models and
regions are untested. Please report what works for you.

## Local devices

| Device | `kind` | Role | Reads | Writes | Needs |
| --- | --- | --- | --- | --- | --- |
| HomeWizard P1 meter (API v2) | `homewizard` | `grid_meter` | yes | | IP, serial, token |
| Solis hybrid S6-EH1P via Modbus TCP gateway | `solis_modbus` | `battery_inverter` | yes | yes | IP and port of the gateway, Modbus unit |
| Omnik string inverter | `omnik` | `pv` | yes | | Base URL |
| SmartEVSE v3 over MQTT | `smartevse_mqtt` | `charger:buzz` | yes | yes | Broker, topic prefix |
| Tuya EV charger, category `qccdz` (Tuya local 3.5) | `tuya_evcharger` | `charger:ev3` | yes | yes | IP, device id, local key |
| Kolanky EV cable (Tuya local 3.4) | `tuya_cable` | `charger:ev3` | yes | yes | IP, device id, local key |
| Any HTTP JSON endpoint | `http_json` | `pv` and others | yes | | URL, JSON pointer |
| Fixed values | `static` | any | yes | | The values, as a stand-in |

### HomeWizard P1

Enable the local API in the HomeWizard app, then create a token following
[HomeWizard's API documentation](https://api-documentation.homewizard.com/). `serial` is the
device's serial number, used to verify its TLS certificate.

### Solis

energie uses the remote-control registers. It must be the **only** Modbus master, so disable
other integrations first. The pacing (310 ms between frames) suits a typical Modbus-TCP gateway.
The registers and their behaviour are listed in [`CONTROL.md`](CONTROL.md).

Other Solis models may use different registers. Test with `dry_run` first.

### SmartEVSE

Needs MQTT enabled on the charger and a broker. `prefix` is the charger's topic prefix,
`SmartEVSE/<serial>`. energie and any other controller must not both steer it: two controllers
can latch a fault on the charger.

If energie stops, the charger **keeps its last current setting**, as it would with any
external controller using its Normal mode.

### Tuya EV chargers

Get the device id, local key and protocol version with a tool such as
[tinytuya](https://github.com/jasonacox/tinytuya) (`tinytuya scan` shows the address, id and
version; `tinytuya wizard` fetches the local key from a Tuya IoT project). The local key changes
when the device is paired again. These devices accept a single local client, so other *local*
integrations (`tuya_local`, LocalTuya) must be disabled; the Tuya cloud integration can stay.

`tuya_evcharger` speaks the standard DPs of Tuya's charging-pile category (`qccdz`: `work_state`
on DP 3, `charge_cur_set` on 4, `power_total` on 9, `work_mode` on 14, `switch` on 18). It was
written for product `dsmsam7xpb3ht7rl` (firmware 2.0.0); another `qccdz` charger may number its
DPs differently, so compare its DP list (Tuya IoT console, *Device debugging*) with the table in
`crates/drivers/README.md` first. The charger reports no current: `current_a` is its power over
230 V. `tuya_cable` is the Kolanky cable (`tuya_local` `kolanky_evcharger`).

## Cloud services

| Service | `kind` | Role | Credentials file (JSON) | Notes |
| --- | --- | --- | --- | --- |
| Day-ahead prices (energy-charts, ENTSO-E fallback) | `energy_charts` | `price_feed` | Optional `entsoe_token_file` | No account needed for the primary source |
| forecast.solar | `forecast_solar` | `solar_forecast` | Optional `api_key_file` | Set `declination`, `azimuth`, `kwp` for your panels. The defaults are placeholders. |
| Kia Connect (EU) | `kia_connect` | `car:ev3` | `username`, `password`, `pin`, `brand` | Unofficial API. Reads cached state only, never wakes the car. |
| VW Group EU Data Act portal | `cupra_data_act` | `car:buzz` | `brand`, `email`, `password`, `vin` | Accept the portal's terms in a browser first. |
| LG ThinQ Connect | `lg_thinq` | `load:airco` | `access_token`, `connect_client_id`, `country` | Personal access token from LG's developer site. `beneden` and `zolder` name the two units. |
| Mijn Fluvius (Belgium) | `fluvius` | `capacity_peak` | `email`, `password`, `ean` | Unofficial API. See [`crates/cloud/README.md`](../crates/cloud/README.md) for the file layout. |

Protect credential files as described in the
[installation guide](INSTALL.md#2-store-secrets).

To replace Kia or VW credentials while the service runs:

```sh
sudo energie credentials set <device-id>
energie credentials status
```

Logins back off automatically, and stop after three rejections so that an account isn't locked.

## Adding a device

A different brand can fill an existing role by writing a driver that maps to the same reading
type. See [`PLUGINS.md`](PLUGINS.md), including what still needs changes to the control logic.
