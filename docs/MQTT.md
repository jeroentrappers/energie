# The MQTT bridge

`energie` publishes its entity registry (`crates/app/src/entities.rs`, the same entities as
`/api/entities`) on an MQTT broker: one retained topic per value for any client, a compact
summary for small displays, `/set` topics for what can be set, and Home Assistant MQTT
discovery so HA becomes a view that can still change the settings. A device added as a
`[[device]]` shows up everywhere without code: its entities get topics, discovery and `/set`
like the rest.

Nothing is published unless `[mqtt_publish] enabled = true`. The passive `[mqtt]` subscriber
of the shadow comparison is a separate thing and still never publishes.

```toml
[mqtt_publish]
enabled = true
broker = "192.168.1.10"
port = 1883
username = "energie"
password_file = "/run/credentials/energie.service/mqtt-password"
client_id = "energie"
prefix = "energie"                 # topic prefix; may have levels ("huis/energie")
discovery = true                   # default: on, except in shadow mode
discovery_prefix = "homeassistant"
include = ["*"]                    # globs on entity ids
exclude = ["derived_kwartier_index"]
summary_interval_s = 5
```

How often a value goes out follows the `[[publish]]` rules (`min_interval_s`, `deadband`,
`enabled`; `Publish::due`), evaluated every tick: a value that changed inside its interval goes
out once the interval has passed. A value whose entity is excluded or disabled is neither
published nor settable.

## Topics

