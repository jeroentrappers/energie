# Using energie with Home Assistant

energie doesn't need Home Assistant. If you already run it, there are three ways to combine them.

## 1. HA as a display

energie runs the house and HA shows the values. Enable the MQTT bridge with discovery:

```toml
[mqtt_publish]
enabled = true
broker = "192.168.1.10"
username = "energie"
password_file = "/run/credentials/energie.service/mqtt-password"
discovery = true
```

Every entity then appears in HA through MQTT discovery, and settable ones can be changed from
HA. See [`MQTT.md`](MQTT.md).

## 2. Shadow mode: compare before you trust it

In shadow mode energie reads everything from HA, makes its own decisions and never writes.
The *Systeem* view and `/api/shadow` show how far its decisions agree with what HA did.

```toml
mode = "shadow"

[ha]
url = "http://192.168.1.10:8123"
token_file = "/run/credentials/energie.service/ha-token"
```

Note that the entity mapping (`crates/app/src/ha/mapping.rs`) expects the entity ids of the
installation energie was ported from. Shadow comparison is only meaningful if your HA has
matching entities, or if you adapt that table. For most new users, `control` with `dry_run` is
the better way to validate.

## 3. Taking over from HA

Move control from HA to energie in this order:

1. Run `control` with `dry_run` while HA still controls the house, keeping anything that needs
   an exclusive connection in HA until the next step.
2. Make backups of HA and of the state directory.
3. Disable the HA automations that control the energy devices, because two controllers fighting
   over a charger can latch a fault.
4. Disable the HA integrations that need exclusive device access: Modbus and Tuya local.
5. Import existing settings once, if they match:

   ```sh
   energie import-settings --from-ha --config /etc/energie/energie.toml
   ```

6. Remove `dry_run`, restart, and watch the audit log.
7. Turn on MQTT discovery. [`MQTT.md`](MQTT.md) explains how entities can keep their old ids so
   that existing dashboards keep working.

To go back: stop energie, or switch off its kill switch, and then re-enable the HA integrations
and automations.

## Replaying history

`energie replay` runs an export of the HA recorder through the engine and reports differences
from what HA did. See [`REPLAY.md`](REPLAY.md).
