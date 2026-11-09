//! Declarative tables from HA entities to the core types: [`world_bindings`] (device
//! readings), [`settings_bindings`] (the user helpers) and [`memory_bindings`] (hidden helpers,
//! counters, automation timestamps). Each row names the HA entity and the Rust field it feeds;
//! a missing, `unavailable` or unparseable entity leaves Settings/Memory fields as they were
//! and makes World fields `None`. [`meter_bindings`] maps the `utility_meter` / `integration`
//! helpers to `Memory.meters` ([`seed_meters`], [`MeterAdopter`]).

use std::collections::BTreeMap;

use energie_core::automations::ev_laden_cable::START_SCRIPT_ID;
use energie_core::memory::Memory;
use energie_core::meters::{IntegrationConfig, Integrator, MetersState, UtilityConfig, UtilityMeter};
use energie_core::settings::{BatterijNiveau, NetNiveau, Settings, Zonprioriteit};
use energie_core::time::Ts;
use energie_core::world::{EvseMode, PriceSlot, RcMode, World};
use serde::Serialize;
use serde_json::Value;

use super::entity::{EntityState, Lookup, parse_time};

/// One row: the HA entity, the Rust field it feeds, and how.
pub struct Binding<T> {
    pub entity: &'static str,
    /// The Rust field, as a dotted path (documentation and tests).
    pub field: &'static str,
    /// Reads the entity (`None` when HA does not have it) into the target.
    pub set: fn(&mut T, Option<&EntityState>),
}

impl<T> Binding<T> {
    pub fn apply(&self, target: &mut T, e: Option<&EntityState>) {
        (self.set)(target, e)
    }
}

/// The state as a number.
pub fn num(e: Option<&EntityState>) -> Option<f64> {
    e.and_then(EntityState::num)
}

/// A numeric attribute.
pub fn attr(e: Option<&EntityState>, k: &str) -> Option<f64> {
    e.and_then(|e| e.attr_num(k))
}

/// The state as `on`/`off`.
pub fn on(e: Option<&EntityState>) -> Option<bool> {
    e.and_then(EntityState::on_off)
}

/// The state as text (`None` when unavailable).
pub fn text(e: Option<&EntityState>) -> Option<&str> {
    e.and_then(EntityState::value)
}

fn b<T>(entity: &'static str, field: &'static str, set: fn(&mut T, Option<&EntityState>)) -> Binding<T> {
    Binding { entity, field, set }
}

/// Apply every binding of a table.
pub fn apply_all<T>(bindings: &[Binding<T>], l: &dyn Lookup, target: &mut T) {
    for bd in bindings {
        bd.apply(target, l.get(bd.entity));
    }
}

// --- enums ---------------------------------------------------------------------------------

/// `select.solis_s6_eh1p_rc_force_charge_discharge_2` options.
pub fn rc_mode_label(m: RcMode) -> &'static str {
    match m {
        RcMode::None => "None",
        RcMode::ForceCharge => "Solis RC Force Battery Charge",
        RcMode::ForceDischarge => "Solis RC Force Battery Discharge",
    }
}

pub fn parse_rc_mode(s: &str) -> Option<RcMode> {
    [RcMode::None, RcMode::ForceCharge, RcMode::ForceDischarge].into_iter().find(|m| rc_mode_label(*m) == s)
}

/// `select.smartevse_1234_mode` options.
pub fn evse_mode_label(m: EvseMode) -> &'static str {
    m.label()
}

pub fn parse_evse_mode(s: &str) -> Option<EvseMode> {
    [EvseMode::Off, EvseMode::Normal, EvseMode::Smart, EvseMode::Solar, EvseMode::Pause]
        .into_iter()
        .find(|m| evse_mode_label(*m) == s)
}

