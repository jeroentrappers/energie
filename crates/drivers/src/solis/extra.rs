//! The Solis values beyond what the control logic reads: the lifetime and daily energies of
//! HA's Energy dashboard, the battery's voltage, current and health, its SoC limits, the work
//! mode and the peak-shaving settings. Read-only, as device entities (`solis_<key>`), so they
//! reach the dashboard, the history and a publisher without further code once HA's
//! `solis_modbus` entry is gone.
//!
//! No extra load on the gateway: battery voltage and current come with the measurement block
//! (33133.. instead of 33135..), the SoC limits and peak-shaving registers with the settings
//! reads (every `settings_every` polls, 60 s; HA polls them at 60 s / 300 s), the energies on a
//! slow cycle of five settings reads (300 s, HA's `poll_interval_slow`). A block the inverter
//! refuses only blanks its own entities; it never fails the reading the logic needs.
//!
//! Registers, multipliers and HA entity ids, from `sensor_data/hybrid_sensors.py`,
//! `switch_sensors.py` and `select_sensors.py` (HA entity id = `<platform>.solis_s6_eh1p_<name>_2`
//! as in the live registry of 2026-11-07):
//!
//! | Key | Register | Decode | HA entity |
//! | --- | --- | --- | --- |
//! | `pv_total_energy_kwh` | in 33029-33030 | U32 kWh | `sensor.solis_s6_eh1p_pv_total_energy_generation_2` |
//! | `pv_today_energy_kwh` | in 33035 | ×0.1 kWh | `sensor.solis_s6_eh1p_pv_today_energy_generation_2` |
//! | `battery_voltage_v` | in 33133 | ×0.1 V | `sensor.solis_s6_eh1p_battery_voltage_2` |
//! | `battery_current_a` | in 33134 | S16 ×0.1 A | `sensor.solis_s6_eh1p_battery_current_2` |
//! | `battery_soh_pct` | in 33140 | % | `sensor.solis_s6_eh1p_battery_soh_2` |
//! | `battery_charge_total_kwh` | in 33161-33162 | U32 kWh | `sensor.solis_s6_eh1p_total_battery_charge_energy_2` |
//! | `battery_charge_today_kwh` | in 33163 | ×0.1 kWh | `sensor.solis_s6_eh1p_today_battery_charge_energy_2` |
//! | `battery_discharge_total_kwh` | in 33165-33166 | U32 kWh | `sensor.solis_s6_eh1p_total_battery_discharge_energy_2` |
//! | `battery_discharge_today_kwh` | in 33167 | ×0.1 kWh | `sensor.solis_s6_eh1p_today_battery_discharge_energy_2` |
//! | `grid_import_total_kwh` | in 33169-33170 | U32 kWh | `sensor.solis_s6_eh1p_total_energy_imported_from_grid_2` |
//! | `grid_import_today_kwh` | in 33171 | ×0.1 kWh | `sensor.solis_s6_eh1p_today_energy_imported_from_grid_2` |
//! | `grid_export_total_kwh` | in 33173-33174 | U32 kWh | `sensor.solis_s6_eh1p_total_energy_fed_into_grid_2` |
//! | `grid_export_today_kwh` | in 33175 | ×0.1 kWh | `sensor.solis_s6_eh1p_today_energy_fed_into_grid_2` |
//! | `overdischarge_soc_pct` | hold 43011 | % | `number.solis_s6_eh1p_overdischarge_soc_2` |
//! | `force_charge_soc_pct` | hold 43018 | % | `number.solis_s6_eh1p_force_charge_soc_2` |
//! | `backup_soc_pct` | hold 43024 | % | `number.solis_s6_eh1p_backup_soc_2` |
//! | `work_mode` | hold 43110 | the Storage Mode select's option | `select.solis_s6_eh1p_work_mode_2` |
//! | `storage_control_raw` | hold 43110 | raw | `sensor.solis_s6_eh1p_storage_control_switch_value_2` |
//! | `self_use_mode` | hold 43110 bit 0 | | `switch.solis_s6_eh1p_self_use_mode_2` |
//! | `reserve_battery_mode` | hold 43110 bit 4 | | `switch.solis_s6_eh1p_reserve_battery_mode_2` |
//! | `battery_forcecharge_peakshaving` | hold 43110 bit 8 | | `switch.solis_s6_eh1p_battery_forcecharge_peakshaving_2` |
//! | `peak_shaving_mode` | hold 43110 bit 11 | | `switch.solis_s6_eh1p_peak_shaving_mode_2` |
//! | `grid_peak_shaving_power_enable` | hold 43483 bit 7 | | `switch.solis_s6_eh1p_grid_peak_shaving_power_enable_2` |
//! | `peak_baseline_soc_pct` | hold 43487 | % | `number.solis_s6_eh1p_peak_baseline_soc_2` |
//! | `peak_max_grid_power_w` | hold 43488 | ×100 W | `number.solis_s6_eh1p_peak_max_usable_grid_power_2` |