| Topic | Retained | Payload |
| --- | --- | --- |
| `<prefix>/status` | yes | `online`; `offline` at a clean stop and as the last will |
| `<prefix>/<group>/<key>` | yes | the value: a number as JSON writes it (`3066.0`), `ON`/`OFF`, a text, `unavailable` (HA's unavailable/unknown) |
| `<prefix>/<group>/<key>/attributes` | yes | `{"name", "unit", "class", "ha_entity", "updated"}`; `updated` is the tick that sent the value |
| `<prefix>/summary` | yes | the summary below, every `summary_interval_s` |
| `<prefix>/<group>/<key>/set` | no (subscribed) | a new value for a settable entity |
| `<discovery_prefix>/<component>/<node>/<entity id>/config` | yes | HA discovery (below); `<node>` is the prefix with `/` as `_` |
| `<discovery_prefix>/status` | (HA's) | subscribed: `online` makes energie send its discovery and states again |

`<group>/<key>` is the entity id split at the group: `world/grid_meter_power_w`,
`derived/kwartier_gemiddelde_w`, `setting/ev_net_niveau`, `memory/capaciteit_piekhistoriek`,
`ev3/ac_limit`, `forecast/energy_today_kwh`. The groups:

| Group | What |
| --- | --- |
| `world` | the World as the logic sees it, by role (`<slot>_<field>`) |
| `<device id>` | one `[[device]]` instance's own readings and its driver's extra entities |
| `derived` | the template sensors and meters |
| `setting` | the settings (the former `input_*` helpers) |
| `memory` | the latches, counters and measurements the controllers keep (read only) |

### Setting values

`/set` is off by default. It needs `allow_set = true` in `[mqtt_publish]`, and a broker access
list that limits who may write these topics: see
[`deploy/mosquitto-energie.acl`](../deploy/mosquitto-energie.acl). A broker on the network also
needs a `username` and `password_file`, or energie won't start.

`<prefix>/<group>/<key>/set` takes, by the entity's control: a number (`750`, `14.5`), `ON` /
`OFF` (also `true`/`false`/`1`/`0`), one of a select's options, or a text. The value then
goes through `Runtime::set_entity`, the path of `POST /api/entities/<id>`: the control
checks type, range, step and options; a setting becomes a settings change (persisted at
once), a World field the command the device's controller carries out. Anything else (an
unknown entity, one that is not settable now — read only, shadow mode, `settings_source =
"ha"` —, a value that does not parse or does not fit) is refused and logged
(`mqtt set refused`); nothing changes. The new value comes back on the state topic.

```
mosquitto_pub -h 192.168.1.10 -u USER -P PASSWORD -t energie/setting/ev_net_niveau/set -m "Uit"
mosquitto_pub -h 192.168.1.10 -u USER -P PASSWORD -t energie/setting/batterij_reservering_w/set -m 750
mosquitto_pub -h 192.168.1.10 -u USER -P PASSWORD -t energie/setting/peak_guard_actief/set -m OFF
```

### Connection

The client (`rumqttc`, MQTT 3.1.1, QoS 1) connects with the last will `<prefix>/status` =
`offline` (retained). On every connect it subscribes to `<prefix>/+/+/set` (and
`<discovery_prefix>/status` with discovery on), publishes `online`, and sends every value,
its attributes, the summary and the discovery again; a lost connection is retried with
exponential backoff (1 s up to 60 s). While disconnected nothing is queued. At a clean stop
it publishes `offline` and disconnects.

## The summary

`<prefix>/summary`, retained, every `summary_interval_s` (5 s): the house in one message for
a display that does not want 250 topics. Unknown values are `null`. Powers in W (grid: +
import / − export; battery: + discharging / − charging), prices in €/kWh.

```json
{
  "at": "2026-11-08T12:00:00+01:00",
  "mode": "control",
  "grid_w": 3066.0,
  "pv_w": 533.0,
  "house_w": 3599.0,
  "battery": {"soc": 54.0, "w": -1200.0, "setpoint_w": -1200.0},
  "ev3": {"w": 0.0, "a": null, "status": "Idle", "soc": 54.5, "plugged": false},
  "buzz": {"w": 3404.0, "a": 14.8, "mode": "Normal", "plug": "Connected", "soc": 43.0, "target_soc": 100.0},
  "active_car": "Buzz",
  "price": {"now": 0.37692, "next": 0.3787, "level": "goedkoop"},
  "quarter": {"avg_w": 3270.0, "target_w": 3900.0, "projection_w": 3085.0, "allowed_w": 3830.0},
  "peak_risk": false
}
```

| Field | Source |
| --- | --- |
| `at`, `mode` | the tick's time, the run mode |
| `grid_w` | `world.p1.power_w` (the P1 meter) |
| `pv_w`, `house_w` | `derived.pv_vermogen_totaal_w`, `derived.huisverbruik_vermogen_w` |
| `battery` | `world.solis.soc`, `world.solis.battery_power_w`, `derived.batterij_setpoint_w` |
| `ev3` | the cable: `derived.ev_laadkabel_vermogen_w`, `world.cable.current_a`, `.status`; the car: `world.ev3.soc`, `.plugged` |
| `buzz` | the SmartEVSE: `derived.ev_laadvermogen_smartevse_w`, `world.evse.charge_current_a`, `.mode`, `.plug`; the car: `world.buzz.soc`, `.target_soc` |
| `active_car` | `derived.ev_actieve_laadauto` |
| `price` | `derived.stroomprijs_nu`, `derived.kwartierprijs_volgend`, `derived.stroomprijs_niveau` |
| `quarter` | this quarter's average (`kwartier_gemiddelde_w`) against the month target (`capaciteit_maandtarget_kw` × 1000), the projection and what may still be drawn (`kwartier_toegestaan_vermogen_w`) |
| `peak_risk` | `derived.capaciteitspiek_risico` |

### An external display

Python (`paho-mqtt`), a line per summary:

```python
import json, paho.mqtt.client as mqtt

def on_message(c, _, m):
    s = json.loads(m.payload)
    q = s["quarter"]
    print(f"net {s['grid_w']:.0f} W  zon {s['pv_w']:.0f} W  batterij {s['battery']['soc']:.0f}%  "
          f"kwartier {q['avg_w']:.0f}/{q['target_w']:.0f} W  {'PIEK!' if s['peak_risk'] else ''}")

c = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2)
c.on_message = on_message
c.connect("192.168.1.10")
c.subscribe("energie/summary")
c.loop_forever()
```

An ESP32 with ESPHome can take single values instead (retained, so a display that boots gets
them at once):

```yaml
mqtt:
  broker: 192.168.1.10
sensor:
  - platform: mqtt_subscribe
    name: "Net"
    topic: energie/world/grid_meter_power_w
  - platform: mqtt_subscribe
    name: "Kwartier gemiddelde"
    topic: energie/derived/kwartier_gemiddelde_w
text_sensor:
  - platform: mqtt_subscribe
    name: "Energie"
    topic: energie/status
```

`mosquitto_sub -h 192.168.1.10 -t 'energie/#' -v` shows everything.

## Home Assistant discovery

With discovery on, every published entity gets a config under
`<discovery_prefix>/<component>/<node>/<entity id>/config`:

- **Component**: `number`, `select`, `switch` or `text` for a settable entity (its control:
  min/max/step with `mode: box`, the options, `ON`/`OFF`, the maximum length;
  `command_topic` is the `/set` topic); otherwise `binary_sensor` for an on/off value and
  `sensor` for the rest. An entity that is settable only in some setups (a World field with or
  without a controller) changes component with it; the old config is cleared (an empty
  retained message) and the new one sent.
- **Device grouping**: `world` is the device *energie*; `derived` (*energie berekend*),
  `setting` (*energie instellingen*), `memory` (*energie geheugen*, entity category
  diagnostic) and each `[[device]]` instance (its `name`, model = its kind) are devices via
  *energie*.
- **Availability**: `<prefix>/status`. A value `unavailable` renders as HA's unknown
  (`value_template: "{{ none if value == 'unavailable' else value }}"`).
- **Unit, device class, state class**: for an entity that stands for an HA entity, HA's own
  (from the live snapshot of 2026-11-07, `bridge::ha_tables::HA_META`), so the long-term
  statistics continue under the same entity id; otherwise the entity's (`state_class` when a
  driver names one; else energy → `total_increasing`, monetary → `total`, power, current,
  voltage, battery and `%` → `measurement`). kWh totals of a driver (the Solis energy
  counters) therefore come with `device_class: energy`, `state_class: total_increasing` and
  can go on HA's Energy dashboard; the forecast's kWh values have no state class (they are
  not meters).