/// A `prices` attribute: a list of `{time, price}`; malformed items are skipped.
pub fn parse_slots(v: Option<&Value>) -> Vec<PriceSlot> {
    v.and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|it| {
                    let start = parse_time(it.get("time")?.as_str()?)?;
                    let price = it.get("price")?.as_f64()?;
                    Some(PriceSlot { start, price })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn has_tomorrow(e: Option<&EntityState>) -> bool {
    e.and_then(|e| e.attr("prices_tomorrow")).and_then(Value::as_array).is_some_and(|a| !a.is_empty())
}

// --- world ---------------------------------------------------------------------------------

/// Device readings, as documented on each `World` field.
pub fn world_bindings() -> Vec<Binding<World>> {
    vec![
        b("sensor.p1_meter_vermogen", "p1.power_w", |w, e| w.p1.power_w = num(e)),
        b("sensor.p1_meter_stroom_fase_1", "p1.phase_a[0]", |w, e| w.p1.phase_a[0] = num(e)),
        b("sensor.p1_meter_stroom_fase_2", "p1.phase_a[1]", |w, e| w.p1.phase_a[1] = num(e)),
        b("sensor.p1_meter_stroom_fase_3", "p1.phase_a[2]", |w, e| w.p1.phase_a[2] = num(e)),
        b("sensor.p1_meter_energie_import", "p1.import_kwh", |w, e| w.p1.import_kwh = num(e)),
        b("sensor.p1_meter_energie_export", "p1.export_kwh", |w, e| w.p1.export_kwh = num(e)),
        b("sensor.p1_meter_piekverbruik_huidige_maand", "p1.month_peak_w", |w, e| w.p1.month_peak_w = num(e)),
        b("sensor.solis_s6_eh1p_battery_soc_2", "solis.soc", |w, e| w.solis.soc = num(e)),
        b("sensor.solis_s6_eh1p_battery_power_net_2", "solis.battery_power_w", |w, e| w.solis.battery_power_w = num(e)),
        b("sensor.solis_s6_eh1p_total_pv_power_2", "solis.pv_power_w", |w, e| w.solis.pv_power_w = num(e)),
        b("select.solis_s6_eh1p_rc_force_charge_discharge_2", "solis.rc_mode", |w, e| {
            w.solis.rc_mode = text(e).and_then(parse_rc_mode)
        }),
        b("number.solis_s6_eh1p_rc_force_battery_charge_power_2", "solis.rc_charge_w", |w, e| {
            w.solis.rc_charge_w = num(e)
        }),
        b("number.solis_s6_eh1p_rc_force_battery_discharge_power_2", "solis.rc_discharge_w", |w, e| {
            w.solis.rc_discharge_w = num(e)
        }),
        b("number.solis_s6_eh1p_rc_timeout_2", "solis.rc_timeout_min", |w, e| w.solis.rc_timeout_min = num(e)),
        b("switch.solis_s6_eh1p_allow_grid_to_charge_the_battery_2", "solis.allow_grid_charge", |w, e| {
            w.solis.allow_grid_charge = on(e)
        }),
        b("switch.solis_s6_eh1p_grid_feed_in_power_limit_switch_2", "solis.feed_in_limit_on", |w, e| {
            w.solis.feed_in_limit_on = on(e)
        }),
        b("number.solis_s6_eh1p_backflow_power_2", "solis.backflow_w", |w, e| w.solis.backflow_w = num(e)),
        b("sensor.omnik_hoofddak_inverter_omnik_hoofddak_current_power_production", "omnik.power_w", |w, e| {
            w.omnik.power_w = num(e)
        }),
        b("sensor.smartevse_1234_ev_plug_state", "evse.plug", |w, e| w.evse.plug = text(e).map(str::to_string)),
        b("sensor.smartevse_1234_state", "evse.state", |w, e| w.evse.state = text(e).map(str::to_string)),
        b("sensor.smartevse_1234_stateid", "evse.state_id", |w, e| w.evse.state_id = text(e).map(str::to_string)),
        b("select.smartevse_1234_mode", "evse.mode", |w, e| w.evse.mode = text(e).and_then(parse_evse_mode)),
        b("number.smartevse_1234_charge_current_override", "evse.override_a", |w, e| w.evse.override_a = num(e)),
        b("number.smartevse_1234_charge_current_override", "evse.override_max_a", |w, e| {
            w.evse.override_max_a = attr(e, "max")
        }),
        b("sensor.smartevse_1234_charge_current", "evse.charge_current_a", |w, e| w.evse.charge_current_a = num(e)),
        b("sensor.ev_charging_cable_status", "cable.status", |w, e| w.cable.status = text(e).map(str::to_string)),
        b("sensor.ev_charging_cable_stroom", "cable.current_a", |w, e| w.cable.current_a = num(e)),
        b("sensor.ev_charging_cable_spanning", "cable.voltage_v", |w, e| w.cable.voltage_v = num(e)),
        b("sensor.ev_charging_cable_vermogen", "cable.power_kw", |w, e| w.cable.power_kw = num(e)),
        b("number.ev_charging_cable_charge_current", "cable.set_current_a", |w, e| w.cable.set_current_a = num(e)),
        b("number.ev_charging_cable_charge_current", "cable.set_current_max_a", |w, e| {
            w.cable.set_current_max_a = attr(e, "max")
        }),
        b("binary_sensor.ev3_ev_battery_plug", "ev3.plugged", |w, e| w.ev3.plugged = on(e)),
        b("sensor.ev3_ev_battery_level", "ev3.soc", |w, e| w.ev3.soc = num(e)),
        b("number.ev3_ac_charging_limit", "ev3.ac_limit", |w, e| w.ev3.ac_limit = num(e)),
        b("sensor.ev3_odometer", "ev3.odometer_km", |w, e| w.ev3.odometer_km = num(e)),
        b("sensor.de_buzz_accu", "buzz.soc", |w, e| w.buzz.soc = num(e)),
        b("sensor.de_buzz_laaddoel", "buzz.target_soc", |w, e| w.buzz.target_soc = num(e)),
        b("sensor.de_buzz_kilometerstand", "buzz.odometer_km", |w, e| w.buzz.odometer_km = num(e)),
        b("sensor.jeroen_current_electricity_market_price", "prices.afname_now", |w, e| w.prices.afname_now = num(e)),
        b("sensor.jeroen_current_percentage_in_electricity_price_range", "prices.afname_pct_in_range", |w, e| {
            w.prices.afname_pct_in_range = num(e)
        }),
        b("sensor.jeroen_average_electricity_price", "prices.afname_slots", |w, e| {
            w.prices.afname_slots = parse_slots(e.and_then(|e| e.attr("prices")))
        }),
        b("sensor.jeroen_average_electricity_price", "prices.afname_has_tomorrow", |w, e| {
            w.prices.afname_has_tomorrow = has_tomorrow(e)
        }),
        b("sensor.injectie_current_electricity_market_price", "prices.injectie_now", |w, e| {
            w.prices.injectie_now = num(e)
        }),
        b("sensor.injectie_average_electricity_price", "prices.injectie_has_tomorrow", |w, e| {
            w.prices.injectie_has_tomorrow = has_tomorrow(e)
        }),
        b("sensor.entso_e_energy_prices_15_min", "prices.epex_now", |w, e| w.prices.epex_now = num(e)),
        // −1 is the fetcher's "no backup price" marker.
        b("input_number.prijs_backup_nu", "prices.backup_now", |w, e| {
            w.prices.backup_now = num(e).filter(|p| (p + 1.0).abs() > 1e-9)
        }),
        b("switch.airco_beneden_power", "airco.beneden_on", |w, e| w.airco.beneden_on = on(e)),
        b("switch.airco_zolder_power", "airco.zolder_on", |w, e| w.airco.zolder_on = on(e)),
        b("sensor.vriezer_vermogen", "vriezer.power_w", |w, e| w.vriezer.power_w = num(e)),
        b("switch.vriezer", "vriezer.switch_on", |w, e| w.vriezer.switch_on = on(e)),
        b("sensor.fluvius_meter_1sag0000000000_peak_power", "fluvius.peak_kw", |w, e| w.fluvius.peak_kw = num(e)),
    ]
}

/// The World as HA sees it.
pub fn world_from(l: &dyn Lookup) -> World {
    let mut w = World::default();
    apply_all(&world_bindings(), l, &mut w);
    w
}

// --- settings ------------------------------------------------------------------------------

/// The user helpers, as documented on each `Settings` field.
pub fn settings_bindings() -> Vec<Binding<Settings>> {
    macro_rules! num {
        ($e:literal, $f:ident) => {
            b($e, stringify!($f), |s, e| s.$f = num(e).unwrap_or(s.$f))
        };
    }
    macro_rules! flag {
        ($e:literal, $f:ident) => {
            b($e, stringify!($f), |s, e| s.$f = on(e).unwrap_or(s.$f))
        };
    }
    vec![
        b("input_select.zonprioriteit", "zonprioriteit", |s, e| {
            s.zonprioriteit = text(e).and_then(Zonprioriteit::parse).unwrap_or(s.zonprioriteit)
        }),
        b("input_select.ev_net_niveau", "ev_net_niveau", |s, e| {
            s.ev_net_niveau = text(e).and_then(NetNiveau::parse).unwrap_or(s.ev_net_niveau)
        }),
        b("input_select.ev_batterij_niveau", "ev_batterij_niveau", |s, e| {
            s.ev_batterij_niveau = text(e).and_then(BatterijNiveau::parse).unwrap_or(s.ev_batterij_niveau)
        }),
        num!("input_number.goedkoop_laden_prijs_drempel", goedkoop_laden_prijs_drempel),
        num!("input_number.capaciteit_maandtarget_instelling", capaciteit_maandtarget_instelling),
        num!("input_number.batterij_naar_auto_budget", batterij_naar_auto_budget),
        num!("input_number.batterij_reserve_soc", batterij_reserve_soc),
        flag!("input_boolean.batterij_reservering_actief", batterij_reservering_actief),
        num!("input_number.batterij_reservering_w", batterij_reservering_w),
        flag!("input_boolean.pv_overschot_laden_actief", pv_overschot_laden_actief),
        flag!("input_boolean.peak_guard_actief", peak_guard_actief),
        flag!("input_boolean.soluna_absorptie_actief", soluna_absorptie_actief),
        flag!("input_boolean.airco_overschot_koeling_actief", airco_overschot_koeling_actief),
        num!("input_number.airco_dump_overschot_drempel", airco_dump_overschot_drempel),
        num!("input_number.airco_dump_soc_drempel", airco_dump_soc_drempel),
        num!("input_number.buzz_laaddoel_soc", buzz_laaddoel_soc),
        num!("input_number.ev_benzineprijs", ev_benzineprijs),
        num!("input_number.ev3_ice_verbruik", ev3_ice_verbruik),
        num!("input_number.buzz_ice_verbruik", buzz_ice_verbruik),
        num!("input_number.ev3_odometer_start", ev3_odometer_start),
        num!("input_number.buzz_odometer_start", buzz_odometer_start),
    ]
}

/// HA's settings on top of `base` (fields HA does not have keep their `base` value).
pub fn settings_from(l: &dyn Lookup, base: Settings) -> Settings {
    let mut s = base;
    apply_all(&settings_bindings(), l, &mut s);
    s
}

// --- memory --------------------------------------------------------------------------------

/// The hidden helpers and counters, as documented on each `Memory` field.
pub fn memory_bindings() -> Vec<Binding<Memory>> {
    macro_rules! num {
        ($e:literal, $f:ident) => {
            b($e, stringify!($f), |m, e| m.$f = num(e).unwrap_or(m.$f))
        };
    }
    macro_rules! flag {
        ($e:literal, $f:ident) => {
            b($e, stringify!($f), |m, e| m.$f = on(e).unwrap_or(m.$f))
        };
    }
    macro_rules! count {
        ($e:literal, $f:ident) => {
            b($e, stringify!($f), |m, e| m.$f = num(e).map(|c| c.max(0.0) as u64).unwrap_or(m.$f))
        };
    }
    vec![
        num!("input_number.batterij_setpoint_bias", batterij_setpoint_bias),
        flag!("input_boolean.batterij_net_laden", batterij_net_laden),
        flag!("input_boolean.soluna_absorptie_bezig", soluna_absorptie_bezig),
        flag!("input_boolean.ev_hoger_laadstil", ev_hoger_laadstil),
        num!("input_number.ev_hoger_laadstil_sinds", ev_hoger_laadstil_sinds),
        flag!("input_boolean.airco_dump_sessie_bezig", airco_dump_sessie_bezig),
        b("input_text.smartevse_plug_laatst_bekend", "smartevse_plug_laatst_bekend", |m, e| {
            if let Some(v) = text(e) {
                m.smartevse_plug_laatst_bekend = v.to_string();
            }
        }),
        b("input_text.smartevse_modus_laatst_bekend", "smartevse_modus_laatst_bekend", |m, e| {
            if let Some(v) = text(e) {
                m.smartevse_modus_laatst_bekend = v.to_string();
            }
        }),
        // The start script's `last_triggered` (ev_laden_cable retries a charged EV3 at most
        // every 30 min), kept with the automation timestamps.
        b(START_SCRIPT_ID, "last_triggered[script.laadkabel_start_laden]", |m, e| {
            if let Some(at) = e.and_then(|e| e.attr_str("last_triggered")).and_then(parse_time) {
                m.last_triggered.insert(START_SCRIPT_ID.to_string(), at);
            }
        }),
        num!("input_number.batterij_naar_auto_start_kwh", batterij_naar_auto_start_kwh),
        num!("input_number.nacht_import_startmeting", nacht_import_startmeting),
        num!("input_number.nacht_ontlaad_startmeting", nacht_ontlaad_startmeting),
        num!("input_number.nacht_ev_net_startmeting", nacht_ev_net_startmeting),
        num!("input_number.nachtdekking", nachtdekking),
        num!("input_number.nacht_batterij_kwh", nacht_batterij_kwh),
        num!("input_number.nacht_import_kwh", nacht_import_kwh),
        num!("input_number.nacht_duur_uren", nacht_duur_uren),
        b("input_text.capaciteit_piekhistoriek", "capaciteit_piekhistoriek", |m, e| {
            if let Some(v) = text(e) {
                m.capaciteit_piekhistoriek = v.to_string();
            }
        }),
        b("input_datetime.watchdog_laatste_herstart", "watchdog_laatste_herstart", |m, e| {
            m.watchdog_laatste_herstart = text(e).and_then(parse_time).or(m.watchdog_laatste_herstart)
        }),
        count!("counter.peak_guard_interventies", peak_guard_interventies),
        count!("counter.buzz_laadweigering", buzz_laadweigering),
        // A running timer reports when it finishes; an idle one has no next update.
        b("timer.steering_volgende_update", "steering_volgende_update", |m, e| {
            if let Some(e) = e {
                m.steering_volgende_update =
                    if e.state == "active" { e.attr_str("finishes_at").and_then(parse_time) } else { None };
            }
        }),
    ]
}

/// HA's memory on top of `base`. Automation `last_triggered` comes from the `automation.*`
/// entities (their `id` attribute is the automation id). Meters keep `base`'s state.
pub fn memory_from(l: &dyn Lookup, base: Memory) -> Memory {
    let mut m = base;
    apply_all(&memory_bindings(), l, &mut m);
    for e in l.all().filter(|e| e.entity_id.starts_with("automation.")) {
        let id = e.attr_str("id");
        let at = e.attr_str("last_triggered").and_then(parse_time);
        if let (Some(id), Some(at)) = (id, at) {
            m.last_triggered.insert(id.to_string(), at);
        }
    }
    m
}

// --- meters --------------------------------------------------------------------------------

/// Which Rust meter an HA meter entity is, and how it counts.
#[derive(Clone, Copy)]
pub enum MeterRef {
    Utility(UtilityConfig, fn(&mut MetersState) -> &mut UtilityMeter),
    Integral(IntegrationConfig, fn(&mut MetersState) -> &mut Integrator),
}

/// One `utility_meter` / `integration` helper and the HA entity that shows it.
#[derive(Clone, Copy)]
pub struct MeterBinding {
    pub entity: &'static str,
    pub meter: MeterRef,
}

impl MeterBinding {
    /// Rust's state of this meter at `now`, as other entities read it.
    pub fn reading(&self, st: &mut MetersState, now: Ts) -> Option<f64> {
        match self.meter {
            MeterRef::Utility(cfg, f) => f(st).reading(cfg, now),
            MeterRef::Integral(cfg, f) => f(st).reading(cfg),
        }
    }
}

fn um(entity: &'static str, cfg: UtilityConfig, f: fn(&mut MetersState) -> &mut UtilityMeter) -> MeterBinding {
    MeterBinding { entity, meter: MeterRef::Utility(cfg, f) }
}

fn ig(entity: &'static str, cfg: IntegrationConfig, f: fn(&mut MetersState) -> &mut Integrator) -> MeterBinding {
    MeterBinding { entity, meter: MeterRef::Integral(cfg, f) }
}

/// Every meter in `MetersState` except the P1 median (a 10 s sample window, not restorable).
pub fn meter_bindings() -> Vec<MeterBinding> {
    use energie_core::meters::integration::{ENERGIEKOST, EV, KWH_FROM_W};
    use energie_core::meters::utility::{DAG_ALTIJD, ENERGIEKOST_VANDAAG, EV_DAG, EV_MAAND, KWARTIER_IMPORT};
    vec![
        um("sensor.p1_meter_kwartier_import", KWARTIER_IMPORT, |m| &mut m.kwartier_import),
        ig("sensor.batterij_ontladen_energie", KWH_FROM_W, |m| &mut m.batterij_ontladen_energie),
        ig("sensor.batterij_geladen_energie", KWH_FROM_W, |m| &mut m.batterij_geladen_energie),
        ig("sensor.pv_energie_totaal", KWH_FROM_W, |m| &mut m.pv_energie_totaal),
        ig("sensor.huisverbruik_energie", KWH_FROM_W, |m| &mut m.huisverbruik_energie),
        ig("sensor.energiekost_cumulatief", ENERGIEKOST, |m| &mut m.energiekost_cumulatief),
        um("sensor.pv_energie_dag", DAG_ALTIJD, |m| &mut m.pv_energie_dag),
        um("sensor.huisverbruik_dag", DAG_ALTIJD, |m| &mut m.huisverbruik_dag),
        um("sensor.netimport_dag", DAG_ALTIJD, |m| &mut m.netimport_dag),
        um("sensor.injectie_dag", DAG_ALTIJD, |m| &mut m.injectie_dag),
        um("sensor.batterij_geladen_dag", DAG_ALTIJD, |m| &mut m.batterij_geladen_dag),
        um("sensor.batterij_ontladen_dag", DAG_ALTIJD, |m| &mut m.batterij_ontladen_dag),
        um("sensor.energiekost_vandaag", ENERGIEKOST_VANDAAG, |m| &mut m.energiekost_vandaag),
        ig("sensor.ev3_laadenergie_zon", EV, |m| &mut m.ev3.laadenergie_zon),
        ig("sensor.ev3_laadenergie_net", EV, |m| &mut m.ev3.laadenergie_net),
        ig("sensor.ev3_laadenergie_totaal", EV, |m| &mut m.ev3.laadenergie_totaal),
        ig("sensor.ev3_laadkost_net", EV, |m| &mut m.ev3.laadkost_net),
        um("sensor.ev3_laadkost_dag", EV_DAG, |m| &mut m.ev3.laadkost_dag),
        um("sensor.ev3_laadkost_maand", EV_MAAND, |m| &mut m.ev3.laadkost_maand),
        um("sensor.ev3_laadenergie_dag", EV_DAG, |m| &mut m.ev3.laadenergie_dag),
        um("sensor.ev3_laadenergie_maand", EV_MAAND, |m| &mut m.ev3.laadenergie_maand),
        ig("sensor.buzz_laadenergie_zon", EV, |m| &mut m.buzz.laadenergie_zon),
        ig("sensor.buzz_laadenergie_net", EV, |m| &mut m.buzz.laadenergie_net),
        ig("sensor.buzz_laadenergie_totaal", EV, |m| &mut m.buzz.laadenergie_totaal),
        ig("sensor.buzz_laadkost_net", EV, |m| &mut m.buzz.laadkost_net),
        um("sensor.buzz_laadkost_dag", EV_DAG, |m| &mut m.buzz.laadkost_dag),
        um("sensor.buzz_laadkost_maand", EV_MAAND, |m| &mut m.buzz.laadkost_maand),
        um("sensor.buzz_laadenergie_dag", EV_DAG, |m| &mut m.buzz.laadenergie_dag),
        um("sensor.buzz_laadenergie_maand", EV_MAAND, |m| &mut m.buzz.laadenergie_maand),
    ]
}

/// Utility meters whose source is one of Rust's own integrals: `(utility meter, integral)`.
/// When an integral's total is adopted from HA, these must take the new total as their previous
/// source reading, or the next tick counts the whole jump as consumption.
pub const FED_BY_INTEGRAL: [(&str, &str); 13] = [
    ("sensor.pv_energie_dag", "sensor.pv_energie_totaal"),
    ("sensor.huisverbruik_dag", "sensor.huisverbruik_energie"),
    ("sensor.batterij_geladen_dag", "sensor.batterij_geladen_energie"),
    ("sensor.batterij_ontladen_dag", "sensor.batterij_ontladen_energie"),
    ("sensor.energiekost_vandaag", "sensor.energiekost_cumulatief"),
    ("sensor.ev3_laadkost_dag", "sensor.ev3_laadkost_net"),
    ("sensor.ev3_laadkost_maand", "sensor.ev3_laadkost_net"),
    ("sensor.ev3_laadenergie_dag", "sensor.ev3_laadenergie_totaal"),
    ("sensor.ev3_laadenergie_maand", "sensor.ev3_laadenergie_totaal"),
    ("sensor.buzz_laadkost_dag", "sensor.buzz_laadkost_net"),
    ("sensor.buzz_laadkost_maand", "sensor.buzz_laadkost_net"),
    ("sensor.buzz_laadenergie_dag", "sensor.buzz_laadenergie_totaal"),
    ("sensor.buzz_laadenergie_maand", "sensor.buzz_laadenergie_totaal"),
];

/// After integrals in `adopted` took HA's totals, rebase the utility meters they feed.
fn rebase_fed_meters(st: &mut MetersState, adopted: &[&str]) {
    let bindings = meter_bindings();
    let find = |e: &str| bindings.iter().find(|b| b.entity == e).copied();
    for (um_entity, ig_entity) in FED_BY_INTEGRAL.iter() {
        if !adopted.contains(ig_entity) {
            continue;
        }
        if let (
            Some(MeterBinding { meter: MeterRef::Utility(_, uf), .. }),
            Some(MeterBinding { meter: MeterRef::Integral(icfg, igf), .. }),
        ) = (find(um_entity), find(ig_entity))
        {
            let src = igf(st).reading(icfg);
            uf(st).last_source = src;
        }
    }
}

/// A utility meter's `last_reset` attribute, else the start of the cycle containing `now`.
fn last_reset(e: &EntityState, cfg: UtilityConfig, now: Ts) -> Ts {
    e.attr_str("last_reset").and_then(parse_time).unwrap_or_else(|| cfg.cycle.period_start(now))
}

/// Seed one meter from HA's entity, the way HA restores it at start: a utility meter gets its
/// state, `last_reset` and (when HA exposes it) `last_valid_state` as the previous source
/// reading; an integral gets its total and integrates from the next source event on.
/// Returns false (and leaves the meter alone) when HA has no number for it.
pub fn seed_meter(b: &MeterBinding, st: &mut MetersState, e: Option<&EntityState>, now: Ts) -> bool {
    let Some((e, v)) = e.and_then(|e| e.num().map(|v| (e, v))) else { return false };
    match b.meter {
        MeterRef::Utility(cfg, f) => {
            let mut m = UtilityMeter::seeded(v, Some(last_reset(e, cfg, now)));
            m.last_source = e.attr_num("last_valid_state");
            *f(st) = m;
        }
        MeterRef::Integral(_, f) => *f(st) = Integrator::seeded(v),
    }
    true
}

/// Every meter as HA shows it at `now` (first start, or after an HA restart). Meters HA has
/// no number for start empty, as they would in a fresh HA.
pub fn seed_meters(l: &dyn Lookup, now: Ts) -> MetersState {
    let mut st = MetersState::default();
    for b in meter_bindings() {
        seed_meter(&b, &mut st, l.get(b.entity), now);
    }
    st
}

/// A meter Rust re-adopted from HA because they had drifted apart.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MeterAdoption {
    pub at: String,
    pub entity: &'static str,
    pub rust: Option<f64>,
    pub ha: f64,
}

/// Divergence that counts: `|rust − ha| > max(ABS, REL·|ha|)`. Above the shadow tolerance of
/// the meter fields (0.01 + 0.1 %), and above what HA's own update lag can explain (an
/// integral is written at least every `max_sub_interval`, 1 min: 0.08 kWh at 5 kW).
pub const METER_ADOPT_ABS: f64 = 0.1;
pub const METER_ADOPT_REL: f64 = 0.002;
/// ... held this long, so a midnight or quarter reset seen a tick apart never counts.
pub const METER_ADOPT_HOLD_S: f64 = 120.0;

/// Shadow lockstep for meters: when a Rust meter and HA's entity disagree by more than
/// [`METER_ADOPT_ABS`]/[`METER_ADOPT_REL`] for [`METER_ADOPT_HOLD_S`], take HA's value (an
/// HA restart that lost or restored energy, a manual reset, accumulated drift) and count it.
/// The adoption keeps the meter's source chain, so the next delta still counts.
#[derive(Debug, Clone, Default)]
pub struct MeterAdopter {
    since: BTreeMap<&'static str, Ts>,
    pub adoptions: u64,
    pub last: Option<MeterAdoption>,
}

impl MeterAdopter {
    pub fn adopt(&mut self, st: &mut MetersState, l: &dyn Lookup, now: Ts) -> Vec<MeterAdoption> {
        let mut out = Vec::new();
        for b in meter_bindings() {
            let e = l.get(b.entity);
            let Some((e, ha)) = e.and_then(|e| e.num().map(|v| (e, v))) else {
                self.since.remove(b.entity);
                continue;
            };
            let rust = b.reading(st, now);
            let lim = METER_ADOPT_ABS.max(METER_ADOPT_REL * ha.abs());
            if rust.is_some_and(|r| (r - ha).abs() <= lim) {
                self.since.remove(b.entity);
                continue;
            }
            let since = *self.since.entry(b.entity).or_insert(now);
            if energie_core::time::secs(since, now) < METER_ADOPT_HOLD_S {
                continue;
            }
            self.since.remove(b.entity);
            match b.meter {
                MeterRef::Utility(cfg, f) => {
                    let m = f(st);
                    m.value = Some(ha);
                    m.available = true;
                    m.last_reset = Some(last_reset(e, cfg, now));
                }
                MeterRef::Integral(_, f) => f(st).total = Some(ha),
            }
            out.push(MeterAdoption { at: now.to_rfc3339(), entity: b.entity, rust, ha });
        }
        let adopted: Vec<&str> = out.iter().map(|a| a.entity).collect();
        rebase_fed_meters(st, &adopted);
        self.adoptions += out.len() as u64;
        if let Some(a) = out.last() {
            self.last = Some(a.clone());
        }
        out
    }
}

/// Every entity the bridge reads.
pub fn input_entities() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = world_bindings().iter().map(|b| b.entity).collect();
    v.extend(settings_bindings().iter().map(|b| b.entity));
    v.extend(memory_bindings().iter().map(|b| b.entity));
    v.sort_unstable();
    v.dedup();
    v
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ha::entity::{EntityState, index};
    use energie_core::settings::Consumer;
    use serde_json::json;
    use std::collections::HashMap;

    /// The real 2026-11-07 snapshot (energy entities only).
    pub(crate) fn fixture() -> HashMap<String, EntityState> {
        let raw = include_str!("../../tests/fixtures/ha-states-2026-11-07.json");
        index(serde_json::from_str(raw).unwrap())
    }

    /// The live snapshot reads into the World values HA showed at 13:46 on 2026-11-07.
    #[test]
    fn world_from_the_live_snapshot() {
        let w = world_from(&fixture());
        assert_eq!(w.p1.power_w, Some(3066.0));
        assert_eq!(w.p1.phase_a[0], Some(-1.4));
        assert_eq!(w.p1.import_kwh, Some(11884.497));
        assert_eq!(w.p1.month_peak_w, Some(3892.0));
        assert_eq!(w.solis.soc, Some(100.0));
        assert_eq!(w.solis.rc_mode, Some(RcMode::ForceCharge));
        assert_eq!(w.solis.rc_timeout_min, Some(10.0));
        assert_eq!(w.solis.allow_grid_charge, Some(false));
        assert_eq!(w.solis.feed_in_limit_on, Some(true));
        assert_eq!(w.solis.backflow_w, Some(5500.0));
        assert_eq!(w.omnik.power_w, Some(306.0));
        assert_eq!(w.evse.plug.as_deref(), Some("Connected"));
        assert_eq!(w.evse.state.as_deref(), Some("Charging"));
        assert_eq!(w.evse.state_id.as_deref(), Some("C"));
        assert_eq!(w.evse.mode, Some(EvseMode::Normal));
        assert_eq!(w.evse.override_a, Some(14.8));
        assert_eq!(w.evse.override_max_a, Some(29.0));
        // The cable was offline: everything None except the restored `max` attribute.
        assert_eq!(w.cable.status, None);
        assert_eq!(w.cable.set_current_a, None);
        assert_eq!(w.cable.set_current_max_a, Some(16.0));
        assert_eq!(w.ev3.plugged, Some(false));
        assert_eq!(w.ev3.soc, Some(54.5));
        assert_eq!(w.buzz.soc, Some(43.0));
        assert_eq!(w.prices.afname_now, Some(0.37692));
        assert_eq!(w.prices.afname_slots.len(), 96);
        assert_eq!(w.prices.afname_slots[1].start.to_rfc3339(), "2026-11-07T00:15:00+01:00");
        assert_eq!(w.prices.afname_slots[0].price, 0.41417);
        assert!(!w.prices.afname_has_tomorrow);
        assert!(!w.prices.injectie_has_tomorrow);
        assert_eq!(w.prices.backup_now, Some(0.37692));
        assert_eq!(w.airco.beneden_on, Some(false));
        assert_eq!(w.vriezer.power_w, None);
        assert_eq!(w.fluvius.peak_kw, Some(3.892));
    }

    /// `input_number.prijs_backup_nu = -1` means "no backup price".
    #[test]
    fn backup_price_minus_one_is_none() {
        let m = index(vec![EntityState::new("input_number.prijs_backup_nu", "-1.0")]);
        assert_eq!(world_from(&m).prices.backup_now, None);
        let m = index(vec![EntityState::new("input_number.prijs_backup_nu", "-0.05")]);
        assert_eq!(world_from(&m).prices.backup_now, Some(-0.05));
    }

    /// Tomorrow's prices count only when the list is non-empty; bad slots are skipped.
    #[test]
    fn price_attributes() {
        let e = EntityState::new("sensor.jeroen_average_electricity_price", "0.4")
            .with_attr(
                "prices",
                json!([{"time": "2026-11-08 00:00:00+01:00", "price": 0.3}, {"time": "x", "price": 1}, {"price": 2}]),
            )
            .with_attr("prices_tomorrow", json!([{"time": "2026-11-08 00:00:00+01:00", "price": 0.3}]));
        let w = world_from(&index(vec![e]));
        assert_eq!(w.prices.afname_slots.len(), 1);
        assert!(w.prices.afname_has_tomorrow);
        assert!(parse_slots(Some(&json!("nope"))).is_empty());
    }

    /// Select options parse into enums and back.
    #[test]
    fn enum_labels_roundtrip() {
        for m in [RcMode::None, RcMode::ForceCharge, RcMode::ForceDischarge] {
            assert_eq!(parse_rc_mode(rc_mode_label(m)), Some(m));
        }
        for m in [EvseMode::Off, EvseMode::Normal, EvseMode::Smart, EvseMode::Solar, EvseMode::Pause] {
            assert_eq!(parse_evse_mode(evse_mode_label(m)), Some(m));
        }
        assert_eq!(parse_rc_mode("Turbo"), None);
        assert_eq!(parse_evse_mode("Turbo"), None);
    }

    /// The live helpers seed the Settings the dashboard shows.
    #[test]
    fn settings_from_the_live_snapshot() {
        let s = settings_from(&fixture(), Settings::default());
        assert_eq!(s.zonprioriteit, Zonprioriteit([Consumer::Buzz, Consumer::Batterij, Consumer::Ev3]));
        assert_eq!(s.ev_net_niveau, NetNiveau::MaxTotPiek);
        assert_eq!(s.ev_batterij_niveau, BatterijNiveau::Uit);
        assert_eq!(s.goedkoop_laden_prijs_drempel, 0.38);
        assert!(s.batterij_reservering_actief);
        assert_eq!(s.batterij_reservering_w, 700.0);
        assert!(!s.airco_overschot_koeling_actief);
        assert_eq!(s.ev3_odometer_start, 1967.2);
    }

    /// Missing or unparseable helpers keep the previous value instead of resetting it.
    #[test]
    fn settings_keep_base_values_for_missing_or_bad_helpers() {
        let base = Settings { batterij_reserve_soc: 42.0, ev_net_niveau: NetNiveau::Uit, ..Default::default() };
        let m = index(vec![
            EntityState::new("input_select.ev_net_niveau", "Volle kracht"),
            EntityState::new("input_boolean.peak_guard_actief", "unavailable"),
            EntityState::new("input_number.goedkoop_laden_prijs_drempel", "0.25"),
        ]);
        let s = settings_from(&m, base.clone());
        assert_eq!(s.batterij_reserve_soc, 42.0);
        assert_eq!(s.ev_net_niveau, NetNiveau::Uit);
        assert!(s.peak_guard_actief);
        assert_eq!(s.goedkoop_laden_prijs_drempel, 0.25);
    }

    /// The live hidden helpers, counters and automation timestamps seed Memory.
    #[test]
    fn memory_from_the_live_snapshot() {
        let m = memory_from(&fixture(), Memory::default());
        assert_eq!(m.batterij_setpoint_bias, 200.0);
        assert!(!m.batterij_net_laden);
        assert_eq!(m.smartevse_plug_laatst_bekend, "Connected");
        assert_eq!(m.batterij_naar_auto_start_kwh, 549.546);
        assert_eq!(m.nacht_ontlaad_startmeting, 749.95);
        assert!(m.capaciteit_piekhistoriek.starts_with("2025-11:6.572,"));
        assert_eq!(m.watchdog_laatste_herstart.unwrap().to_rfc3339(), "2026-10-30T13:32:52+01:00");
        assert_eq!(m.peak_guard_interventies, 2880);
        assert_eq!(m.buzz_laadweigering, 0);
        assert_eq!(m.steering_volgende_update.unwrap().to_rfc3339(), "2026-11-07T13:46:30+01:00");
        assert_eq!(m.last_triggered("soluna_dispatcher").unwrap().to_rfc3339(), "2026-11-07T13:46:00.452091+01:00");
        assert_eq!(m.last_triggered.len(), 29, "{:?}", m.last_triggered.keys());
    }

    /// An idle timer clears the next-update time; missing text helpers keep theirs; an
    /// automation that never ran contributes no timestamp.
    #[test]
    fn memory_edge_cases() {
        let base = Memory {
            smartevse_plug_laatst_bekend: "Disconnected".into(),
            capaciteit_piekhistoriek: "2026-10:5.0".into(),
            steering_volgende_update: Some(parse_time("2026-11-07T12:00:00Z").unwrap()),
            ..Default::default()
        };
        let m = index(vec![
            EntityState::new("timer.steering_volgende_update", "idle"),
            EntityState::new("input_text.smartevse_plug_laatst_bekend", "unknown"),
            EntityState::new("automation.x", "on").with_attr("id", json!("x")),
            EntityState::new("counter.buzz_laadweigering", "3"),
        ]);
        let out = memory_from(&m, base);
        assert_eq!(out.steering_volgende_update, None);
        assert_eq!(out.smartevse_plug_laatst_bekend, "Disconnected");
        assert_eq!(out.capaciteit_piekhistoriek, "2026-10:5.0");
        assert!(out.last_triggered.is_empty());
        assert_eq!(out.buzz_laadweigering, 3);
        let none = memory_from(&index(vec![]), Memory::default());
        assert_eq!(none, Memory::default());
    }

    /// The 2026-11-07 additions: `input_text.smartevse_modus_laatst_bekend` seeds the mode
    /// stamp's last valid mode (an unavailable helper keeps the base value), and the start
    /// script's `last_triggered` lands under its `script.` key; a script that never ran adds
    /// nothing.
    #[test]
    fn memory_seeds_the_mode_helper_and_the_cable_start_script() {
        let base = Memory { smartevse_modus_laatst_bekend: "Pause".into(), ..Default::default() };
        let m = index(vec![
            EntityState::new("input_text.smartevse_modus_laatst_bekend", "Normal"),
            EntityState::new("script.laadkabel_start_laden", "off")
                .with_attr("last_triggered", json!("2026-11-07T13:40:00.5+00:00")),
        ]);
        let out = memory_from(&m, base.clone());
        assert_eq!(out.smartevse_modus_laatst_bekend, "Normal");
        assert_eq!(out.last_triggered(START_SCRIPT_ID).unwrap().to_rfc3339(), "2026-11-07T14:40:00.500+01:00");
        let m = index(vec![
            EntityState::new("input_text.smartevse_modus_laatst_bekend", "unavailable"),
            EntityState::new("script.laadkabel_start_laden", "off").with_attr("last_triggered", json!(null)),
        ]);
        let out = memory_from(&m, base);
        assert_eq!(out.smartevse_modus_laatst_bekend, "Pause");
        assert_eq!(out.last_triggered(START_SCRIPT_ID), None);
    }

    /// Every entity the tables read exists in the live HA (the vriezer plug is known absent).
    #[test]
    fn every_input_entity_exists_in_the_live_snapshot() {
        let f = fixture();
        let missing: Vec<_> = input_entities()
            .into_iter()
            .filter(|e| !f.contains_key(*e))
            .filter(|e| !matches!(*e, "sensor.vriezer_vermogen" | "switch.vriezer"))
            // Created by the 2026-11-07 fixes, after this snapshot was taken; the script's
            // state is not in the energy-only export.
            .filter(|e| !matches!(*e, "input_text.smartevse_modus_laatst_bekend" | "script.laadkabel_start_laden"))
            .collect();
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// Every Settings field is fed by exactly one HA helper.
    #[test]
    fn every_settings_field_has_a_binding() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        let mut fields: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        let mut bound: Vec<_> = settings_bindings().iter().map(|b| b.field.to_string()).collect();
        fields.sort();
        bound.sort();
        assert_eq!(fields, bound);
    }

    fn t0() -> Ts {
        crate::plug::tests::t0()
    }

    /// The live snapshot seeds every meter with HA's value: the kwartier meter with its
    /// `last_reset` and `last_valid_state` (13:45 quarter, P1 at 11884.497), the monthly EV
    /// meters with the month start, the integrals with their totals.
    #[test]
    fn seed_meters_from_the_live_snapshot() {
        let f = fixture();
        let mut st = seed_meters(&f, t0());
        assert_eq!(st.kwartier_import.value, Some(0.066));
        assert_eq!(st.kwartier_import.last_reset.unwrap().to_rfc3339(), "2026-11-07T13:45:00.005532+01:00");
        assert_eq!(st.kwartier_import.last_source, Some(11884.497));
        assert_eq!(st.batterij_ontladen_energie.total, Some(750.288));
        assert_eq!(st.batterij_ontladen_energie.last_event, None, "integrates from the next event");
        assert_eq!(st.ev3.laadkost_maand.last_reset.unwrap().to_rfc3339(), "2026-11-01T00:00:00.001614+01:00");
        assert_eq!(st.netimport_dag.value, Some(20.523));
        for b in meter_bindings() {
            let ha = f.get(b.entity).and_then(EntityState::num);
            assert!(ha.is_some(), "{} missing in the live snapshot", b.entity);
            assert_eq!(b.reading(&mut st, t0()), ha, "{}", b.entity);
        }
        assert_eq!(meter_bindings().len(), 29);
    }

    /// No number, no seed; a utility meter without `last_reset` starts at its cycle start.
    #[test]
    fn seed_meter_edge_cases() {
        let b = meter_bindings().into_iter().find(|b| b.entity == "sensor.netimport_dag").unwrap();
        let mut st = MetersState::default();
        assert!(!seed_meter(&b, &mut st, None, t0()));
        assert!(!seed_meter(&b, &mut st, Some(&EntityState::new(b.entity, "unavailable")), t0()));
        assert_eq!(st, MetersState::default());
        assert!(seed_meter(&b, &mut st, Some(&EntityState::new(b.entity, "3.5")), t0()));
        assert_eq!(st.netimport_dag.last_reset.unwrap().to_rfc3339(), "2026-11-07T00:00:00+01:00");
        assert_eq!(st.netimport_dag.last_source, None);
        let empty = seed_meters(&index(vec![]), t0());
        assert_eq!(empty, MetersState::default());
    }

    /// A meter is re-adopted only after it disagrees by more than max(0.1, 0.2 %) for 120 s:
    /// 100.05 vs 100 never counts; 105 counts at the 120th second, once; a disagreement that
    /// heals inside the hold starts over.
    #[test]
    fn meter_adopter_takes_ha_after_a_held_divergence() {
        let ent = "sensor.batterij_ontladen_energie";
        let mut st = MetersState { batterij_ontladen_energie: Integrator::seeded(100.0), ..Default::default() };
        let mut a = MeterAdopter::default();
        let ha = |v: &str| index(vec![EntityState::new(ent, v)]);
        let at = |s: i64| t0() + chrono::Duration::seconds(s);
        assert!(a.adopt(&mut st, &ha("100.05"), at(0)).is_empty());
        assert!(a.adopt(&mut st, &ha("105"), at(1)).is_empty());
        assert!(a.adopt(&mut st, &ha("100"), at(2)).is_empty(), "healed: the hold restarts");
        assert!(a.adopt(&mut st, &ha("105"), at(10)).is_empty());
        assert!(a.adopt(&mut st, &ha("105"), at(129)).is_empty());
        let got = a.adopt(&mut st, &ha("105"), at(130));
        assert_eq!(got, vec![MeterAdoption { at: at(130).to_rfc3339(), entity: ent, rust: Some(100.0), ha: 105.0 }]);
        assert_eq!(st.batterij_ontladen_energie.total, Some(105.0));
        assert_eq!((a.adoptions, a.last.as_ref().map(|l| l.ha)), (1, Some(105.0)));
        assert!(a.adopt(&mut st, &ha("105"), at(400)).is_empty(), "agrees now");
        // HA without a number clears the hold.
        assert!(a.adopt(&mut st, &ha("200"), at(500)).is_empty());
        assert!(a.adopt(&mut st, &ha("unavailable"), at(600)).is_empty());
        assert!(a.adopt(&mut st, &ha("200"), at(700)).is_empty());
        assert_eq!(a.adoptions, 1);
    }

    /// Adopting an integral's total rebases the daily meters it feeds: otherwise the next tick
    /// sees the source jump from Rust's old total to HA's and counts it all (seen live on
    /// 2026-11-07: `batterij_geladen_dag` 626 kWh right after an adoption).
    #[test]
    fn adopting_an_integral_rebases_the_meters_it_feeds() {
        let mut st = MetersState {
            batterij_geladen_energie: Integrator::seeded(0.6),
            batterij_geladen_dag: UtilityMeter::seeded(0.6, None),
            ..Default::default()
        };
        st.batterij_geladen_dag.last_source = Some(0.6);
        let ha = index(vec![
            EntityState::new("sensor.batterij_geladen_energie", "621.762"),
            EntityState::new("sensor.batterij_geladen_dag", "4.684"),
        ]);
        let mut a = MeterAdopter::default();
        a.adopt(&mut st, &ha, t0());
        let got = a.adopt(&mut st, &ha, t0() + chrono::Duration::seconds(120));
        assert_eq!(got.len(), 2);
        assert_eq!(st.batterij_geladen_dag.value, Some(4.684));
        assert_eq!(st.batterij_geladen_dag.last_source, Some(621.762), "the jump is not consumption");
        // Every pair in the table names a utility meter fed by an integral.
        for (u, i) in FED_BY_INTEGRAL.iter() {
            let b = meter_bindings();
            assert!(matches!(b.iter().find(|x| x.entity == *u).unwrap().meter, MeterRef::Utility(..)), "{u}");
            assert!(matches!(b.iter().find(|x| x.entity == *i).unwrap().meter, MeterRef::Integral(..)), "{i}");
        }
    }

    /// An unavailable Rust utility meter that HA shows is adopted with HA's `last_reset`, and
    /// keeps its source chain (the next P1 delta still counts).
    #[test]
    fn meter_adopter_restores_a_utility_meter_keeping_its_source() {
        let ent = "sensor.p1_meter_kwartier_import";
        let mut st = MetersState::default();
        st.kwartier_import.available = false;
        st.kwartier_import.last_source = Some(11884.0);
        let ha = index(vec![EntityState::new(ent, "0.25").with_attr("last_reset", json!("2026-11-07T12:45:00+00:00"))]);
        let mut a = MeterAdopter::default();
        assert!(a.adopt(&mut st, &ha, t0()).is_empty());
        let got = a.adopt(&mut st, &ha, t0() + chrono::Duration::seconds(120));
        assert_eq!(got[0].rust, None);
        let m = &st.kwartier_import;
        assert_eq!((m.value, m.available, m.last_source), (Some(0.25), true, Some(11884.0)));
        assert_eq!(m.last_reset.unwrap().to_rfc3339(), "2026-11-07T13:45:00+01:00");
    }
}