use std::collections::HashMap;

use crate::entity::Entity;

/// When a block is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cycle {
    /// Every poll (HA: fast, 15 s).
    Poll,
    /// With the settings (HA: normal/slow).
    Settings,
    /// Every fifth settings read (HA: slow, 300 s).
    Slow,
}

/// The extra blocks: (input?, start, count, cycle).
pub const BLOCKS: [(bool, u16, u16, Cycle); 5] = [
    (false, 43011, 14, Cycle::Settings),
    (false, 43483, 1, Cycle::Settings),
    (false, 43487, 2, Cycle::Settings),
    (true, 33029, 7, Cycle::Slow),
    (true, 33161, 16, Cycle::Slow),
];

/// The key of each extra entity and the HA entity it replaces.
pub const HA_ENTITIES: [(&str, &str); 25] = [
    ("pv_total_energy_kwh", "sensor.solis_s6_eh1p_pv_total_energy_generation_2"),
    ("pv_today_energy_kwh", "sensor.solis_s6_eh1p_pv_today_energy_generation_2"),
    ("battery_voltage_v", "sensor.solis_s6_eh1p_battery_voltage_2"),
    ("battery_current_a", "sensor.solis_s6_eh1p_battery_current_2"),
    ("battery_soh_pct", "sensor.solis_s6_eh1p_battery_soh_2"),
    ("battery_charge_total_kwh", "sensor.solis_s6_eh1p_total_battery_charge_energy_2"),
    ("battery_charge_today_kwh", "sensor.solis_s6_eh1p_today_battery_charge_energy_2"),
    ("battery_discharge_total_kwh", "sensor.solis_s6_eh1p_total_battery_discharge_energy_2"),
    ("battery_discharge_today_kwh", "sensor.solis_s6_eh1p_today_battery_discharge_energy_2"),
    ("grid_import_total_kwh", "sensor.solis_s6_eh1p_total_energy_imported_from_grid_2"),
    ("grid_import_today_kwh", "sensor.solis_s6_eh1p_today_energy_imported_from_grid_2"),
    ("grid_export_total_kwh", "sensor.solis_s6_eh1p_total_energy_fed_into_grid_2"),
    ("grid_export_today_kwh", "sensor.solis_s6_eh1p_today_energy_fed_into_grid_2"),
    ("overdischarge_soc_pct", "number.solis_s6_eh1p_overdischarge_soc_2"),
    ("force_charge_soc_pct", "number.solis_s6_eh1p_force_charge_soc_2"),
    ("backup_soc_pct", "number.solis_s6_eh1p_backup_soc_2"),
    ("work_mode", "select.solis_s6_eh1p_work_mode_2"),
    ("storage_control_raw", "sensor.solis_s6_eh1p_storage_control_switch_value_2"),
    ("self_use_mode", "switch.solis_s6_eh1p_self_use_mode_2"),
    ("reserve_battery_mode", "switch.solis_s6_eh1p_reserve_battery_mode_2"),
    ("battery_forcecharge_peakshaving", "switch.solis_s6_eh1p_battery_forcecharge_peakshaving_2"),
    ("peak_shaving_mode", "switch.solis_s6_eh1p_peak_shaving_mode_2"),
    ("grid_peak_shaving_power_enable", "switch.solis_s6_eh1p_grid_peak_shaving_power_enable_2"),
    ("peak_baseline_soc_pct", "number.solis_s6_eh1p_peak_baseline_soc_2"),
    ("peak_max_grid_power_w", "number.solis_s6_eh1p_peak_max_usable_grid_power_2"),
];

/// `STORAGE_MODE_BITS`: every Storage Mode option clears these before setting its own.
const STORAGE_MODE_BITS: [u8; 6] = [0, 1, 2, 4, 6, 11];

/// The Storage Mode options of `select_sensors.py`: name, bit, required bits.
const WORK_MODES: [(&str, u8, &[u8]); 10] = [
    ("Self-Use", 0, &[]),
    ("Self-Use + TOU", 0, &[1]),
    ("Self-Use + Reserve/Backup", 0, &[4]),
    ("Self-Use + TOU + Reserve/Backup", 0, &[1, 4]),
    ("Feed-in Priority", 6, &[]),
    ("Feed-in Priority + TOU", 6, &[1]),
    ("Feed-in Priority + Reserve/Backup", 6, &[4]),
    ("Feed-in Priority + TOU + Reserve/Backup", 6, &[1, 4]),
    ("Off-Grid Operation", 2, &[]),
    ("Peak Shaving", 11, &[]),
];

fn bit(v: u16, b: u8) -> bool {
    v >> b & 1 == 1
}

