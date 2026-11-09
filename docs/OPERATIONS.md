# Operating energie

## Modes

| `mode` | Reads from | Writes to devices | Use it to |
| --- | --- | --- | --- |
| `simulate` | Built-in house model | Nothing | Explore, develop, demonstrate |
| `shadow` | Home Assistant and any configured devices | Nothing | Compare decisions with an existing HA setup |
| `control` with `dry_run = true` | The devices | Nothing, plans are logged | Verify the writes before going live |
| `control` | The devices | Yes | Run the house |

## The dashboard

Open the `listen` address in a browser. The interface follows the browser's language, English or
Dutch, and the button next to the clock switches it. The tabs are Overview, Capacity tariff,
Prices, Battery & Solis, EV charging and System (system health, commands and device states).
Without a home battery the battery tab and controls are hidden, and a charger slot nothing
reads is hidden too.

To charge from the grid in cheap hours, drag across the hours you want on the price chart. energie
shows the limit that covers them, and applying it sets *Cheap charging: price limit* and turns
the grid level to *Max to peak*. The limit is a price, not a timetable: every quarter hour below it counts,
on any day, and the preview says how many others that includes.

## Installing on a phone

The dashboard installs as an app, with its own icon and window. On Android, use the *Install*
button in the header or *Install app* in the Chrome menu. On iPhone, open it in Safari and choose
*Share*, then *Add to Home Screen*.

Installing needs HTTPS, because browsers only allow it on a secure address. Put a TLS reverse
proxy in front of energie, as for OIDC. On plain `http://` a phone can at most add a bookmark to
the home screen.

The app stores no energy data on the phone and shows live values only, so it needs a connection
to the server. Away from home that means a VPN or a public HTTPS address with sign-in. OIDC
is the better sign-in for an installed app, because the password dialog isn't available in every
installed-app window.

## Running without writing to an SD card

energie writes little on its own: settings and counters only when they changed, at most once per
`persist_interval_s`, and one small history line a minute. To write less, or nothing:

```toml
require_state = true             # control stays off if the saved state is gone
persist_interval_s = 3600
state_dir = "/run/energie"       # a tmpfs, or a small partition that survives a reboot

[history]
storage = "memory"

[history.victoriametrics]
url = "http://192.168.1.20:8428"
username = "energie"
password_file = "/run/credentials/energie.service/vm-password"
```

Storage in RAM is lost at every reboot, and that matters. The kill switch, settings, sign-in tokens for
cloud accounts and the 12 monthly peaks behind the capacity tariff are all in `state_dir`. If you
can, keep `state_dir` on something that survives a reboot, such as a small partition, USB stick or
network share. If it really is RAM, `require_state` makes energie start with control off after
each reboot, until someone switches it on again on the System tab.

In memory, history keeps 2 days at 1-minute detail and the quarter-hour and daily rows until a
restart. With `[history.victoriametrics]` every closed minute is also sent to the database, up to
three days are queued in memory while it's out of reach, and the charts read older data
back from it. Run the database on another machine:

```sh
docker run -d --name victoriametrics --restart unless-stopped -p 8428:8428 \
  -v vm-data:/victoria-metrics-data victoriametrics/victoria-metrics \
  -retentionPeriod=5y -httpAuth.username=energie -httpAuth.password=CHANGE_ME
```

The data is stored as `energie_<series>_mean`, `_min`, `_max` and `_last` with a `site` label, ready
to use from Grafana. Any database that accepts InfluxDB line protocol on `/write` can receive it,
with `read = false` if it can't answer the chart queries.

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
| Won't start: `[oidc] or web_password_file is required` | `listen` is reachable from the network. Set up sign-in. |
| *Geen toegang* after signing in | The account isn't on the allow-list, or the email isn't verified. The log says which. |
| *Aanmelden lukt nu niet* | The provider can't be reached, or `issuer` doesn't match exactly what it states. Check the log. |
| A bearer token gets `401` | The log gives the reason at `info` level. Check `api_audience`, and that it's a JWT and not an opaque token. |
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
