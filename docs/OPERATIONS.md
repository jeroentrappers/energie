# Operating energie

## Modes

| `mode` | Reads from | Writes to devices | Use it to |
| --- | --- | --- | --- |
| `simulate` | Built-in house model | Nothing | Explore, develop, demonstrate |
| `shadow` | Home Assistant and any configured devices | Nothing | Compare decisions with an existing HA setup |
| `control` with `dry_run = true` | The devices | Nothing, plans are logged | Verify the writes before going live |
| `control` | The devices | Yes | Run the house |

## The dashboard

Open the `listen` address in a browser. The interface is in Dutch: *Overzicht* (overview),
*Capaciteitstarief* (capacity tariff), *Prijzen* (prices), *Batterij & Solis*, *EV laden* (EV
charging) and *Systeem* (system health, commands and device states).

## The kill switch

Any of these immediately stops all writes to devices. Queued commands are dropped.

- Dashboard: *Systeem → Sturing*.
- API:

  ```sh
  curl -u any:PASSWORD -X POST http://HOST:8080/api/control \
    -H 'Content-Type: application/json' -d '{"enabled": false}'
  ```

- MQTT or Home Assistant: the `control_enabled` entity.

The stop is saved to `control.json`, so it survives restarts. Re-enable it the same way with
`true`. To stop one device, send `{"device": "solis", "enabled": false}`. Devices are `solis`,
`evse`, `cable`, `airco` and `prices`.

## What happens when energie stops

| Device | Clean stop | Crash, power cut or lost network |
| --- | --- | --- |
| Solis battery | Returns to self-use at once (`release_on_shutdown`) | Returns to self-use by itself within the RC timeout, 10 minutes at most |
| SmartEVSE | Keeps its last setting, unless `evse` is in `release_on_shutdown` | **Keeps charging at its last current** |
| Tuya cable | Keeps its last setting, unless `cable` is in `release_on_shutdown` | **Keeps its last state** |
| Aircos | Keep their state | Keep their state |

Because the chargers don't fall back on their own, make sure that a stuck charger is safe in
your installation: your breakers and the charger's own maximum current must be right without
energie. The breaker guard is software, and doesn't run when energie doesn't.

On restart, energie serves the dashboard immediately and starts deciding after the first
readings arrive, normally within seconds. A device with no reading yet gets no commands.

## Monitoring

| What | How |
| --- | --- |
| Liveness | `GET /healthz`: `200` while running, `503` if starting or stalled. Needs no password. |
| Device health | `GET /api/state`, under `drivers`: `available`, `consecutive_errors`, `last_error`. |
| Writes | `GET /api/control`, or `<state_dir>/control-audit.jsonl`. |
| Logs | `journalctl -u energie` |
| Alarms | Failed writes send a notification when [`[notify]`](CONFIGURATION.md#notifications) is set. |

Each audited write ends as `verified`, `unchanged`, `sent`, `superseded`, `failed` or `error`.
A `failed` write means that the device did not show the expected value in time.

## Backups

Everything is in `state_dir`, which under the supplied unit is `/var/lib/private/energie`. Also
back up `/etc/energie`. Settings exist **only** in the state directory, so never empty it on a
running system without a copy.

```sh
sudo tar czf energie-backup.tar.gz /var/lib/private/energie /etc/energie
```

The archive holds credentials, so store it accordingly.

| File | Contents |
| --- | --- |
| `settings.json` | Your settings |
| `memory.json` | Latches, counters and meters |
| `control.json` | Kill-switch state |
| `control-audit.jsonl` | Log of writes |
| `history/` | Chart data |
| `credentials/`, `*-token.json` | Credentials and rotated cloud tokens |

## Troubleshooting

| Symptom | Check |
| --- | --- |
| Service won't start | `journalctl -u energie -n 50`. Run `energie --check-config`. A missing `LoadCredential` source file stops the unit before energie runs. |
| Dashboard unreachable | `listen` must be an address other machines can reach, such as `0.0.0.0:8080`. Check your firewall. |
| `401` from the dashboard | Any user name with the contents of `web_password_file`. |
| Won't start: `web_password_file is required` | `listen` is reachable from the network. Set a password. |
| Won't start: `needs username and password_file` | A broker on the network needs a login. |
| MQTT `/set` or HA controls do nothing | The bridge is publish-only unless `allow_set = true`. |
| A device is unavailable | See `last_error` under *Systeem*. Check that its IP hasn't changed and that no other client holds the connection. |
| Solis timeouts or reconnects | Another Modbus master is probably active. |
| Commands show `blocked: no reading of … yet` | The device hasn't been read yet. Fix the read first. |
| A command is `failed` | The device didn't take the value. Compare the audit entry with the device, and make sure no other controller is writing to it. |
| Charger shows a fault after a start | Check that only one system controls it. |
| Cloud device shows `blocked` | Three logins were rejected. Run `energie credentials set <id>`. |
| No prices | The feed may not have tomorrow's prices until the afternoon. Check `last_error` of the `price_feed` device. |
| Sun-based actions at the wrong time | Set `[site]`. |
| Times look wrong | Local time is always Europe/Brussels. Check the system clock too. |

When filing an issue, include the version from `energie --version`, the relevant log lines and
your config **with addresses and identifiers removed**.

## Resource use

Measured on one x86-64 server in control mode, with about ten devices: 18 MB of memory, 0.35 %
of one CPU core and 0.6 KB/s of disk writes. The supplied systemd unit caps memory at 128 MB.