/// `SolisSelectEntity.current_option` for 43110: options with more required bits first, a
/// strict pass (no other mode bit set) and then a lenient one. The V1 TOU options are not
/// hidden (that needs 33289, which is not read); `None` when nothing matches.
pub fn work_mode(v: u16) -> Option<&'static str> {
    let mut sorted = WORK_MODES.to_vec();
    sorted.sort_by_key(|(_, _, req)| std::cmp::Reverse(req.len()));
    for strict in [true, false] {
        for &(name, b, req) in &sorted {
            if !bit(v, b) || !req.iter().all(|r| bit(v, *r)) {
                continue;
            }
            let own = |c: &u8| *c == b || req.contains(c);
            if strict && STORAGE_MODE_BITS.iter().filter(|c| !own(c)).any(|c| bit(v, *c)) {
                continue;
            }
            return Some(name);
        }
    }
    None
}

/// The extra entities from the register cache, each naming the HA entity it replaces; `null`
/// for what has not been read.
pub fn entities(cache: &HashMap<u16, u16>) -> Vec<Entity> {
    values(cache).into_iter().zip(HA_ENTITIES).map(|(e, (_, ha))| e.with_ha(ha)).collect()
}

fn values(cache: &HashMap<u16, u16>) -> Vec<Entity> {
    let r = |a: u16| cache.get(&a).copied();
    let u32_of = |a: u16| Some(f64::from(u32::from(r(a)?) << 16 | u32::from(r(a + 1)?)));
    let scaled = |a: u16, m: f64| r(a).map(|v| f64::from(v) * m);
    let s16 = |a: u16, m: f64| r(a).map(|v| f64::from(v as i16) * m);
    let flag = |a: u16, b: u8| r(a).map(|v| bit(v, b));
    let n = |key: &str, name: &str, (class, unit): (&str, &str), v: Option<f64>| {
        Entity::sensor(key, name, v.into()).with_unit(class, unit)
    };
    let b = |key: &str, name: &str, v: Option<bool>| Entity::sensor(key, name, v.into());
    const KWH: (&str, &str) = ("energy", "kWh");
    const PCT: (&str, &str) = ("battery", "%");
    let storage = r(43110);
    vec![
        n("pv_total_energy_kwh", "PV-opbrengst totaal", KWH, u32_of(33029)),
        n("pv_today_energy_kwh", "PV-opbrengst vandaag", KWH, scaled(33035, 0.1)),
        n("battery_voltage_v", "Batterijspanning", ("voltage", "V"), scaled(33133, 0.1)),
        n("battery_current_a", "Batterijstroom", ("current", "A"), s16(33134, 0.1)),
        n("battery_soh_pct", "Batterijgezondheid", ("", "%"), scaled(33140, 1.0)),
        n("battery_charge_total_kwh", "Batterij geladen totaal", KWH, u32_of(33161)),
        n("battery_charge_today_kwh", "Batterij geladen vandaag", KWH, scaled(33163, 0.1)),
        n("battery_discharge_total_kwh", "Batterij ontladen totaal", KWH, u32_of(33165)),
        n("battery_discharge_today_kwh", "Batterij ontladen vandaag", KWH, scaled(33167, 0.1)),
        n("grid_import_total_kwh", "Netafname totaal (omvormer)", KWH, u32_of(33169)),
        n("grid_import_today_kwh", "Netafname vandaag (omvormer)", KWH, scaled(33171, 0.1)),
        n("grid_export_total_kwh", "Netinjectie totaal (omvormer)", KWH, u32_of(33173)),
        n("grid_export_today_kwh", "Netinjectie vandaag (omvormer)", KWH, scaled(33175, 0.1)),
        n("overdischarge_soc_pct", "Ontlaadgrens", PCT, scaled(43011, 1.0)),
        n("force_charge_soc_pct", "Noodlaadgrens", PCT, scaled(43018, 1.0)),
        n("backup_soc_pct", "Back-upreserve", PCT, scaled(43024, 1.0)),
        Entity::sensor("work_mode", "Werkmodus", storage.and_then(work_mode).into()),
        n("storage_control_raw", "Opslagregister 43110", ("", ""), storage.map(f64::from)),
        b("self_use_mode", "Eigen verbruik", flag(43110, 0)),
        b("reserve_battery_mode", "Reservemodus", flag(43110, 4)),
        b("battery_forcecharge_peakshaving", "Noodladen bij peak shaving", flag(43110, 8)),
        b("peak_shaving_mode", "Peak shaving", flag(43110, 11)),
        b("grid_peak_shaving_power_enable", "Peak shaving netvermogen", flag(43483, 7)),
        n("peak_baseline_soc_pct", "Peak shaving SoC", PCT, scaled(43487, 1.0)),
        n("peak_max_grid_power_w", "Peak shaving max netvermogen", ("power", "W"), scaled(43488, 100.0)),
    ]
}