- `unique_id` `<node>_<entity id>`; `origin` energie with its version.

The configs are sent on connect, when HA says `online`, and when the registry's shape changes
(a device's extra entities appear after its first read, a control appears or goes).

### Taking over HA's entity ids

At the cutover the HA template sensors, helpers and integrations that energie replaces are
removed from HA, and the MQTT entities take over their entity ids, so the dashboard, the
history and the Energy dashboard keep working. Each config carries `default_entity_id`
(HA 2025.10 and later) and `object_id` (older HA):

- An entity listed in `bridge::ha_tables::TAKEOVER` gets `<component>.<object id>` of the
  entity id the cutover gives it. The table follows the cutover mapping of 2026-11-08 (the
  HA repo's `cutover_mapping.json`, 164 rows with an entity).
- A driver's own entity that names its HA entity (`Entity::with_ha`) takes that id over
  (the forecast, the EV3's extras, and new device entities such as the Solis energy totals).
- Every other entity gets `<component>.<node>_<entity id>`
  (`sensor.energie_world_grid_meter_power_w`): the HomeWizard P1, the Omnik, the SmartEVSE
  and the price sensors stay HA's own, so energie does not claim them.

An `input_*`, `counter`, `timer` or `input_datetime` helper cannot keep its domain over MQTT:
a setting becomes `number.` / `select.` / `switch.` with the helper's object id
(`input_number.batterij_reserve_soc` → `number.batterij_reserve_soc`), a hidden state helper
a read-only `sensor.` / `binary_sensor.` (`input_boolean.batterij_net_laden` →
`binary_sensor.batterij_net_laden`, `counter.peak_guard_interventies` →
`sensor.peak_guard_interventies`); dashboard cards that name the old ids are repointed at
the cutover. When the component differs from the table's domain (a World field that is
read-only because no device controls it), the id keeps the object id with the actual
component (`binary_sensor.airco_zolder_power` instead of `switch.airco_zolder_power`).

HA assigns `default_entity_id` only when it first sees the `unique_id`, and only if the id is
free: if the old entity still exists, the MQTT entity becomes `<id>_2` and keeps that. So:

1. Run with `[mqtt_publish] enabled = true` and `discovery = false` (the default in shadow
   mode) to check the topics.
2. At the cutover remove (or let the cutover script remove) the HA entities energie
   replaces, and SmartEVSE's own discovery if a takeover would collide with it.
3. Then switch discovery on (`discovery = true`, or leave shadow mode) and restart energie,
   or publish `online` to `homeassistant/status`.
4. An entity that got `_2` anyway: delete it in HA (Settings → Entities) or clear its config
   (`mosquitto_pub -r -t homeassistant/<component>/energie/<id>/config -n`) and let energie
   send it again.

HA's Energy dashboard reads the solar forecast from the `forecast_solar` integration's
config entry, not from entities; MQTT sensors cannot stand in for that. Keep HA's
forecast.solar integration for the Energy dashboard's forecast line (energie's `forecast`
entities then take other ids until it is removed).

### The entity ids taken over

#### Template sensors, utility meters, integrals (same id) (88)

| energie entity | HA entity today | takes over |
| --- | --- | --- |
| `derived_kwartier_import_kwh` | `sensor.p1_meter_kwartier_import` | `sensor.p1_meter_kwartier_import` |
| `derived_p1_mediaan_w` | `sensor.p1_meter_p1_mediaan_90s` | `sensor.p1_meter_p1_mediaan_90s` |
| `derived_batterij_ontladen_energie_kwh` | `sensor.batterij_ontladen_energie` | `sensor.batterij_ontladen_energie` |
| `derived_batterij_geladen_energie_kwh` | `sensor.batterij_geladen_energie` | `sensor.batterij_geladen_energie` |
| `derived_pv_energie_totaal_kwh` | `sensor.pv_energie_totaal` | `sensor.pv_energie_totaal` |
| `derived_huisverbruik_energie_kwh` | `sensor.huisverbruik_energie` | `sensor.huisverbruik_energie` |
| `derived_energiekost_cumulatief_eur` | `sensor.energiekost_cumulatief` | `sensor.energiekost_cumulatief` |
| `derived_ev3_laadenergie_zon_kwh` | `sensor.ev3_laadenergie_zon` | `sensor.ev3_laadenergie_zon` |
| `derived_ev3_laadenergie_net_kwh` | `sensor.ev3_laadenergie_net` | `sensor.ev3_laadenergie_net` |
| `derived_ev3_laadenergie_totaal_kwh` | `sensor.ev3_laadenergie_totaal` | `sensor.ev3_laadenergie_totaal` |
| `derived_ev3_laadkost_net_eur` | `sensor.ev3_laadkost_net` | `sensor.ev3_laadkost_net` |
| `derived_buzz_laadenergie_zon_kwh` | `sensor.buzz_laadenergie_zon` | `sensor.buzz_laadenergie_zon` |
| `derived_buzz_laadenergie_net_kwh` | `sensor.buzz_laadenergie_net` | `sensor.buzz_laadenergie_net` |
| `derived_buzz_laadenergie_totaal_kwh` | `sensor.buzz_laadenergie_totaal` | `sensor.buzz_laadenergie_totaal` |
| `derived_buzz_laadkost_net_eur` | `sensor.buzz_laadkost_net` | `sensor.buzz_laadkost_net` |
| `derived_pv_energie_dag_kwh` | `sensor.pv_energie_dag` | `sensor.pv_energie_dag` |
| `derived_huisverbruik_dag_kwh` | `sensor.huisverbruik_dag` | `sensor.huisverbruik_dag` |
| `derived_netimport_dag_kwh` | `sensor.netimport_dag` | `sensor.netimport_dag` |
| `derived_injectie_dag_kwh` | `sensor.injectie_dag` | `sensor.injectie_dag` |
| `derived_batterij_geladen_dag_kwh` | `sensor.batterij_geladen_dag` | `sensor.batterij_geladen_dag` |
| `derived_batterij_ontladen_dag_kwh` | `sensor.batterij_ontladen_dag` | `sensor.batterij_ontladen_dag` |
| `derived_energiekost_vandaag_eur` | `sensor.energiekost_vandaag` | `sensor.energiekost_vandaag` |
| `derived_ev3_laadkost_dag_eur` | `sensor.ev3_laadkost_dag` | `sensor.ev3_laadkost_dag` |
| `derived_ev3_laadkost_maand_eur` | `sensor.ev3_laadkost_maand` | `sensor.ev3_laadkost_maand` |
| `derived_ev3_laadenergie_dag_kwh` | `sensor.ev3_laadenergie_dag` | `sensor.ev3_laadenergie_dag` |
| `derived_ev3_laadenergie_maand_kwh` | `sensor.ev3_laadenergie_maand` | `sensor.ev3_laadenergie_maand` |
| `derived_buzz_laadkost_dag_eur` | `sensor.buzz_laadkost_dag` | `sensor.buzz_laadkost_dag` |
| `derived_buzz_laadkost_maand_eur` | `sensor.buzz_laadkost_maand` | `sensor.buzz_laadkost_maand` |
| `derived_buzz_laadenergie_dag_kwh` | `sensor.buzz_laadenergie_dag` | `sensor.buzz_laadenergie_dag` |
| `derived_buzz_laadenergie_maand_kwh` | `sensor.buzz_laadenergie_maand` | `sensor.buzz_laadenergie_maand` |
| `derived_kwartiervermogen_vastgelegd_kw` | `sensor.kwartiervermogen_vastgelegd` | `sensor.kwartiervermogen_vastgelegd` |
| `derived_kwartiervermogen_projectie_w` | `sensor.kwartiervermogen_projectie` | `sensor.kwartiervermogen_projectie` |
| `derived_pv_vermogen_totaal_w` | `sensor.pv_vermogen_totaal` | `sensor.pv_vermogen_totaal` |
| `derived_huisverbruik_vermogen_w` | `sensor.huisverbruik_vermogen` | `sensor.huisverbruik_vermogen` |
| `derived_batterij_beschikbare_energie_kwh` | `sensor.batterij_beschikbare_energie` | `sensor.batterij_beschikbare_energie` |
| `derived_stroomprijs_niveau` | `sensor.stroomprijs_niveau` | `sensor.stroomprijs_niveau` |
| `derived_capaciteitspiek_risico` | `binary_sensor.capaciteitspiek_risico` | `binary_sensor.capaciteitspiek_risico` |
| `derived_kwartierprijs_volgend` | `sensor.kwartierprijs_volgend` | `sensor.kwartierprijs_volgend` |
| `derived_vaste_kosten_per_maand_eur` | `sensor.vaste_kosten_per_maand` | `sensor.vaste_kosten_per_maand` |
| `derived_capaciteitstarief_deze_maand_eur` | `sensor.capaciteitstarief_deze_maand` | `sensor.capaciteitstarief_deze_maand` |
| `derived_pv_overschot_beschikbaar_w` | `sensor.pv_overschot_beschikbaar` | `sensor.pv_overschot_beschikbaar` |
| `derived_huisverbruik_excl_stuurbaar_w` | `sensor.huisverbruik_excl_stuurbaar` | `sensor.huisverbruik_excl_stuurbaar` |
| `derived_piek_verschil_fluvius_vs_p1_w` | `sensor.piek_verschil_fluvius_vs_p1` | `sensor.piek_verschil_fluvius_vs_p1` |
| `derived_capaciteit_maandtarget_kw` | `sensor.capaciteit_maandtarget` | `sensor.capaciteit_maandtarget` |
| `derived_pv_bruto_overschot_w` | `sensor.pv_bruto_overschot` | `sensor.pv_bruto_overschot` |
| `derived_kost_per_uur_eur` | `sensor.kost_per_uur` | `sensor.kost_per_uur` |
| `derived_solis_offline` | `binary_sensor.solis_offline` | `binary_sensor.solis_offline` |
| `derived_ev_actieve_laadauto` | `sensor.ev_actieve_laadauto` | `sensor.ev_actieve_laadauto` |
| `derived_ev_laadvermogen_zon_kw` | `sensor.ev_laadvermogen_zon` | `sensor.ev_laadvermogen_zon` |
| `derived_ev_laadvermogen_net_kw` | `sensor.ev_laadvermogen_net` | `sensor.ev_laadvermogen_net` |
| `derived_ev_laadkabel_vermogen_w` | `sensor.ev_laadkabel_vermogen_w` | `sensor.ev_laadkabel_vermogen_w` |
| `derived_ev_laadvermogen_smartevse_kw` | `sensor.ev_laadvermogen_smartevse` | `sensor.ev_laadvermogen_smartevse` |
| `derived_ev_laadvermogen_smartevse_w` | `sensor.ev_laadvermogen_smartevse_w` | `sensor.ev_laadvermogen_smartevse_w` |
| `derived_injectie_effectieve_terugleververgoeding` | `sensor.injectie_effectieve_terugleververgoeding` | `sensor.injectie_effectieve_terugleververgoeding` |
| `derived_batterij_naar_auto_geleverd_kwh` | `sensor.batterij_naar_auto_geleverd_kwh` | `sensor.batterij_naar_auto_geleverd_kwh` |
| `derived_capaciteitspiek_meerkost_eur` | `sensor.capaciteitspiek_meerkost` | `sensor.capaciteitspiek_meerkost` |
| `derived_batterij_ontlaadvermogen_w` | `sensor.batterij_ontlaadvermogen` | `sensor.batterij_ontlaadvermogen` |
| `derived_batterij_laadvermogen_w` | `sensor.batterij_laadvermogen` | `sensor.batterij_laadvermogen` |
| `derived_zelfverbruik_vandaag_pct` | `sensor.zelfverbruik_vandaag` | `sensor.zelfverbruik_vandaag` |
| `derived_zelfvoorziening_vandaag_pct` | `sensor.zelfvoorziening_vandaag` | `sensor.zelfvoorziening_vandaag` |
| `derived_zelfverbruik_vandaag_kwh` | `sensor.zelfverbruik_vandaag_kwh` | `sensor.zelfverbruik_vandaag_kwh` |
| `derived_kwartier_toegestaan_vermogen_w` | `sensor.kwartier_toegestaan_vermogen` | `sensor.kwartier_toegestaan_vermogen` |
| `derived_kwartier_gemiddelde_w` | `sensor.kwartier_gemiddelde` | `sensor.kwartier_gemiddelde` |
| `derived_kwartier_index` | `sensor.kwartier_index` | `sensor.kwartier_index` |
| `derived_batterij_reservering_effectief_w` | `sensor.batterij_reservering_effectief` | `sensor.batterij_reservering_effectief` |
| `derived_stroomprijs_nu` | `sensor.stroomprijs_nu` | `sensor.stroomprijs_nu` |
| `derived_gefactureerde_piek_kw` | `sensor.gefactureerde_piek` | `sensor.gefactureerde_piek` |
| `derived_capaciteitstarief_factuur_deze_maand_eur` | `sensor.capaciteitstarief_factuur_deze_maand` | `sensor.capaciteitstarief_factuur_deze_maand` |
| `derived_batterij_setpoint_w` | `sensor.batterij_setpoint` | `sensor.batterij_setpoint` |
| `derived_dispatcher_off_target` | `binary_sensor.dispatcher_off_target` | `binary_sensor.dispatcher_off_target` |
| `derived_ev3_laadvermogen_zon_kw` | `sensor.ev3_laadvermogen_zon` | `sensor.ev3_laadvermogen_zon` |
| `derived_ev3_laadvermogen_net_kw` | `sensor.ev3_laadvermogen_net` | `sensor.ev3_laadvermogen_net` |
| `derived_ev3_laadvermogen_totaal_kw` | `sensor.ev3_laadvermogen_totaal` | `sensor.ev3_laadvermogen_totaal` |
| `derived_ev3_laadkost_per_uur_eur` | `sensor.ev3_laadkost_per_uur` | `sensor.ev3_laadkost_per_uur` |
| `derived_ev3_laad_zonaandeel_pct` | `sensor.ev3_laad_zonaandeel` | `sensor.ev3_laad_zonaandeel` |
| `derived_ev3_laadkost_per_100km_eur` | `sensor.ev3_laadkost_per_100km` | `sensor.ev3_laadkost_per_100km` |
| `derived_ev3_verbruik_per_100km_kwh` | `sensor.ev3_verbruik_per_100km` | `sensor.ev3_verbruik_per_100km` |
| `derived_ev3_ice_kost_per_100km_eur` | `sensor.ev3_ice_kost_per_100km` | `sensor.ev3_ice_kost_per_100km` |
| `derived_ev3_besparing_per_100km_eur` | `sensor.ev3_besparing_per_100km` | `sensor.ev3_besparing_per_100km` |
| `derived_buzz_laadvermogen_zon_kw` | `sensor.buzz_laadvermogen_zon` | `sensor.buzz_laadvermogen_zon` |
| `derived_buzz_laadvermogen_net_kw` | `sensor.buzz_laadvermogen_net` | `sensor.buzz_laadvermogen_net` |
| `derived_buzz_laadvermogen_totaal_kw` | `sensor.buzz_laadvermogen_totaal` | `sensor.buzz_laadvermogen_totaal` |
| `derived_buzz_laadkost_per_uur_eur` | `sensor.buzz_laadkost_per_uur` | `sensor.buzz_laadkost_per_uur` |
| `derived_buzz_laad_zonaandeel_pct` | `sensor.buzz_laad_zonaandeel` | `sensor.buzz_laad_zonaandeel` |
| `derived_buzz_laadkost_per_100km_eur` | `sensor.buzz_laadkost_per_100km` | `sensor.buzz_laadkost_per_100km` |
| `derived_buzz_verbruik_per_100km_kwh` | `sensor.buzz_verbruik_per_100km` | `sensor.buzz_verbruik_per_100km` |
| `derived_buzz_ice_kost_per_100km_eur` | `sensor.buzz_ice_kost_per_100km` | `sensor.buzz_ice_kost_per_100km` |
| `derived_buzz_besparing_per_100km_eur` | `sensor.buzz_besparing_per_100km` | `sensor.buzz_besparing_per_100km` |

#### Settings: `input_*` → `number.`/`select.`/`switch.` (same object id) (21)

| energie entity | HA entity today | takes over |
| --- | --- | --- |
| `setting_zonprioriteit` | `input_select.zonprioriteit` | `select.zonprioriteit` |
| `setting_ev_net_niveau` | `input_select.ev_net_niveau` | `select.ev_net_niveau` |
| `setting_ev_batterij_niveau` | `input_select.ev_batterij_niveau` | `select.ev_batterij_niveau` |
| `setting_goedkoop_laden_prijs_drempel` | `input_number.goedkoop_laden_prijs_drempel` | `number.goedkoop_laden_prijs_drempel` |
| `setting_capaciteit_maandtarget_instelling` | `input_number.capaciteit_maandtarget_instelling` | `number.capaciteit_maandtarget_instelling` |
| `setting_batterij_naar_auto_budget` | `input_number.batterij_naar_auto_budget` | `number.batterij_naar_auto_budget` |
| `setting_batterij_reserve_soc` | `input_number.batterij_reserve_soc` | `number.batterij_reserve_soc` |
| `setting_batterij_reservering_actief` | `input_boolean.batterij_reservering_actief` | `switch.batterij_reservering_actief` |
| `setting_batterij_reservering_w` | `input_number.batterij_reservering_w` | `number.batterij_reservering_w` |
| `setting_pv_overschot_laden_actief` | `input_boolean.pv_overschot_laden_actief` | `switch.pv_overschot_laden_actief` |
| `setting_peak_guard_actief` | `input_boolean.peak_guard_actief` | `switch.peak_guard_actief` |
| `setting_soluna_absorptie_actief` | `input_boolean.soluna_absorptie_actief` | `switch.soluna_absorptie_actief` |
| `setting_airco_overschot_koeling_actief` | `input_boolean.airco_overschot_koeling_actief` | `switch.airco_overschot_koeling_actief` |
| `setting_airco_dump_overschot_drempel` | `input_number.airco_dump_overschot_drempel` | `number.airco_dump_overschot_drempel` |
| `setting_airco_dump_soc_drempel` | `input_number.airco_dump_soc_drempel` | `number.airco_dump_soc_drempel` |
| `setting_buzz_laaddoel_soc` | `input_number.buzz_laaddoel_soc` | `number.buzz_laaddoel_soc` |
| `setting_ev_benzineprijs` | `input_number.ev_benzineprijs` | `number.ev_benzineprijs` |
| `setting_ev3_ice_verbruik` | `input_number.ev3_ice_verbruik` | `number.ev3_ice_verbruik` |
| `setting_buzz_ice_verbruik` | `input_number.buzz_ice_verbruik` | `number.buzz_ice_verbruik` |
| `setting_ev3_odometer_start` | `input_number.ev3_odometer_start` | `number.ev3_odometer_start` |
| `setting_buzz_odometer_start` | `input_number.buzz_odometer_start` | `number.buzz_odometer_start` |

#### Hidden state helpers → read-only `sensor.`/`binary_sensor.` (same object id) (21)

| energie entity | HA entity today | takes over |
| --- | --- | --- |
| `memory_batterij_setpoint_bias` | `input_number.batterij_setpoint_bias` | `sensor.batterij_setpoint_bias` |
| `memory_batterij_net_laden` | `input_boolean.batterij_net_laden` | `binary_sensor.batterij_net_laden` |
| `memory_soluna_absorptie_bezig` | `input_boolean.soluna_absorptie_bezig` | `binary_sensor.soluna_absorptie_bezig` |
| `memory_ev_hoger_laadstil` | `input_boolean.ev_hoger_laadstil` | `binary_sensor.ev_hoger_laadstil` |
| `memory_ev_hoger_laadstil_sinds` | `input_number.ev_hoger_laadstil_sinds` | `sensor.ev_hoger_laadstil_sinds` |
| `memory_airco_dump_sessie_bezig` | `input_boolean.airco_dump_sessie_bezig` | `binary_sensor.airco_dump_sessie_bezig` |
| `memory_smartevse_plug_laatst_bekend` | `input_text.smartevse_plug_laatst_bekend` | `sensor.smartevse_plug_laatst_bekend` |
| `memory_smartevse_modus_laatst_bekend` | `input_text.smartevse_modus_laatst_bekend` | `sensor.smartevse_modus_laatst_bekend` |
| `memory_batterij_naar_auto_start_kwh` | `input_number.batterij_naar_auto_start_kwh` | `sensor.batterij_naar_auto_start_kwh` |
| `memory_nacht_import_startmeting` | `input_number.nacht_import_startmeting` | `sensor.nacht_import_startmeting` |
| `memory_nacht_ontlaad_startmeting` | `input_number.nacht_ontlaad_startmeting` | `sensor.nacht_ontlaad_startmeting` |
| `memory_nacht_ev_net_startmeting` | `input_number.nacht_ev_net_startmeting` | `sensor.nacht_ev_net_startmeting` |
| `memory_nachtdekking` | `input_number.nachtdekking` | `sensor.nachtdekking` |
| `memory_nacht_batterij_kwh` | `input_number.nacht_batterij_kwh` | `sensor.nacht_batterij_kwh` |
| `memory_nacht_import_kwh` | `input_number.nacht_import_kwh` | `sensor.nacht_import_kwh` |
| `memory_nacht_duur_uren` | `input_number.nacht_duur_uren` | `sensor.nacht_duur_uren` |
| `memory_capaciteit_piekhistoriek` | `input_text.capaciteit_piekhistoriek` | `sensor.capaciteit_piekhistoriek` |
| `memory_watchdog_laatste_herstart` | `input_datetime.watchdog_laatste_herstart` | `sensor.watchdog_laatste_herstart` |
| `memory_peak_guard_interventies` | `counter.peak_guard_interventies` | `sensor.peak_guard_interventies` |
| `memory_buzz_laadweigering` | `counter.buzz_laadweigering` | `sensor.buzz_laadweigering` |
| `memory_steering_volgende_update` | `timer.steering_volgende_update` | `sensor.steering_volgende_update` |

#### Integration entities read through the World (same id, while the field is settable; read only: `sensor.`/`binary_sensor.`) (24)

| energie entity | HA entity today | takes over |
| --- | --- | --- |
| `world_battery_inverter_soc` | `sensor.solis_s6_eh1p_battery_soc_2` | `sensor.solis_s6_eh1p_battery_soc_2` |
| `world_battery_inverter_battery_power_w` | `sensor.solis_s6_eh1p_battery_power_net_2` | `sensor.solis_s6_eh1p_battery_power_net_2` |
| `world_battery_inverter_pv_power_w` | `sensor.solis_s6_eh1p_total_pv_power_2` | `sensor.solis_s6_eh1p_total_pv_power_2` |
| `world_battery_inverter_rc_mode` | `select.solis_s6_eh1p_rc_force_charge_discharge_2` | `select.solis_s6_eh1p_rc_force_charge_discharge_2` |
| `world_battery_inverter_rc_charge_w` | `number.solis_s6_eh1p_rc_force_battery_charge_power_2` | `number.solis_s6_eh1p_rc_force_battery_charge_power_2` |
| `world_battery_inverter_rc_discharge_w` | `number.solis_s6_eh1p_rc_force_battery_discharge_power_2` | `number.solis_s6_eh1p_rc_force_battery_discharge_power_2` |
| `world_battery_inverter_rc_timeout_min` | `number.solis_s6_eh1p_rc_timeout_2` | `number.solis_s6_eh1p_rc_timeout_2` |
| `world_battery_inverter_allow_grid_charge` | `switch.solis_s6_eh1p_allow_grid_to_charge_the_battery_2` | `switch.solis_s6_eh1p_allow_grid_to_charge_the_battery_2` |
| `world_battery_inverter_feed_in_limit_on` | `switch.solis_s6_eh1p_grid_feed_in_power_limit_switch_2` | `switch.solis_s6_eh1p_grid_feed_in_power_limit_switch_2` |
| `world_battery_inverter_backflow_w` | `number.solis_s6_eh1p_backflow_power_2` | `number.solis_s6_eh1p_backflow_power_2` |
| `world_charger_ev3_status` | `sensor.ev_charging_cable_status` | `sensor.ev_charging_cable_status` |
| `world_charger_ev3_current_a` | `sensor.ev_charging_cable_stroom` | `sensor.ev_charging_cable_stroom` |
| `world_charger_ev3_voltage_v` | `sensor.ev_charging_cable_spanning` | `sensor.ev_charging_cable_spanning` |
| `world_charger_ev3_power_kw` | `sensor.ev_charging_cable_vermogen` | `sensor.ev_charging_cable_vermogen` |
| `world_charger_ev3_set_current_a` | `number.ev_charging_cable_charge_current` | `number.ev_charging_cable_charge_current` |
| `world_car_ev3_plugged` | `binary_sensor.ev3_ev_battery_plug` | `binary_sensor.ev3_ev_battery_plug` |
| `world_car_ev3_soc` | `sensor.ev3_ev_battery_level` | `sensor.ev3_ev_battery_level` |
| `world_car_ev3_odometer_km` | `sensor.ev3_odometer` | `sensor.ev3_odometer` |
| `world_car_buzz_soc` | `sensor.de_buzz_accu` | `sensor.de_buzz_accu` |
| `world_car_buzz_target_soc` | `sensor.de_buzz_laaddoel` | `sensor.de_buzz_laaddoel` |
| `world_car_buzz_odometer_km` | `sensor.de_buzz_kilometerstand` | `sensor.de_buzz_kilometerstand` |
| `world_load_airco_beneden_on` | `switch.airco_beneden_power` | `switch.airco_beneden_power` |
| `world_load_airco_zolder_on` | `switch.airco_zolder_power` | `switch.airco_zolder_power` |
| `world_capacity_peak_peak_kw` | `sensor.fluvius_meter_1sag3200274808_peak_power` | `sensor.fluvius_meter_1sag3200274808_peak_power` |

#### Integration entities of device instances (`ev3`, `forecast`) (10)

| energie entity | HA entity today | takes over |
| --- | --- | --- |
| `ev3_ac_limit` | `number.ev3_ac_charging_limit` | `number.ev3_ac_charging_limit` |
| `ev3_charging` | `binary_sensor.ev3_ev_battery_charge` | `binary_sensor.ev3_ev_battery_charge` |
| `ev3_dc_limit` | `number.ev3_dc_charging_limit` | `sensor.ev3_dc_charging_limit` |
| `ev3_last_updated` | `sensor.ev3_last_updated_at` | `sensor.ev3_last_updated_at` |
| `forecast_power_now_w` | `sensor.power_production_now` | `sensor.power_production_now` |
| `forecast_energy_today_kwh` | `sensor.energy_production_today` | `sensor.energy_production_today` |
| `forecast_energy_today_remaining_kwh` | `sensor.energy_production_today_remaining` | `sensor.energy_production_today_remaining` |
| `forecast_energy_tomorrow_kwh` | `sensor.energy_production_tomorrow` | `sensor.energy_production_tomorrow` |
| `forecast_energy_current_hour_kwh` | `sensor.energy_current_hour` | `sensor.energy_current_hour` |
| `forecast_energy_next_hour_kwh` | `sensor.energy_next_hour` | `sensor.energy_next_hour` |
