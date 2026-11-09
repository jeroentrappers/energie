//! The HA template sensors, recomputed every tick from the world, the settings and memory.
//!
//! Values are in the unit of the HA *state* (what other templates read), which is not always
//! the unit the template computes: `sensor.kwartiervermogen_projectie` renders kW but HA
//! stores it as W. Field suffixes name the stored unit.
//!
//! `None` means the HA sensor would be `unavailable`/`unknown`.

use chrono::{Datelike, Timelike};
use serde::{Deserialize, Serialize};

use crate::memory::Memory;
use crate::render::{Rendered, Renders};
use crate::settings::{BatterijNiveau, Settings};
use crate::time::{self, Ts};
use crate::world::World;
use crate::{cascade, jinja, meters, sun};

/// `sensor.ev_actieve_laadauto`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ActieveLaadauto {
    Ev3,
    #[default]
    Buzz,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Derived {
    // --- sun -----------------------------------------------------------------------------
    /// `is_state('sun.sun', 'above_horizon')`.
    pub sun_above_horizon: bool,

    // --- meters (utility_meter / integration / statistics), read back from memory ---------
    /// `sensor.p1_meter_kwartier_import`, kWh this quarter (utility_meter, quarter-hourly).
    pub kwartier_import_kwh: Option<f64>,
    /// `state_attr('sensor.p1_meter_kwartier_import', 'next_reset')`.
    #[serde(with = "crate::time::serde_opt")]
    pub kwartier_next_reset: Option<Ts>,
    /// `sensor.p1_meter_p1_mediaan_90s`, W (statistics: median, 10 s max age, 60 samples).
    pub p1_mediaan_w: Option<f64>,
    /// `sensor.batterij_ontladen_energie`, kWh (integration of the discharge power).
    pub batterij_ontladen_energie_kwh: Option<f64>,
    /// `sensor.batterij_geladen_energie`, kWh.
    pub batterij_geladen_energie_kwh: Option<f64>,
    /// `sensor.pv_energie_totaal`, kWh.
    pub pv_energie_totaal_kwh: Option<f64>,
    /// `sensor.huisverbruik_energie`, kWh.
    pub huisverbruik_energie_kwh: Option<f64>,
    /// `sensor.energiekost_cumulatief`, EUR.
    pub energiekost_cumulatief_eur: Option<f64>,
    /// `sensor.ev3_laadenergie_zon` / `_net` / `_totaal`, kWh; `sensor.ev3_laadkost_net`, EUR.
    pub ev3_laadenergie_zon_kwh: Option<f64>,
    pub ev3_laadenergie_net_kwh: Option<f64>,
    pub ev3_laadenergie_totaal_kwh: Option<f64>,
    pub ev3_laadkost_net_eur: Option<f64>,
    /// Same four for the Buzz.
    pub buzz_laadenergie_zon_kwh: Option<f64>,
    pub buzz_laadenergie_net_kwh: Option<f64>,
    pub buzz_laadenergie_totaal_kwh: Option<f64>,
    pub buzz_laadkost_net_eur: Option<f64>,
    /// Daily utility meters.
    pub pv_energie_dag_kwh: Option<f64>,
    pub huisverbruik_dag_kwh: Option<f64>,
    pub netimport_dag_kwh: Option<f64>,
    pub injectie_dag_kwh: Option<f64>,
    pub batterij_geladen_dag_kwh: Option<f64>,
    pub batterij_ontladen_dag_kwh: Option<f64>,
    pub energiekost_vandaag_eur: Option<f64>,
    pub ev3_laadkost_dag_eur: Option<f64>,
    pub ev3_laadkost_maand_eur: Option<f64>,
    pub ev3_laadenergie_dag_kwh: Option<f64>,
    pub ev3_laadenergie_maand_kwh: Option<f64>,
    pub buzz_laadkost_dag_eur: Option<f64>,
    pub buzz_laadkost_maand_eur: Option<f64>,
    pub buzz_laadenergie_dag_kwh: Option<f64>,
    pub buzz_laadenergie_maand_kwh: Option<f64>,

    // --- template sensors -----------------------------------------------------------------
    pub kwartiervermogen_vastgelegd_kw: f64,
    /// Stored in W (template renders kW, rounded to 3 decimals, HA converts).
    pub kwartiervermogen_projectie_w: f64,
    pub pv_vermogen_totaal_w: f64,
    pub huisverbruik_vermogen_w: f64,
    pub batterij_beschikbare_energie_kwh: f64,
    /// `goedkoop` / `normaal` / `duur`.
    pub stroomprijs_niveau: String,
    /// `binary_sensor.capaciteitspiek_risico`.
    pub capaciteitspiek_risico: bool,
    pub kwartierprijs_volgend: Option<f64>,
    pub vaste_kosten_per_maand_eur: f64,
    pub capaciteitstarief_deze_maand_eur: f64,
    pub pv_overschot_beschikbaar_w: f64,
    pub huisverbruik_excl_stuurbaar_w: f64,
    pub piek_verschil_fluvius_vs_p1_w: f64,
    pub capaciteit_maandtarget_kw: f64,
    pub pv_bruto_overschot_w: f64,
    pub kost_per_uur_eur: f64,
    /// `binary_sensor.solis_offline`.
    pub solis_offline: bool,
    pub ev_actieve_laadauto: ActieveLaadauto,
    pub ev_laadvermogen_zon_kw: f64,
    pub ev_laadvermogen_net_kw: f64,
    pub ev3_laadvermogen_zon_kw: f64,
    pub ev3_laadvermogen_net_kw: f64,
    pub ev3_laadvermogen_totaal_kw: f64,
    pub ev3_laadkost_per_uur_eur: f64,
    pub ev3_laad_zonaandeel_pct: f64,
    pub ev3_laadkost_per_100km_eur: f64,
    pub ev3_verbruik_per_100km_kwh: f64,
    pub ev3_ice_kost_per_100km_eur: f64,
    pub ev3_besparing_per_100km_eur: f64,
    pub buzz_laadvermogen_zon_kw: f64,
    pub buzz_laadvermogen_net_kw: f64,
    pub buzz_laadvermogen_totaal_kw: f64,
    pub buzz_laadkost_per_uur_eur: f64,
    pub buzz_laad_zonaandeel_pct: f64,
    pub buzz_laadkost_per_100km_eur: f64,
    pub buzz_verbruik_per_100km_kwh: f64,
    pub buzz_ice_kost_per_100km_eur: f64,
    pub buzz_besparing_per_100km_eur: f64,
    pub ev_laadkabel_vermogen_w: f64,
    pub ev_laadvermogen_smartevse_kw: f64,
    pub ev_laadvermogen_smartevse_w: f64,
    pub injectie_effectieve_terugleververgoeding: Option<f64>,
    pub batterij_naar_auto_geleverd_kwh: f64,
    pub capaciteitspiek_meerkost_eur: f64,
    pub batterij_ontlaadvermogen_w: f64,
    pub batterij_laadvermogen_w: f64,
    pub zelfverbruik_vandaag_pct: f64,
    pub zelfvoorziening_vandaag_pct: f64,
    pub zelfverbruik_vandaag_kwh: f64,
    /// `sensor.kwartier_toegestaan_vermogen`; `None` until the quarter meter has a value.
    pub kwartier_toegestaan_vermogen_w: Option<f64>,
    pub kwartier_gemiddelde_w: f64,
    pub kwartier_index: u32,
    pub batterij_reservering_effectief_w: f64,
    /// `sensor.stroomprijs_nu`, EUR/kWh; `None` = `unavailable`.
    pub stroomprijs_nu: Option<f64>,
    pub gefactureerde_piek_kw: f64,
    pub capaciteitstarief_factuur_deze_maand_eur: f64,

    // --- battery brain (filled by `battery`, after the rest) ------------------------------
    /// `sensor.batterij_setpoint`, W, + discharge / − charge.
    pub batterij_setpoint_w: f64,
    /// `binary_sensor.dispatcher_off_target`.
    pub dispatcher_off_target: bool,

    // --- appended -------------------------------------------------------------------------
    /// `has_value('sensor.ev_laadvermogen_smartevse')` (and `_w`, which shares it): the
    /// availability template `has_value('sensor.smartevse_1234_state') and (not
    /// is_state('sensor.smartevse_1234_state','Charging') or
    /// has_value('sensor.smartevse_1234_charge_current'))`. While `false` the two power fields
    /// hold 0, which is what every `|float(0)` reader sees.
    pub ev_laadvermogen_smartevse_beschikbaar: bool,
}

/// `rem` of the quarter templates: seconds to `next_reset`, or from the wall clock when the
/// quarter meter is unavailable (its attributes are gone):
/// `(as_timestamp(nr) - as_timestamp(now())) if nr is not none else (900 - (now().minute % 15)
/// * 60 - now().second)`.
fn quarter_rem(now: Ts, next_reset: Option<Ts>) -> f64 {
    match next_reset {
        Some(nr) => time::secs(now, nr),
        None => f64::from(900 - (now.minute() % 15) * 60 - now.second()),
    }
}

/// Jinja `x|int` on a string: Python `int(s)`, else `int(float(s))`, else 0.
fn str_int(s: &str) -> i64 {
    let s = s.trim();
    s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().filter(|f| f.is_finite()).map(jinja::int)).unwrap_or(0)
}

/// Python slice `s[a:b]` on characters.
fn slice(s: &str, a: usize, b: usize) -> String {
    s.chars().skip(a).take(b - a).collect()
}

/// `sensor.gefactureerde_piek`: the rolling 12-month average of the month peaks (min 2.5 kW)
/// over the 11 previous months in `input_text.capaciteit_piekhistoriek` plus this month.
fn gefactureerde_piek(now: Ts, historiek: &str, deze_maand_w: f64) -> f64 {
    let nu = i64::from(now.year()) * 12 + i64::from(now.month()) - 1;
    let (mut tot, mut n) = (0.0, 0u32);
    for e in historiek.split(',').filter(|e| e.contains(':')) {
        // `{% set k, v = e.split(':', 1) %}`: the value is everything after the first colon (a
        // second colon makes it unparseable, so it reads 2.5).
        let (k, v) = e.split_once(':').expect("filtered on ':'");
        let age = nu - (str_int(&slice(k, 0, 4)) * 12 + str_int(&slice(k, 5, 7)) - 1);
        if (1..=11).contains(&age) {
            tot += jinja::max2(2.5, v.trim().parse::<f64>().unwrap_or(2.5));
            n += 1;
        }
    }
    let deze = jinja::max2(2.5, deze_maand_w / 1000.0);
    jinja::round((tot + deze) / f64::from(n + 1), 3)
}

/// `sensor.zelfvoorziening_vandaag`, %: own solar used over everything taken in. `eigen =
/// max(0, pv_energie_dag − injectie_dag)`, `use = eigen + netimport_dag`; 0 below 0.1 kWh, else
/// `eigen / use × 100` clamped to 0–100 and rounded.
fn zelfvoorziening(pv_dag: Option<f64>, injectie_dag: Option<f64>, netimport_dag: Option<f64>) -> f64 {
    let eigen = jinja::max2(0.0, pv_dag.unwrap_or(0.0) - injectie_dag.unwrap_or(0.0));
    let usage = eigen + netimport_dag.unwrap_or(0.0);
    if usage < 0.1 { 0.0 } else { jinja::round(jinja::clamp(eigen / usage * 100.0, 0.0, 100.0), 0) }
}

/// Last day of `now`'s month: `(now().replace(day=28) + timedelta(days=4)).replace(day=1) -
/// timedelta(days=1)`.
fn days_in_month(now: Ts) -> u32 {
    let d = now.date_naive().with_day(28).expect("day 28 exists") + chrono::Duration::days(4);
    (d.with_day(1).expect("day 1 exists") - chrono::Duration::days(1)).day()
}

/// Recompute every template sensor except the battery brain (see [`crate::battery`]).
/// Meter values are read from `m.meters`; [`crate::meters::update`] advances them afterwards.
/// Templates that read each other are evaluated in dependency order.
pub fn compute(now: Ts, w: &World, s: &Settings, m: &Memory) -> Derived {
    compute_with(now, w, s, m).0
}

/// [`compute`], and the render bookkeeping of the `now()` templates after this tick
/// ([`crate::render`]): those keep their last render (from `m.template_renders`) until an input
/// changes or a minute passes.
pub fn compute_with(now: Ts, w: &World, s: &Settings, m: &Memory) -> (Derived, Renders) {
    use jinja::{max2, min2, round};
    let mut renders = m.template_renders.clone();
    use meters::{integration as ic, utility as uc};

    let mt = &m.meters;
    let p1 = w.p1.power_w.unwrap_or(0.0);
    let batt = cascade::batt_w(w);
    let peak_w = w.p1.month_peak_w.unwrap_or(0.0);
    let cable_a = w.cable.current_a.unwrap_or(0.0);
    let cable_v = w.cable.voltage_v.unwrap_or(230.0);
    let sun = sun::above_horizon(now);

    // --- meters, as other templates read them ----------------------------------------------
    let kwartier_import_kwh = mt.kwartier_import.reading(uc::KWARTIER_IMPORT, now);
    let kwartier_next_reset = mt.kwartier_import.available.then(|| time::next_quarter(now));
    let ev = |c: &meters::CarMeters| {
        (
            c.laadenergie_zon.reading(ic::EV),
            c.laadenergie_net.reading(ic::EV),
            c.laadenergie_totaal.reading(ic::EV),
            c.laadkost_net.reading(ic::EV),
        )
    };
    let (ev3_zon_kwh, ev3_net_kwh, ev3_tot_kwh, ev3_kost_eur) = ev(&mt.ev3);
    let (buzz_zon_kwh, buzz_net_kwh, buzz_tot_kwh, buzz_kost_eur) = ev(&mt.buzz);
    let batterij_ontladen_energie_kwh = mt.batterij_ontladen_energie.reading(ic::KWH_FROM_W);
    let pv_energie_dag_kwh = mt.pv_energie_dag.reading(uc::DAG_ALTIJD, now);
    let huisverbruik_dag_kwh = mt.huisverbruik_dag.reading(uc::DAG_ALTIJD, now);
    let netimport_dag_kwh = mt.netimport_dag.reading(uc::DAG_ALTIJD, now);
    let injectie_dag_kwh = mt.injectie_dag.reading(uc::DAG_ALTIJD, now);

    // --- quarter-hour budget ---------------------------------------------------------------
    let e = kwartier_import_kwh.unwrap_or(0.0);
    let kwartiervermogen_vastgelegd_kw = round(e * 4.0, 3);
    let rem_clock = f64::from(900 - ((now.minute() % 15) * 60 + now.second()));
    // HA's PowerConverter kW → W is `(val / (1/1000)) * 1`, unrounded (the entity has a
    // display precision, so HA does not round the converted state).
    let projectie_in = [kwartier_import_kwh, w.p1.power_w];
    let kwartiervermogen_projectie_w = renders.kwartiervermogen_projectie.gate(now, &projectie_in, || {
        let projectie_kw = round((e + max2(p1, 0.0) * rem_clock / 3_600_000.0) * 4.0, 3);
        Rendered { value: projectie_kw / (1.0 / 1000.0), inputs: projectie_in.to_vec() }
    });
    let capaciteit_maandtarget_kw = round(max2(max2(2.5, peak_w / 1000.0), s.capaciteit_maandtarget_instelling), 3);
    let tgt_w = capaciteit_maandtarget_kw * 1000.0;
    let rem = quarter_rem(now, kwartier_next_reset);
    let capaciteitspiek_risico = rem > 20.0 && (900.0 - rem) > 60.0 && kwartiervermogen_projectie_w > tgt_w - 25.0;

    // `None` (state unknown) while `sensor.capaciteit_maandtarget` or the quarter meter has no
    // value, so the `has_value('sensor.kwartier_toegestaan_vermogen')` fallbacks of its readers
    // apply. The month target always renders here (`float` defaults), so only the meter counts.
    // The render is still gated, so its stored inputs remember the meter was missing.
    //
    // It reads the quarter meter (state and `next_reset`) and the month target, and `now()`:
    // HA re-renders it when one of those changes or at the top of the minute. While the P1
    // import register stands still, the `rem <= 30` clamp waits for the next render.
    let toegestaan_in = [kwartier_import_kwh, Some(capaciteit_maandtarget_kw)];
    let toegestaan_w = renders.kwartier_toegestaan_vermogen.gate(now, &toegestaan_in, || {
        let rem = max2(rem, 1.0);
        let budget = (tgt_w - 100.0) * 0.25 / 1000.0;
        let raw = (budget - e) / (rem / 3600.0) * 1000.0;
        let aim = tgt_w - 100.0;
        let credit = raw - aim;
        let mut allowed = aim + if credit > 0.0 { credit * 0.65 } else { credit };
        if rem <= 30.0 {
            allowed = min2(allowed, tgt_w - 100.0);
        }
        Rendered { value: round(max2(0.0, min2(allowed, tgt_w * 1.35)), 0), inputs: toegestaan_in.to_vec() }
    });
    let toegestaan_w = kwartier_import_kwh.map(|_| toegestaan_w);
    // Reads P1 only in its first 30 s: past that, only the quarter meter and the minute
    // re-render it.
    let gemiddelde = &mut renders.kwartier_gemiddelde;
    let gemiddelde_in = [kwartier_import_kwh, w.p1.power_w];
    let read = gemiddelde.inputs.len().min(2);
    let kwartier_gemiddelde_w = gemiddelde.gate(now, &gemiddelde_in[..read], || {
        let elapsed = 900.0 - min2(max2(rem, 0.0), 900.0);
        if elapsed < 30.0 {
            Rendered { value: round(max2(0.0, p1), 0), inputs: gemiddelde_in.to_vec() }
        } else {
            Rendered { value: round(e / (elapsed / 3600.0) * 1000.0, 0), inputs: gemiddelde_in[..1].to_vec() }
        }
    });
    let kwartier_index = now.minute() / 15;

    // --- power balance ---------------------------------------------------------------------
    let omnik = if sun { w.omnik.power_w.unwrap_or(0.0) } else { 0.0 };
    let pv_vermogen_totaal_w = round(w.solis.pv_power_w.unwrap_or(0.0) + omnik, 0);
    let huisverbruik_vermogen_w = round(max2(0.0, pv_vermogen_totaal_w + batt + p1), 0);
    let batterij_beschikbare_energie_kwh = round(max2(w.solis.soc.unwrap_or(0.0) - 10.0, 0.0) / 100.0 * 10.0, 2);
    let pv_overschot_beschikbaar_w = round(-p1 - max2(batt, 0.0), 0);
    let piek_verschil_fluvius_vs_p1_w = round(w.fluvius.peak_kw.unwrap_or(0.0) * 1000.0 - peak_w, 0);
    let batterij_ontlaadvermogen_w = round(max2(0.0, batt), 0);
    let batterij_laadvermogen_w = round(max2(0.0, -batt), 0);
    let solis_offline = w.solis.soc.is_none();

    // --- chargers --------------------------------------------------------------------------
    let ev_laadkabel_vermogen_w = round(cable_a * cable_v, 0);
    let evse_charging = cascade::buzz_drawing(w);
    let ev_laadvermogen_smartevse_beschikbaar =
        w.evse.state.is_some() && (!evse_charging || w.evse.charge_current_a.is_some());
    let ev_laadvermogen_smartevse_kw = if evse_charging && ev_laadvermogen_smartevse_beschikbaar {
        round(w.evse.charge_current_a.unwrap_or(0.0) * 230.0 / 1000.0, 3)
    } else {
        0.0
    };
    let ev_laadvermogen_smartevse_w = round(ev_laadvermogen_smartevse_kw * 1000.0, 0);
    let huisverbruik_excl_stuurbaar_w = round(
        max2(
            0.0,
            huisverbruik_vermogen_w - w.cable.power_kw.unwrap_or(0.0) * 1000.0 - ev_laadvermogen_smartevse_kw * 1000.0,
        ),
        0,
    );
    let pv_bruto_overschot_w = if w.p1.power_w.is_some() && w.solis.battery_power_w.is_some() {
        round(max2(0.0, ev_laadkabel_vermogen_w + ev_laadvermogen_smartevse_w - batt - p1), 0)
    } else {
        0.0
    };

    let ev_actieve_laadauto = if cable_a > 0.5 {
        ActieveLaadauto::Ev3
    } else if evse_charging {
        ActieveLaadauto::Buzz
    } else if w.ev3.plugged == Some(true) {
        ActieveLaadauto::Ev3
    } else {
        ActieveLaadauto::Buzz
    };
    let cable_kw = cable_a * cable_v / 1000.0;
    let surplus_kw = max2(0.0, pv_bruto_overschot_w / 1000.0);
    let ev_laadvermogen_zon_kw = max2(min2(cable_kw + ev_laadvermogen_smartevse_kw, surplus_kw), 0.0);
    let ev_laadvermogen_net_kw = max2(cable_kw + ev_laadvermogen_smartevse_kw - ev_laadvermogen_zon_kw, 0.0);
    let ev_tot_kw = ev_laadvermogen_zon_kw + ev_laadvermogen_net_kw;
    let share_zon = |share: f64| {
        if ev_tot_kw > 0.0 { round(ev_laadvermogen_zon_kw * share / ev_tot_kw, 4) } else { 0.0 }
    };

    let ev3_laadvermogen_totaal_kw = round(if ev_actieve_laadauto == ActieveLaadauto::Ev3 { cable_kw } else { 0.0 }, 4);
    let ev3_laadvermogen_zon_kw = share_zon(ev3_laadvermogen_totaal_kw);
    let ev3_laadvermogen_net_kw = round(ev3_laadvermogen_totaal_kw - ev3_laadvermogen_zon_kw, 4);
    let buzz_cable_kw = if ev_actieve_laadauto == ActieveLaadauto::Buzz { cable_kw } else { 0.0 };
    let buzz_laadvermogen_totaal_kw = round(ev_laadvermogen_smartevse_kw + buzz_cable_kw, 4);
    let buzz_laadvermogen_zon_kw = share_zon(buzz_laadvermogen_totaal_kw);
    let buzz_laadvermogen_net_kw = round(buzz_laadvermogen_totaal_kw - buzz_laadvermogen_zon_kw, 4);

    // --- prices ----------------------------------------------------------------------------
    let backup = w.prices.backup_now.unwrap_or(-1.0);
    let stroomprijs_nu = if backup >= 0.0 { Some(backup) } else { w.prices.afname_now };
    let laadprijs = stroomprijs_nu.unwrap_or(w.prices.afname_now.unwrap_or(0.0));
    let ev3_laadkost_per_uur_eur = round(ev3_laadvermogen_net_kw * laadprijs, 4);
    let buzz_laadkost_per_uur_eur = round(buzz_laadvermogen_net_kw * laadprijs, 4);
    let pct = w.prices.afname_pct_in_range.unwrap_or(50.0);
    let niveau = if pct < 30.0 {
        "goedkoop"
    } else if pct > 70.0 {
        "duur"
    } else {
        "normaal"
    };
    let kwartierprijs_volgend =
        w.prices.afname_slots.iter().find(|p| p.start > now).map(|p| p.price).or(w.prices.afname_now);
    let kost_per_uur_eur = {
        let imp = max2(0.0, p1) / 1000.0;
        let exp = max2(0.0, -p1) / 1000.0;
        round(imp * w.prices.afname_now.unwrap_or(0.0) - exp * w.prices.injectie_now.unwrap_or(0.0), 4)
    };
    let injectie_effectieve_terugleververgoeding = w.prices.epex_now.map(|epex| round(0.98 * epex - 0.015, 5));

    // --- tariffs ---------------------------------------------------------------------------
    let vaste_kosten_per_maand_eur = round(5.0 * 1.06 + 17.85 * 1.06 / 12.0 + 0.004, 2);
    let peak_kw = max2(2.5, peak_w / 1000.0);
    let capaciteitstarief_deze_maand_eur = round(peak_kw * 56.04 / 12.0 * 1.06, 2);
    let capaciteitspiek_meerkost_eur = round(max2(0.0, capaciteit_maandtarget_kw - peak_kw) * 56.04 / 12.0 * 1.06, 2);
    let gefactureerde_piek_kw = gefactureerde_piek(now, &m.capaciteit_piekhistoriek, peak_w);
    let capaciteitstarief_factuur_deze_maand_eur =
        round(gefactureerde_piek_kw * 56.0429 * f64::from(days_in_month(now)) / 365.0 * 1.06, 2);

    // --- per-car economics -----------------------------------------------------------------
    let zonaandeel = |z: Option<f64>, t: Option<f64>| {
        let (z, t) = (z.unwrap_or(0.0), t.unwrap_or(0.0));
        if t > 0.0 { round(z / t * 100.0, 0) } else { 0.0 }
    };
    let per_100km = |v: Option<f64>, km: f64, n: i32| {
        if km > 0.0 { round(v.unwrap_or(0.0) / km * 100.0, n) } else { 0.0 }
    };
    let ev3_km = w.ev3.odometer_km.unwrap_or(0.0) - s.ev3_odometer_start;
    let buzz_km = w.buzz.odometer_km.unwrap_or(0.0) - s.buzz_odometer_start;
    let ev3_laadkost_per_100km_eur = per_100km(ev3_kost_eur, ev3_km, 2);
    let buzz_laadkost_per_100km_eur = per_100km(buzz_kost_eur, buzz_km, 2);
    let ev3_ice_kost_per_100km_eur = round(s.ev_benzineprijs * s.ev3_ice_verbruik, 2);
    let buzz_ice_kost_per_100km_eur = round(s.ev_benzineprijs * s.buzz_ice_verbruik, 2);

    // --- battery to car, self-consumption --------------------------------------------------
    let batterij_naar_auto_geleverd_kwh = if s.ev_batterij_niveau == BatterijNiveau::OntlaadBudget {
        round(max2(0.0, batterij_ontladen_energie_kwh.unwrap_or(0.0) - m.batterij_naar_auto_start_kwh), 2)
    } else {
        0.0
    };
    // `Zelfverbruik vandaag`: the share of today's PV not exported.
    let share_kept = |total: Option<f64>, minus: Option<f64>| {
        let (t, x) = (total.unwrap_or(0.0), minus.unwrap_or(0.0));
        if t < 0.1 { 0.0 } else { round(max2(0.0, min2(100.0, (t - x) / t * 100.0)), 0) }
    };
    let zelfverbruik_vandaag_kwh =
        round(max2(0.0, pv_energie_dag_kwh.unwrap_or(0.0) - injectie_dag_kwh.unwrap_or(0.0)), 2);

    // --- battery reservation ---------------------------------------------------------------
    let batterij_reservering_effectief_w = {
        // `states(toegestaan)|float(tgt) if has_value(toegestaan) else tgt`.
        let allowed = toegestaan_w.unwrap_or(tgt_w);
        let batt_chg = cascade::batt_charge_w(w);
        let load_nonsteer =
            p1 - ev_laadkabel_vermogen_w - ev_laadvermogen_smartevse_w - batt_chg + cascade::batt_discharge_w(w);
        let maxphase = cascade::maxphase(w);
        let ev3_elig =
            cascade::ev3_wants(w) && w.ev3.soc.unwrap_or(0.0) < w.ev3.ac_limit.unwrap_or(100.0) && cascade::ev3_ctrl(w);
        let buzz_elig = cascade::buzz_wants(w, m)
            && w.buzz.soc.unwrap_or(0.0) < w.buzz.target_soc.unwrap_or(100.0)
            && cascade::buzz_ctrl(w);
        let ev3_cur = w.cable.set_current_a.unwrap_or(0.0);
        let ev3_mx = cascade::ev3_max_a(w);
        let bz_cur = w.evse.override_a.unwrap_or(0.0);
        let bz_mx = cascade::buzz_max_a(w);
        let max_w = |elig: bool, mx: f64, cur: f64| {
            if elig { min2(mx, cur + (27.0 - maxphase)) * 230.0 } else { 0.0 }
        };
        let manual = if s.batterij_reservering_actief { s.batterij_reservering_w } else { 0.0 };
        let auto =
            max2(0.0, allowed - load_nonsteer - max_w(ev3_elig, ev3_mx, ev3_cur) - max_w(buzz_elig, bz_mx, bz_cur));
        let need = |drawing: bool, mx: f64, cur: f64| if drawing { min2(max2(mx - cur, 0.0), 4.0) } else { 0.0 };
        let ev3_need_a = need(cascade::ev3_current_flowing(w), ev3_mx, ev3_cur);
        let buzz_need_a = need(ev_laadvermogen_smartevse_w > 200.0, bz_mx, bz_cur);
        let phase_bound = max2(0.0, batt_chg + (27.0 - maxphase - ev3_need_a - buzz_need_a) * 230.0);
        if w.solis.soc.unwrap_or(0.0) >= 100.0 {
            // A full battery reserves nothing.
            0.0
        } else if w.p1.power_w.is_some() && w.solis.battery_power_w.is_some() {
            let manual_bound = max2(0.0, batt_chg + (27.0 - maxphase) * 230.0);
            round(max2(min2(manual, manual_bound), min2(auto, phase_bound)), 0)
        } else {
            round(manual, 0)
        }
    };

    let d = Derived {
        sun_above_horizon: sun,

        kwartier_import_kwh,
        kwartier_next_reset,
        p1_mediaan_w: mt.p1_mediaan.reading(now),
        batterij_ontladen_energie_kwh,
        batterij_geladen_energie_kwh: mt.batterij_geladen_energie.reading(ic::KWH_FROM_W),
        pv_energie_totaal_kwh: mt.pv_energie_totaal.reading(ic::KWH_FROM_W),
        huisverbruik_energie_kwh: mt.huisverbruik_energie.reading(ic::KWH_FROM_W),
        energiekost_cumulatief_eur: mt.energiekost_cumulatief.reading(ic::ENERGIEKOST),
        ev3_laadenergie_zon_kwh: ev3_zon_kwh,
        ev3_laadenergie_net_kwh: ev3_net_kwh,
        ev3_laadenergie_totaal_kwh: ev3_tot_kwh,
        ev3_laadkost_net_eur: ev3_kost_eur,
        buzz_laadenergie_zon_kwh: buzz_zon_kwh,
        buzz_laadenergie_net_kwh: buzz_net_kwh,
        buzz_laadenergie_totaal_kwh: buzz_tot_kwh,
        buzz_laadkost_net_eur: buzz_kost_eur,
        pv_energie_dag_kwh,
        huisverbruik_dag_kwh,
        netimport_dag_kwh,
        injectie_dag_kwh,
        batterij_geladen_dag_kwh: mt.batterij_geladen_dag.reading(uc::DAG_ALTIJD, now),
        batterij_ontladen_dag_kwh: mt.batterij_ontladen_dag.reading(uc::DAG_ALTIJD, now),
        energiekost_vandaag_eur: mt.energiekost_vandaag.reading(uc::ENERGIEKOST_VANDAAG, now),
        ev3_laadkost_dag_eur: mt.ev3.laadkost_dag.reading(uc::EV_DAG, now),
        ev3_laadkost_maand_eur: mt.ev3.laadkost_maand.reading(uc::EV_MAAND, now),
        ev3_laadenergie_dag_kwh: mt.ev3.laadenergie_dag.reading(uc::EV_DAG, now),
        ev3_laadenergie_maand_kwh: mt.ev3.laadenergie_maand.reading(uc::EV_MAAND, now),
        buzz_laadkost_dag_eur: mt.buzz.laadkost_dag.reading(uc::EV_DAG, now),
        buzz_laadkost_maand_eur: mt.buzz.laadkost_maand.reading(uc::EV_MAAND, now),
        buzz_laadenergie_dag_kwh: mt.buzz.laadenergie_dag.reading(uc::EV_DAG, now),
        buzz_laadenergie_maand_kwh: mt.buzz.laadenergie_maand.reading(uc::EV_MAAND, now),

        kwartiervermogen_vastgelegd_kw,
        kwartiervermogen_projectie_w,
        pv_vermogen_totaal_w,
        huisverbruik_vermogen_w,
        batterij_beschikbare_energie_kwh,
        stroomprijs_niveau: niveau.to_string(),
        capaciteitspiek_risico,
        kwartierprijs_volgend,
        vaste_kosten_per_maand_eur,
        capaciteitstarief_deze_maand_eur,
        pv_overschot_beschikbaar_w,
        huisverbruik_excl_stuurbaar_w,
        piek_verschil_fluvius_vs_p1_w,
        capaciteit_maandtarget_kw,
        pv_bruto_overschot_w,
        kost_per_uur_eur,
        solis_offline,
        ev_actieve_laadauto,
        ev_laadvermogen_zon_kw,
        ev_laadvermogen_net_kw,
        ev3_laadvermogen_zon_kw,
        ev3_laadvermogen_net_kw,
        ev3_laadvermogen_totaal_kw,
        ev3_laadkost_per_uur_eur,
        ev3_laad_zonaandeel_pct: zonaandeel(ev3_zon_kwh, ev3_tot_kwh),
        ev3_laadkost_per_100km_eur,
        ev3_verbruik_per_100km_kwh: per_100km(ev3_tot_kwh, ev3_km, 1),
        ev3_ice_kost_per_100km_eur,
        ev3_besparing_per_100km_eur: round(ev3_ice_kost_per_100km_eur - ev3_laadkost_per_100km_eur, 2),
        buzz_laadvermogen_zon_kw,
        buzz_laadvermogen_net_kw,
        buzz_laadvermogen_totaal_kw,
        buzz_laadkost_per_uur_eur,
        buzz_laad_zonaandeel_pct: zonaandeel(buzz_zon_kwh, buzz_tot_kwh),
        buzz_laadkost_per_100km_eur,
        buzz_verbruik_per_100km_kwh: per_100km(buzz_tot_kwh, buzz_km, 1),
        buzz_ice_kost_per_100km_eur,
        buzz_besparing_per_100km_eur: round(buzz_ice_kost_per_100km_eur - buzz_laadkost_per_100km_eur, 2),
        ev_laadkabel_vermogen_w,
        ev_laadvermogen_smartevse_kw,
        ev_laadvermogen_smartevse_w,
        injectie_effectieve_terugleververgoeding,
        batterij_naar_auto_geleverd_kwh,
        capaciteitspiek_meerkost_eur,
        batterij_ontlaadvermogen_w,
        batterij_laadvermogen_w,
        zelfverbruik_vandaag_pct: share_kept(pv_energie_dag_kwh, injectie_dag_kwh),
        zelfvoorziening_vandaag_pct: zelfvoorziening(pv_energie_dag_kwh, injectie_dag_kwh, netimport_dag_kwh),
        zelfverbruik_vandaag_kwh,
        kwartier_toegestaan_vermogen_w: toegestaan_w,
        kwartier_gemiddelde_w,
        kwartier_index,
        batterij_reservering_effectief_w,
        stroomprijs_nu,
        gefactureerde_piek_kw,
        capaciteitstarief_factuur_deze_maand_eur,

        batterij_setpoint_w: 0.0,
        dispatcher_off_target: false,
        ev_laadvermogen_smartevse_beschikbaar,
    };
    (d, renders)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meters::{Integrator, UtilityMeter};
    use crate::time::at;
    use crate::world::PriceSlot;
    use chrono::Duration;

    /// 2026-11-07 13:46:12.660, the instant of the live snapshot (the quarter meter's
    /// `next_reset` minus `now()` was 827.34 s when `Kwartier gemiddelde` rendered 3270).
    fn live_now() -> Ts {
        at(2026, 11, 7, 13, 46, 12) + Duration::milliseconds(660)
    }

    /// The raw energy readings of the live snapshot of 2026-11-07 13:46 (the Kolanky cable was
    /// offline, the Buzz was charging at 14.8 A, the battery full and idle).
    fn live_world() -> World {
        let mut w = World::default();
        w.p1.power_w = Some(3066.0);
        w.p1.phase_a = [Some(-1.4), Some(14.96), Some(14.31)];
        w.p1.import_kwh = Some(11_884.497);
        w.p1.export_kwh = Some(8_042.631);
        w.p1.month_peak_w = Some(3892.0);
        w.solis.soc = Some(100.0);
        w.solis.battery_power_w = Some(0.0);
        w.solis.pv_power_w = Some(227.0);
        w.omnik.power_w = Some(306.0);
        w.evse.plug = Some("Connected".into());
        w.evse.state = Some("Charging".into());
        w.evse.state_id = Some("C".into());
        w.evse.override_a = Some(14.8);
        w.evse.override_max_a = Some(29.0);
        w.evse.charge_current_a = Some(14.8);
        w.ev3.plugged = Some(false);
        w.ev3.soc = Some(54.5);
        w.ev3.ac_limit = Some(100.0);
        w.ev3.odometer_km = Some(8634.1);
        w.buzz.soc = Some(43.0);
        w.buzz.target_soc = Some(100.0);
        w.buzz.odometer_km = Some(10_019.0);
        w.prices.afname_now = Some(0.37692);
        w.prices.afname_pct_in_range = Some(8.5);
        w.prices.afname_slots = vec![
            PriceSlot { start: at(2026, 11, 7, 13, 45, 0), price: 0.37692 },
            PriceSlot { start: at(2026, 11, 7, 14, 0, 0), price: 0.3787 },
            PriceSlot { start: at(2026, 11, 7, 14, 15, 0), price: 0.38454 },
        ];
        w.prices.injectie_now = Some(0.2099);
        w.prices.epex_now = Some(0.2266);
        w.prices.backup_now = Some(0.37692);
        w.fluvius.peak_kw = Some(3.892);
        w
    }

    /// Helpers and meters as HA had them at the snapshot.
    fn live_memory() -> Memory {
        let mut m = Memory {
            smartevse_plug_laatst_bekend: "Connected".into(),
            batterij_naar_auto_start_kwh: 549.546,
            capaciteit_piekhistoriek: "2025-11:6.572,2025-12:8.011,2026-01:6.748,2026-02:6.299,2026-03:7.046,\
                                       2026-04:6.180,2026-05:8.791,2026-06:5.929,2026-07:3.400,2026-08:2.940,\
                                       2026-09:3.910,2026-10:4.916"
                .into(),
            ..Memory::default()
        };
        let mt = &mut m.meters;
        let day = Some(at(2026, 11, 7, 0, 0, 0));
        mt.kwartier_import = UtilityMeter::seeded(0.066, Some(at(2026, 11, 7, 13, 45, 0)));
        mt.pv_energie_dag = UtilityMeter::seeded(2.900, day);
        mt.huisverbruik_dag = UtilityMeter::seeded(19.218, day);
        mt.netimport_dag = UtilityMeter::seeded(20.523, day);
        mt.injectie_dag = UtilityMeter::seeded(0.004, day);
        mt.batterij_ontladen_energie = Integrator::seeded(750.288);
        mt.ev3.laadenergie_zon = Integrator::seeded(272.840_651_367_603_4);
        mt.ev3.laadenergie_totaal = Integrator::seeded(873.220_224_431_741_4);
        mt.ev3.laadkost_net = Integrator::seeded(165.796_522_174_022_5);
        mt.buzz.laadenergie_zon = Integrator::seeded(596.736_366_339_530_5);
        mt.buzz.laadenergie_totaal = Integrator::seeded(1_626.301_454_200_28);
        mt.buzz.laadkost_net = Integrator::seeded(244.364_870_288_538_94);
        mt.p1_mediaan.update(live_now() - Duration::milliseconds(600), Some(3066.0));
        m
    }

    /// Every template sensor of the live snapshot that does not depend on sub-second timing
    /// reproduces the state HA recorded at that instant, except the two templates changed by
    /// the 2026-11-07 fixes (`Zelfvoorziening vandaag`, `Batterij reservering effectief`), which
    /// are checked against the new formulas on the same inputs.
    #[test]
    fn live_snapshot_2026_11_07_reproduces_every_template_state() {
        let d = compute(live_now(), &live_world(), &Settings::default(), &live_memory());
        let expect: [(&str, f64, f64); 59] = [
            ("kwartiervermogen_vastgelegd", d.kwartiervermogen_vastgelegd_kw, 0.264),
            ("kwartiervermogen_projectie", d.kwartiervermogen_projectie_w, 3085.0),
            ("pv_vermogen_totaal", d.pv_vermogen_totaal_w, 533.0),
            ("huisverbruik_vermogen", d.huisverbruik_vermogen_w, 3599.0),
            ("batterij_beschikbare_energie", d.batterij_beschikbare_energie_kwh, 9.0),
            ("kwartierprijs_volgend", d.kwartierprijs_volgend.unwrap(), 0.3787),
            ("vaste_kosten_per_maand", d.vaste_kosten_per_maand_eur, 6.88),
            ("capaciteitstarief_deze_maand", d.capaciteitstarief_deze_maand_eur, 19.27),
            ("pv_overschot_beschikbaar", d.pv_overschot_beschikbaar_w, -3066.0),
            ("huisverbruik_excl_stuurbaar", d.huisverbruik_excl_stuurbaar_w, 195.0),
            ("piek_verschil_fluvius_vs_p1", d.piek_verschil_fluvius_vs_p1_w, 0.0),
            ("capaciteit_maandtarget", d.capaciteit_maandtarget_kw, 3.9),
            ("pv_bruto_overschot", d.pv_bruto_overschot_w, 338.0),
            ("kost_per_uur", d.kost_per_uur_eur, 1.1556),
            ("ev_laadvermogen_zon", d.ev_laadvermogen_zon_kw, 0.338),
            ("ev_laadvermogen_net", d.ev_laadvermogen_net_kw, 3.066),
            ("ev3_laadvermogen_zon", d.ev3_laadvermogen_zon_kw, 0.0),
            ("ev3_laadvermogen_net", d.ev3_laadvermogen_net_kw, 0.0),
            ("ev3_laadvermogen_totaal", d.ev3_laadvermogen_totaal_kw, 0.0),
            ("ev3_laadkost_per_uur", d.ev3_laadkost_per_uur_eur, 0.0),
            ("ev3_laad_zonaandeel", d.ev3_laad_zonaandeel_pct, 31.0),
            ("ev3_laadkost_per_100km", d.ev3_laadkost_per_100km_eur, 2.49),
            ("ev3_verbruik_per_100km", d.ev3_verbruik_per_100km_kwh, 13.1),
            ("ev3_ice_kost_per_100km", d.ev3_ice_kost_per_100km_eur, 11.38),
            ("ev3_besparing_per_100km", d.ev3_besparing_per_100km_eur, 8.89),
            ("buzz_laadvermogen_zon", d.buzz_laadvermogen_zon_kw, 0.338),
            ("buzz_laadvermogen_net", d.buzz_laadvermogen_net_kw, 3.066),
            ("buzz_laadvermogen_totaal", d.buzz_laadvermogen_totaal_kw, 3.404),
            ("buzz_laadkost_per_uur", d.buzz_laadkost_per_uur_eur, 1.1556),
            ("buzz_laad_zonaandeel", d.buzz_laad_zonaandeel_pct, 37.0),
            ("buzz_laadkost_per_100km", d.buzz_laadkost_per_100km_eur, 2.44),
            ("buzz_verbruik_per_100km", d.buzz_verbruik_per_100km_kwh, 16.2),
            ("buzz_ice_kost_per_100km", d.buzz_ice_kost_per_100km_eur, 15.75),
            ("buzz_besparing_per_100km", d.buzz_besparing_per_100km_eur, 13.31),
            ("ev_laadkabel_vermogen_w", d.ev_laadkabel_vermogen_w, 0.0),
            ("ev_laadvermogen_smartevse", d.ev_laadvermogen_smartevse_kw, 3.404),
            ("ev_laadvermogen_smartevse_w", d.ev_laadvermogen_smartevse_w, 3404.0),
            ("injectie_effectieve_terugleververgoeding", d.injectie_effectieve_terugleververgoeding.unwrap(), 0.20707),
            ("batterij_naar_auto_geleverd_kwh", d.batterij_naar_auto_geleverd_kwh, 0.0),
            ("capaciteitspiek_meerkost", d.capaciteitspiek_meerkost_eur, 0.04),
            ("batterij_ontlaadvermogen", d.batterij_ontlaadvermogen_w, 0.0),
            ("batterij_laadvermogen", d.batterij_laadvermogen_w, 0.0),
            ("zelfverbruik_vandaag", d.zelfverbruik_vandaag_pct, 100.0),
            // HA showed 0 with the old formula; (2.9 − 0.004) / (2.896 + 20.523) = 12.37 % → 12.
            ("zelfvoorziening_vandaag", d.zelfvoorziening_vandaag_pct, 12.0),
            ("zelfverbruik_vandaag_kwh", d.zelfverbruik_vandaag_kwh, 2.9),
            ("kwartier_toegestaan_vermogen", d.kwartier_toegestaan_vermogen_w.unwrap(), 3830.0),
            ("kwartier_gemiddelde", d.kwartier_gemiddelde_w, 3270.0),
            ("kwartier_index", f64::from(d.kwartier_index), 3.0),
            // HA showed 700 before the fix; the battery was at SoC 100, which now reserves 0.
            ("batterij_reservering_effectief", d.batterij_reservering_effectief_w, 0.0),
            ("stroomprijs_nu", d.stroomprijs_nu.unwrap(), 0.37692),
            ("gefactureerde_piek", d.gefactureerde_piek_kw, 5.672),
            ("capaciteitstarief_factuur_deze_maand", d.capaciteitstarief_factuur_deze_maand_eur, 27.69),
            ("p1_meter_p1_mediaan_90s", d.p1_mediaan_w.unwrap(), 3066.0),
            ("p1_meter_kwartier_import", d.kwartier_import_kwh.unwrap(), 0.066),
            ("pv_energie_dag", d.pv_energie_dag_kwh.unwrap(), 2.9),
            ("huisverbruik_dag", d.huisverbruik_dag_kwh.unwrap(), 19.218),
            ("netimport_dag", d.netimport_dag_kwh.unwrap(), 20.523),
            ("injectie_dag", d.injectie_dag_kwh.unwrap(), 0.004),
            ("batterij_ontladen_energie", d.batterij_ontladen_energie_kwh.unwrap(), 750.288),
        ];
        for (name, got, want) in expect {
            assert_eq!(got, want, "{name}");
        }
        assert_eq!(d.stroomprijs_niveau, "goedkoop");
        assert!(!d.capaciteitspiek_risico);
        assert!(!d.solis_offline);
        assert!(d.sun_above_horizon);
        assert_eq!(d.ev_actieve_laadauto, ActieveLaadauto::Buzz);
        assert!(d.ev_laadvermogen_smartevse_beschikbaar);
        assert_eq!(d.kwartier_next_reset, Some(at(2026, 11, 7, 14, 0, 0)));
    }

    fn quiet() -> (World, Memory) {
        (World::default(), Memory::default())
    }

    /// `Kwartiervermogen projectie`: 0.2 kWh used, 2400 W for the 300 s left of the quarter
    /// (13:55:00): (0.2 + 2400·300/3 600 000)·4 = (0.2 + 0.2)·4 = 1.6 kW, stored as 1600 W.
    /// `Capaciteitspiek risico` compares that W value with the target in W: with a 1.6 kW
    /// target, 1600 > 1600 − 25 → risk; 300 s left and 600 s gone are inside the 20/60 s guards.
    /// `Kwartiervermogen vastgelegd` = 0.2·4 = 0.8 kW.
    #[test]
    fn projectie_is_stored_in_watts_and_drives_the_risk_flag() {
        let (mut w, mut m) = quiet();
        let now = at(2026, 11, 7, 13, 55, 0);
        w.p1.power_w = Some(2400.0);
        m.meters.kwartier_import = UtilityMeter::seeded(0.2, Some(at(2026, 11, 7, 13, 45, 0)));
        let s = Settings { capaciteit_maandtarget_instelling: 1.6, ..Settings::default() };
        let d = compute(now, &w, &s, &m);
        assert_eq!(d.kwartiervermogen_vastgelegd_kw, 0.8);
        assert_eq!(d.kwartiervermogen_projectie_w, 1600.0);
        // The target floor is 2.5 kW, not the 1.6 setting: 1600 < 2475 → no risk.
        assert_eq!(d.capaciteit_maandtarget_kw, 2.5);
        assert!(!d.capaciteitspiek_risico);
        w.p1.power_w = Some(30_000.0);
        // (0.2 + 30000·300/3.6e6)·4 = (0.2 + 2.5)·4 = 10.8 kW > 2475 W → risk.
        assert!(compute(now, &w, &s, &m).capaciteitspiek_risico);
        // Not in the first 60 s of a quarter, nor the last 20 s.
        assert!(!compute(at(2026, 11, 7, 13, 45, 30), &w, &s, &m).capaciteitspiek_risico);
        assert!(!compute(at(2026, 11, 7, 13, 59, 45), &w, &s, &m).capaciteitspiek_risico);
    }

    /// `Capaciteitspiek risico`: `rem > 20 and (900 - rem) > 60 and projectie > target·1000 −
    /// 25`, all strict. With 20 s left (13:59:40) or exactly 60 s gone (13:46:00) there is no
    /// risk; one second further inside there is. A projection of exactly 2500 W against a
    /// 2.525 kW target (2525 − 25 = 2500) is no risk; against 2.52 kW (2495) it is.
    #[test]
    fn the_risk_window_and_threshold_are_strict() {
        let (mut w, mut m) = quiet();
        // 200 kW: even with 21 s left (0.2 + 200000·21/3.6e6)·4 = 5.47 kW > 3900 − 25 W.
        w.p1.power_w = Some(200_000.0);
        m.meters.kwartier_import = UtilityMeter::seeded(0.2, Some(at(2026, 11, 7, 13, 45, 0)));
        let s = Settings::default();
        let risk = |now: Ts, w: &World, s: &Settings, m: &Memory| compute(now, w, s, m).capaciteitspiek_risico;
        assert!(!risk(at(2026, 11, 7, 13, 59, 40), &w, &s, &m), "20 s left");
        assert!(risk(at(2026, 11, 7, 13, 59, 39), &w, &s, &m), "21 s left");
        assert!(!risk(at(2026, 11, 7, 13, 46, 0), &w, &s, &m), "60 s gone");
        assert!(risk(at(2026, 11, 7, 13, 46, 1), &w, &s, &m), "61 s gone");

        // 0.625 kWh used, no import: (0.625 + 0)·4 = 2.5 kW → 2500 W.
        w.p1.power_w = Some(0.0);
        m.meters.kwartier_import = UtilityMeter::seeded(0.625, Some(at(2026, 11, 7, 13, 45, 0)));
        let now = at(2026, 11, 7, 13, 55, 0);
        let s = Settings { capaciteit_maandtarget_instelling: 2.525, ..Settings::default() };
        assert_eq!(compute(now, &w, &s, &m).kwartiervermogen_projectie_w, 2500.0);
        assert!(!risk(now, &w, &s, &m), "2500 is not above 2525 − 25");
        let s = Settings { capaciteit_maandtarget_instelling: 2.52, ..Settings::default() };
        assert!(risk(now, &w, &s, &m), "2500 > 2520 − 25");
    }

    /// A negative P1 (export) counts as 0 in the projection: `[p1, 0] | max`.
    #[test]
    fn projectie_ignores_export() {
        let (mut w, m) = quiet();
        w.p1.power_w = Some(-3000.0);
        let d = compute(at(2026, 11, 7, 13, 50, 0), &w, &Settings::default(), &m);
        assert_eq!(d.kwartiervermogen_projectie_w, 0.0);
    }

    /// `Capaciteit maandtarget` = max(2.5, month peak in kW, setting), rounded to 3.
    #[test]
    fn maandtarget_takes_the_highest_of_floor_peak_and_setting() {
        let (mut w, m) = quiet();
        let now = at(2026, 11, 7, 12, 0, 0);
        w.p1.month_peak_w = Some(4321.4);
        let d = compute(now, &w, &Settings::default(), &m);
        assert_eq!(d.capaciteit_maandtarget_kw, 4.321);
        let s = Settings { capaciteit_maandtarget_instelling: 5.0, ..Settings::default() };
        assert_eq!(compute(now, &w, &s, &m).capaciteit_maandtarget_kw, 5.0);
    }

    /// `Kwartier toegestaan vermogen`, target 3.9 kW → aim 3800 W, budget 0.95 kWh.
    /// At 13:50 (600 s left, 0.2 kWh used): raw = 0.75/(600/3600)·1000 = 4500, credit 700 →
    /// 3800 + 0.65·700 = 4255.
    /// With 0.5 kWh used: raw = 0.45·6000 = 2700, credit −1100 → 2700 (no discount).
    /// At 13:59:40 (20 s left, 0.94 kWh): raw = 0.01/(20/3600)·1000 = 1800 → but the last 30 s
    /// cap at aim: min(… , 3800); 0.9 kWh used: raw = 0.05·180000 = 9000, credit 5200 →
    /// 3800 + 3380 = 7180, capped to 3800 by the last-30-s rule.
    /// Never above 1.35·3900 = 5265 or below 0.
    #[test]
    fn toegestaan_spends_the_remaining_budget_with_a_65_percent_credit() {
        let (w, mut m) = quiet();
        let s = Settings::default();
        let q = Some(at(2026, 11, 7, 13, 45, 0));
        let at1350 = at(2026, 11, 7, 13, 50, 0);
        let allowed = |used: f64, now: Ts, m: &mut Memory| {
            m.meters.kwartier_import = UtilityMeter::seeded(used, q);
            compute(now, &w, &s, m).kwartier_toegestaan_vermogen_w.unwrap()
        };
        assert_eq!(allowed(0.2, at1350, &mut m), 4255.0);
        assert_eq!(allowed(0.5, at1350, &mut m), 2700.0);
        let late = at(2026, 11, 7, 13, 59, 40);
        assert_eq!(allowed(0.9, late, &mut m), 3800.0);
        // 0.0 used at 13:50: raw = 0.95·6000 = 5700, credit 1900 → 5035 (< 5265).
        assert_eq!(allowed(0.0, at1350, &mut m), 5035.0);
        // 0.0 used at 13:55 (300 s): raw = 11400, credit 7600 → 8740 → capped at 5265.
        assert_eq!(allowed(0.0, at(2026, 11, 7, 13, 55, 0), &mut m), 5265.0);
        // Over budget: 1.2 kWh used → raw negative → 0.
        assert_eq!(allowed(1.2, at1350, &mut m), 0.0);
    }

    /// With the quarter meter unavailable its `next_reset` attribute is gone and `Kwartier
    /// gemiddelde` falls back to the wall clock: `900 - (minute % 15)·60 - second` (whole
    /// seconds), the meter value reading 0 (`float(0)`). `Kwartier toegestaan vermogen` renders
    /// `None` (`not has_value('sensor.p1_meter_kwartier_import')`), and comes back with the meter.
    #[test]
    fn without_the_meter_the_allowance_is_unknown_and_the_average_uses_the_clock() {
        let (w, mut m) = quiet();
        m.meters.kwartier_import.available = false;
        let now = at(2026, 11, 7, 13, 50, 0) + Duration::milliseconds(900);
        let d = compute(now, &w, &Settings::default(), &m);
        assert_eq!(d.kwartier_next_reset, None);
        assert_eq!(d.kwartier_import_kwh, None);
        assert_eq!(d.kwartier_toegestaan_vermogen_w, None);
        // elapsed = 300 s (the .9 s is ignored), 0 kWh used → 0 W.
        assert_eq!(d.kwartier_gemiddelde_w, 0.0);
        let d = compute(at(2026, 11, 7, 13, 50, 30), &w, &Settings::default(), &m);
        assert_eq!(d.kwartier_toegestaan_vermogen_w, None);
        // The meter back at 0 kWh (13:45 quarter): 13:50 → 5035 W as above.
        assert_eq!(
            compute(at(2026, 11, 7, 13, 50, 0), &w, &Settings::default(), &metered()).kwartier_toegestaan_vermogen_w,
            Some(5035.0)
        );
    }

    /// `Batterij reservering effectief`: `allowed = toegestaan if has_value(toegestaan) else
    /// tgt`. Without the quarter meter (allowance unknown) the target 3900 W is used: P1 1000,
    /// worst phase 10 A → auto = 3900 − 1000 = 2900 (< phase bound 17 · 230 = 3910).
    #[test]
    fn the_reservation_falls_back_to_the_month_target_without_an_allowance() {
        let w = resv_world(10.0);
        let d = compute(at(2026, 11, 7, 13, 50, 0), &w, &manual_off(), &Memory::default());
        assert_eq!(d.kwartier_toegestaan_vermogen_w, None);
        assert_eq!(d.batterij_reservering_effectief_w, 2900.0);
    }

    /// `Batterij reservering effectief` is 0 at SoC ≥ 100: a full battery reserves nothing,
    /// whatever the manual setting (5000 W here; 3910 W at 99.9 %).
    #[test]
    fn a_full_battery_reserves_nothing() {
        let mut w = resv_world(10.0);
        let s = Settings { batterij_reservering_actief: true, batterij_reservering_w: 5000.0, ..Settings::default() };
        w.solis.soc = Some(100.0);
        assert_eq!(resv(&w, &s), 0.0);
        w.solis.soc = Some(99.9);
        assert_eq!(resv(&w, &s), 3910.0);
    }

    /// `Kwartier gemiddelde`: in the first 30 s (`elapsed < 30`) of a quarter it shows the live import
    /// (`[0, p1] | max`); after that used energy over elapsed time: 0.1 kWh after 120 s →
    /// 0.1/(120/3600)·1000 = 3000 W.
    #[test]
    fn kwartier_gemiddelde_uses_live_power_in_the_first_30_seconds() {
        let (mut w, mut m) = quiet();
        w.p1.power_w = Some(-500.0);
        let s = Settings::default();
        m.meters.kwartier_import = UtilityMeter::seeded(0.1, Some(at(2026, 11, 7, 13, 45, 0)));
        assert_eq!(compute(at(2026, 11, 7, 13, 45, 10), &w, &s, &m).kwartier_gemiddelde_w, 0.0);
        w.p1.power_w = Some(1234.4);
        assert_eq!(compute(at(2026, 11, 7, 13, 45, 10), &w, &s, &m).kwartier_gemiddelde_w, 1234.0);
        // `elapsed < 30` is strict: at 30 s the average is 0.1/(30/3600)·1000 = 12000 W.
        assert_eq!(compute(at(2026, 11, 7, 13, 45, 30), &w, &s, &m).kwartier_gemiddelde_w, 12000.0);
        assert_eq!(compute(at(2026, 11, 7, 13, 47, 0), &w, &s, &m).kwartier_gemiddelde_w, 3000.0);
        assert_eq!(compute(at(2026, 11, 7, 13, 47, 0), &w, &s, &m).kwartier_index, 3);
        assert_eq!(compute(at(2026, 11, 7, 13, 14, 59), &w, &s, &m).kwartier_index, 0);
    }

    /// The quarter meter rolled over at 14:00 but has not seen a reading yet: templates
    /// already read 0 kWh and a full quarter, like HA, whose reset runs on a timer.
    #[test]
    fn a_due_quarter_reset_reads_as_zero_before_the_meter_updates() {
        let (w, mut m) = quiet();
        m.meters.kwartier_import = UtilityMeter::seeded(0.9, Some(at(2026, 11, 7, 13, 45, 0)));
        let d = compute(at(2026, 11, 7, 14, 0, 0), &w, &Settings::default(), &m);
        assert_eq!(d.kwartier_import_kwh, Some(0.0));
        assert_eq!(d.kwartier_next_reset, Some(at(2026, 11, 7, 14, 15, 0)));
    }

    /// `PV vermogen totaal` adds the Omnik only while the sun is up; `Huisverbruik vermogen`
    /// = max(0, pv + batt + p1); battery views split the signed power.
    #[test]
    fn omnik_counts_only_in_daylight_and_house_load_is_the_balance() {
        let (mut w, m) = quiet();
        w.solis.pv_power_w = Some(100.4);
        w.omnik.power_w = Some(50.0);
        w.solis.battery_power_w = Some(-400.0);
        w.p1.power_w = Some(200.0);
        let s = Settings::default();
        let night = compute(at(2026, 11, 7, 23, 0, 0), &w, &s, &m);
        assert!(!night.sun_above_horizon);
        assert_eq!(night.pv_vermogen_totaal_w, 100.0);
        assert_eq!(night.huisverbruik_vermogen_w, 0.0); // 100 − 400 + 200 < 0
        assert_eq!(night.batterij_laadvermogen_w, 400.0);
        assert_eq!(night.batterij_ontlaadvermogen_w, 0.0);
        // pv overschot = −200 − max(−400, 0) = −200.
        assert_eq!(night.pv_overschot_beschikbaar_w, -200.0);
        let day = compute(at(2026, 11, 7, 12, 0, 0), &w, &s, &m);
        assert_eq!(day.pv_vermogen_totaal_w, 150.0);
        w.solis.battery_power_w = Some(700.0);
        let d = compute(at(2026, 11, 7, 12, 0, 0), &w, &s, &m);
        assert_eq!(d.huisverbruik_vermogen_w, 1050.0);
        assert_eq!(d.batterij_ontlaadvermogen_w, 700.0);
        // −200 − 700 = −900.
        assert_eq!(d.pv_overschot_beschikbaar_w, -900.0);
    }

    /// `Batterij beschikbare energie` = max(soc − 10, 0)/100·10 kWh; `Solis offline` when
    /// the SoC is unavailable (and then the energy reads 0).
    #[test]
    fn battery_energy_above_the_10_percent_floor_and_offline_flag() {
        let (mut w, m) = quiet();
        let now = at(2026, 11, 7, 12, 0, 0);
        w.solis.soc = Some(55.0);
        let d = compute(now, &w, &Settings::default(), &m);
        assert_eq!(d.batterij_beschikbare_energie_kwh, 4.5);
        assert!(!d.solis_offline);
        w.solis.soc = Some(5.0);
        assert_eq!(compute(now, &w, &Settings::default(), &m).batterij_beschikbare_energie_kwh, 0.0);
        w.solis.soc = None;
        assert!(compute(now, &w, &Settings::default(), &m).solis_offline);
    }

    /// `EV laadvermogen smartevse` has an availability template: unavailable without a
    /// SmartEVSE state, or while Charging without a current. Not charging → 0 kW.
    /// Charging at 16 A → 16·230/1000 = 3.68 kW, 3680 W.
    #[test]
    fn smartevse_power_and_its_availability() {
        let (mut w, m) = quiet();
        let now = at(2026, 11, 7, 12, 0, 0);
        let s = Settings::default();
        let d = compute(now, &w, &s, &m);
        assert!(!d.ev_laadvermogen_smartevse_beschikbaar);
        assert_eq!(d.ev_laadvermogen_smartevse_kw, 0.0);
        w.evse.state = Some("Charging".into());
        let d = compute(now, &w, &s, &m);
        assert!(!d.ev_laadvermogen_smartevse_beschikbaar);
        assert_eq!(d.ev_laadvermogen_smartevse_w, 0.0);
        w.evse.charge_current_a = Some(16.0);
        let d = compute(now, &w, &s, &m);
        assert!(d.ev_laadvermogen_smartevse_beschikbaar);
        assert_eq!(d.ev_laadvermogen_smartevse_kw, 3.68);
        assert_eq!(d.ev_laadvermogen_smartevse_w, 3680.0);
        w.evse.state = Some("Connected to EV".into());
        let d = compute(now, &w, &s, &m);
        assert!(d.ev_laadvermogen_smartevse_beschikbaar);
        assert_eq!(d.ev_laadvermogen_smartevse_kw, 0.0);
    }

    /// The EV3 cable at 10 A / 232 V = 2.32 kW, 2320 W, importing 820 W with the battery
    /// idle: `PV bruto overschot` = max(0, 2320 + 0 − 0 − 820) = 1500 W. Zon = min(2.32, 1.5) = 1.5, net = 0.82.
    /// The cable car is the active one (10 A > 0.5), so EV3 gets everything: zon 1.5,
    /// net 0.82, totaal 2.32; cost at 0.30 EUR/kWh = 0.82·0.3 = 0.246 EUR/h. The Buzz gets 0.
    /// `Huisverbruik excl stuurbaar` subtracts the cable's own kW sensor: 2140 − 2.32·1000 → 0.
    #[test]
    fn cable_charging_splits_solar_and_grid_to_the_ev3() {
        let (mut w, m) = quiet();
        let now = at(2026, 11, 7, 23, 0, 0);
        w.cable.current_a = Some(10.0);
        w.cable.voltage_v = Some(232.0);
        w.cable.power_kw = Some(2.32);
        w.p1.power_w = Some(820.0);
        w.solis.battery_power_w = Some(0.0);
        w.solis.pv_power_w = Some(1320.0);
        w.prices.afname_now = Some(0.30);
        let d = compute(now, &w, &Settings::default(), &m);
        assert_eq!(d.ev_laadkabel_vermogen_w, 2320.0);
        assert_eq!(d.pv_bruto_overschot_w, 1500.0);
        assert_eq!(d.ev_actieve_laadauto, ActieveLaadauto::Ev3);
        assert_eq!(d.ev_laadvermogen_zon_kw, 1.5);
        assert!((d.ev_laadvermogen_net_kw - 0.82).abs() < 1e-12);
        assert_eq!(d.ev3_laadvermogen_totaal_kw, 2.32);
        assert_eq!(d.ev3_laadvermogen_zon_kw, 1.5);
        assert_eq!(d.ev3_laadvermogen_net_kw, 0.82);
        assert_eq!(d.ev3_laadkost_per_uur_eur, 0.246);
        assert_eq!(d.buzz_laadvermogen_totaal_kw, 0.0);
        assert_eq!(d.buzz_laadvermogen_zon_kw, 0.0);
        // pv 1320 + 0 + 820 = 2140 − 2320 → 0.
        assert_eq!(d.huisverbruik_excl_stuurbaar_w, 0.0);
    }

    /// `EV actieve laadauto`: cable current > 0.5 A → ev3; else SmartEVSE Charging → buzz;
    /// else EV3 plugged → ev3; else buzz. A cable current on the Buzz's turn (both drawing is
    /// impossible in practice) goes to the Buzz totals only when the Buzz is active.
    #[test]
    fn actieve_laadauto_precedence() {
        let (mut w, m) = quiet();
        let now = at(2026, 11, 7, 23, 0, 0);
        let s = Settings::default();
        assert_eq!(compute(now, &w, &s, &m).ev_actieve_laadauto, ActieveLaadauto::Buzz);
        // `ev_charging_cable_stroom > 0.5` is strict: 0.5 A on an idle cable is not the EV3.
        w.cable.current_a = Some(0.5);
        assert_eq!(compute(now, &w, &s, &m).ev_actieve_laadauto, ActieveLaadauto::Buzz);
        w.cable.current_a = None;
        w.ev3.plugged = Some(true);
        assert_eq!(compute(now, &w, &s, &m).ev_actieve_laadauto, ActieveLaadauto::Ev3);
        w.evse.state = Some("Charging".into());
        assert_eq!(compute(now, &w, &s, &m).ev_actieve_laadauto, ActieveLaadauto::Buzz);
        w.cable.current_a = Some(0.4);
        let d = compute(now, &w, &s, &m);
        assert_eq!(d.ev_actieve_laadauto, ActieveLaadauto::Buzz);
        // 0.4·230/1000 = 0.092 kW of cable current lands on the Buzz.
        assert_eq!(d.buzz_laadvermogen_totaal_kw, 0.092);
        w.cable.current_a = Some(0.6);
        assert_eq!(compute(now, &w, &s, &m).ev_actieve_laadauto, ActieveLaadauto::Ev3);
    }

    /// `PV bruto overschot` is 0 unless both P1 and the battery power have a value.
    #[test]
    fn bruto_overschot_needs_p1_and_battery() {
        let (mut w, m) = quiet();
        let now = at(2026, 11, 7, 12, 0, 0);
        w.p1.power_w = Some(-2000.0);
        assert_eq!(compute(now, &w, &Settings::default(), &m).pv_bruto_overschot_w, 0.0);
        w.solis.battery_power_w = Some(-500.0);
        // 0 + 0 − (−500) − (−2000) = 2500.
        assert_eq!(compute(now, &w, &Settings::default(), &m).pv_bruto_overschot_w, 2500.0);
    }

    /// `Stroomprijs nu`: the backup price when ≥ 0, else the market price, else unavailable.
    /// `Stroomprijs niveau` on the range percentage (default 50): < 30 goedkoop, > 70 duur.
    /// `Kwartierprijs volgend` falls back to the current price after the last slot.
    /// `Injectie effectieve terugleververgoeding` is unknown without EPEX.
    /// `Kost per uur` while exporting 1500 W at 0.05 injection: −1.5·0.05 = −0.075.
    #[test]
    fn price_fallbacks() {
        let (mut w, m) = quiet();
        let now = at(2026, 11, 7, 12, 0, 0);
        let s = Settings::default();
        let d = compute(now, &w, &s, &m);
        assert_eq!(d.stroomprijs_nu, None);
        assert_eq!(d.kwartierprijs_volgend, None);
        assert_eq!(d.injectie_effectieve_terugleververgoeding, None);
        assert_eq!(d.stroomprijs_niveau, "normaal");
        w.prices.backup_now = Some(-1.0);
        w.prices.afname_now = Some(0.25);
        w.prices.afname_pct_in_range = Some(71.0);
        w.prices.injectie_now = Some(0.05);
        w.p1.power_w = Some(-1500.0);
        w.prices.afname_slots = vec![PriceSlot { start: at(2026, 11, 7, 12, 0, 0), price: 0.1 }];
        let d = compute(now, &w, &s, &m);
        assert_eq!(d.stroomprijs_nu, Some(0.25));
        assert_eq!(d.stroomprijs_niveau, "duur");
        // The only slot starts at `now`, not after it.
        assert_eq!(d.kwartierprijs_volgend, Some(0.25));
        assert_eq!(d.kost_per_uur_eur, -0.075);
        w.prices.backup_now = Some(0.0);
        assert_eq!(compute(now, &w, &s, &m).stroomprijs_nu, Some(0.0));
        // `'goedkoop' if p < 30 else ('duur' if p > 70 else 'normaal')`: both ends are normaal.
        for (pct, niveau) in [(29.9, "goedkoop"), (30.0, "normaal"), (70.0, "normaal"), (70.1, "duur")] {
            w.prices.afname_pct_in_range = Some(pct);
            assert_eq!(compute(now, &w, &s, &m).stroomprijs_niveau, niveau, "{pct}");
        }
    }

    /// The per-100-km figures are 0 until the odometer passes the start reading, and the
    /// solar shares 0 until anything was charged.
    #[test]
    fn per_100km_and_solar_share_guard_against_zero() {
        let (mut w, m) = quiet();
        let s = Settings { ev3_odometer_start: 1000.0, ..Settings::default() };
        w.ev3.odometer_km = Some(1000.0);
        let d = compute(at(2026, 11, 7, 12, 0, 0), &w, &s, &m);
        assert_eq!(d.ev3_laadkost_per_100km_eur, 0.0);
        assert_eq!(d.ev3_verbruik_per_100km_kwh, 0.0);
        assert_eq!(d.ev3_laad_zonaandeel_pct, 0.0);
        assert_eq!(d.buzz_verbruik_per_100km_kwh, 0.0);
        // `… if tot > 0 else 0`: nothing charging is a 0 share, not 0/0.
        assert_eq!(d.ev3_laadvermogen_zon_kw, 0.0);
        assert_eq!(d.buzz_laadvermogen_zon_kw, 0.0);
        assert_eq!(d.buzz_laadvermogen_net_kw, 0.0);
        // ICE 1.75·6.5 = 11.375 → 11.38 (exact tie in binary, rounds to even 8).
        assert_eq!(d.ev3_besparing_per_100km_eur, 11.38);
    }

    /// `Batterij naar auto geleverd kWh`: only in `Ontlaad-budget`, discharged since the
    /// start reading, never negative: 752.0 − 749.5 = 2.5; start above the counter → 0.
    #[test]
    fn battery_to_car_counts_only_in_discharge_budget_mode() {
        let (w, mut m) = quiet();
        m.meters.batterij_ontladen_energie = Integrator::seeded(752.0);
        m.batterij_naar_auto_start_kwh = 749.5;
        let now = at(2026, 11, 7, 12, 0, 0);
        let mut s = Settings::default();
        assert_eq!(compute(now, &w, &s, &m).batterij_naar_auto_geleverd_kwh, 0.0);
        s.ev_batterij_niveau = BatterijNiveau::OntlaadBudget;
        assert_eq!(compute(now, &w, &s, &m).batterij_naar_auto_geleverd_kwh, 2.5);
        m.batterij_naar_auto_start_kwh = 760.0;
        assert_eq!(compute(now, &w, &s, &m).batterij_naar_auto_geleverd_kwh, 0.0);
    }

    /// `Zelfverbruik vandaag` is 0 below 0.1 kWh of PV; otherwise (pv − inj)/pv clamped to
    /// 0..100: 4 kWh PV, 1 kWh injected → 75 %, 3 kWh kept.
    #[test]
    fn self_consumption_needs_some_pv() {
        let (w, mut m) = quiet();
        let now = at(2026, 11, 7, 12, 0, 0);
        let day = Some(at(2026, 11, 7, 0, 0, 0));
        m.meters.pv_energie_dag = UtilityMeter::seeded(0.05, day);
        assert_eq!(compute(now, &w, &Settings::default(), &m).zelfverbruik_vandaag_pct, 0.0);
        m.meters.pv_energie_dag = UtilityMeter::seeded(4.0, day);
        m.meters.injectie_dag = UtilityMeter::seeded(1.0, day);
        let d = compute(now, &w, &Settings::default(), &m);
        assert_eq!(d.zelfverbruik_vandaag_pct, 75.0);
        assert_eq!(d.zelfverbruik_vandaag_kwh, 3.0);
    }

    /// `Batterij reservering effectief` with the EV3 eligible and drawing, the Buzz idle:
    /// allowed (meter at 0 kWh, target 3.9 kW, 13:50) = 5035 W; p1 1000, cable 8 A·230 = 1840 W,
    /// battery charging 500 W → load_nonsteer = 1000 − 1840 − 0 − 500 + 0 = −1340.
    /// maxphase 10 A → EV3 max = min(16, 8 + 17)·230 = 3680. auto = 5035 + 1340 − 3680 = 2695.
    /// EV3 needs min(16 − 8, 4) = 4 A → phase_bound = 500 + (27 − 10 − 4)·230 = 3490.
    /// Manual off → max(min(0, …), min(2695, 3490)) = 2695.
    /// Without a P1 reading only the manual reservation remains (0).
    #[test]
    fn reservation_leaves_room_for_an_eligible_ev3() {
        let (mut w, _) = quiet();
        let m = metered();
        let now = at(2026, 11, 7, 13, 50, 0);
        let s = Settings { batterij_reservering_actief: false, ..Settings::default() };
        w.p1.power_w = Some(1000.0);
        w.p1.phase_a = [Some(10.0), Some(3.0), None];
        w.solis.battery_power_w = Some(-500.0);
        w.cable.status = Some("charging".into());
        w.cable.current_a = Some(8.0);
        w.cable.set_current_a = Some(8.0);
        w.ev3.soc = Some(50.0);
        let d = compute(now, &w, &s, &m);
        assert_eq!(d.kwartier_toegestaan_vermogen_w, Some(5035.0));
        assert_eq!(d.batterij_reservering_effectief_w, 2695.0);
        w.p1.power_w = None;
        assert_eq!(compute(now, &w, &s, &m).batterij_reservering_effectief_w, 0.0);
    }

    /// `Gefactureerde piek` averages the 11 previous months (each at least 2.5 kW) with this
    /// month: for March 2026, entries 2025-04 (age 11) and 2026-02 (age 1) count, 2025-03
    /// (age 12) and 2026-03 (age 0) do not; "xx" parses as 2.5; entries without ':' are
    /// skipped. `split(':', 1)`: "2026-01:1:2" is month 2026-01 (age 2) with value "1:2",
    /// which reads 2.5. (2.5 + 4.0 + 2.5 + this month 3.0)/4 = 3.0.
    /// The bill for March: 3.0·56.0429·31/365·1.06 = 15.136… → 15.14.
    #[test]
    fn gefactureerde_piek_rolls_twelve_months() {
        let (mut w, mut m) = quiet();
        w.p1.month_peak_w = Some(3000.0);
        m.capaciteit_piekhistoriek =
            "2025-03:9.0,2025-04:xx,2026-02:4.0,2026-03:9.0,junk,2026-01:1:2,abcd-ef:3.0".into();
        let d = compute(at(2026, 3, 15, 12, 0, 0), &w, &Settings::default(), &m);
        assert_eq!(d.gefactureerde_piek_kw, 3.0);
        assert_eq!(d.capaciteitstarief_factuur_deze_maand_eur, 15.14);
    }

    #[test]
    fn str_int_follows_jinja_int() {
        assert_eq!(str_int("2025"), 2025);
        assert_eq!(str_int(" 07"), 7);
        assert_eq!(str_int("20.9"), 20);
        assert_eq!(str_int("ab"), 0);
        assert_eq!(str_int("inf"), 0);
        assert_eq!(slice("2025-11", 5, 7), "11");
        assert_eq!(slice("20", 0, 4), "20");
    }

    /// The month length from the `replace(day=28) + 4 days` trick.
    #[test]
    fn days_in_month_handles_february_and_december() {
        assert_eq!(days_in_month(at(2026, 2, 10, 12, 0, 0)), 28);
        assert_eq!(days_in_month(at(2028, 2, 10, 12, 0, 0)), 29);
        assert_eq!(days_in_month(at(2026, 12, 31, 23, 0, 0)), 31);
        assert_eq!(days_in_month(at(2026, 11, 7, 12, 0, 0)), 30);
    }

    /// `Buzz laadkost per 100km`: km = odometer − `input_number.buzz_odometer_start`. 10 100 −
    /// 10 000 = 100 km, € 5 of grid charging → € 5.00 per 100 km.
    #[test]
    fn per_100km_counts_from_the_odometer_start() {
        let (mut w, mut m) = quiet();
        let s = Settings { buzz_odometer_start: 10_000.0, ..Settings::default() };
        w.buzz.odometer_km = Some(10_100.0);
        m.meters.buzz.laadkost_net = Integrator::seeded(5.0);
        let d = compute(at(2026, 11, 7, 12, 0, 0), &w, &s, &m);
        assert_eq!(d.buzz_laadkost_per_100km_eur, 5.0);
    }

    /// `Capaciteitspiek meerkost` = `([0, tgt − peak]|max · 56.04 / 12 · 1.06)|round(2)`: a
    /// 5 kW target over a 2.5 kW peak costs 2.5 · 56.04 / 12 · 1.06 = 12.3755 → € 12.38.
    #[test]
    fn meerkost_prices_the_target_above_the_peak() {
        let (w, m) = quiet();
        let s = Settings { capaciteit_maandtarget_instelling: 5.0, ..Settings::default() };
        assert_eq!(compute(at(2026, 11, 7, 12, 0, 0), &w, &s, &m).capaciteitspiek_meerkost_eur, 12.38);
    }

    /// `Zelfverbruik vandaag`: `if pv < 0.1` is strict, so exactly 0.1 kWh of PV with 0.05
    /// injected already reads 50 %.
    #[test]
    fn self_consumption_counts_from_exactly_0_1_kwh() {
        let (w, mut m) = quiet();
        let day = Some(at(2026, 11, 7, 0, 0, 0));
        m.meters.pv_energie_dag = UtilityMeter::seeded(0.1, day);
        m.meters.injectie_dag = UtilityMeter::seeded(0.05, day);
        assert_eq!(compute(at(2026, 11, 7, 12, 0, 0), &w, &Settings::default(), &m).zelfverbruik_vandaag_pct, 50.0);
    }

    /// `Zelfvoorziening vandaag`: `eigen = max(0, pv_dag − injectie_dag)`, `use = eigen +
    /// netimport_dag`, `eigen / use · 100`. PV 10, injection 4, import 2 kWh → 6 / 8 = 75 %;
    /// no import → 100 %; more injected than produced → eigen 0 → 0 %; under 0.1 kWh in total
    /// (0.05 + 0.04) → 0 %; missing meters read 0.
    #[test]
    fn zelfvoorziening_is_own_solar_over_own_solar_plus_import() {
        assert_eq!(zelfvoorziening(Some(10.0), Some(4.0), Some(2.0)), 75.0);
        assert_eq!(zelfvoorziening(Some(10.0), Some(4.0), Some(0.0)), 100.0);
        assert_eq!(zelfvoorziening(Some(1.0), Some(1.5), Some(3.0)), 0.0);
        assert_eq!(zelfvoorziening(Some(0.05), None, Some(0.04)), 0.0);
        assert_eq!(zelfvoorziening(None, None, None), 0.0);
    }

    /// `k[5:7]` is a two-character slice: a key "2026-10x" still reads month 10, so it is one
    /// of the 11 previous months.
    #[test]
    fn the_month_is_a_two_character_slice() {
        assert_eq!(slice("2026-10x", 5, 7), "10");
        assert_eq!(slice("2026-10x", 0, 4), "2026");
    }

    /// A reservation rig at 13:50 with the quarter meter at 0 kWh since 13:45 (allowed 5035 W),
    /// manual reservation off, P1 1000 W, battery idle, worst phase `mp` A.
    fn metered() -> Memory {
        let mut m = Memory::default();
        m.meters.kwartier_import = UtilityMeter::seeded(0.0, Some(at(2026, 11, 7, 13, 45, 0)));
        m
    }

    fn resv_world(mp: f64) -> World {
        let mut w = World::default();
        w.p1.power_w = Some(1000.0);
        w.p1.phase_a = [Some(mp), Some(3.0), None];
        w.solis.battery_power_w = Some(0.0);
        w
    }

    fn resv(w: &World, s: &Settings) -> f64 {
        let m = metered();
        compute(at(2026, 11, 7, 13, 50, 0), w, s, &m).batterij_reservering_effectief_w
    }

    fn manual_off() -> Settings {
        Settings { batterij_reservering_actief: false, ..Settings::default() }
    }

    /// `Batterij reservering effectief` eligibility: `ev3_elig = wants and soc < ac_limit and
    /// ctrl`, `buzz_elig = wants and soc < laaddoel and ctrl`; an eligible car's headroom
    /// `[mx, cur + (27 − maxphase)]|min · 230` comes off `auto`. Worst phase 20 A, car at 8 of
    /// 16 A: min(16, 8 + 7) · 230 = 3450, auto = 5035 − 1000 − 3450 = 585 (phase bound
    /// 7 · 230 = 1610). A car at its charge limit is not eligible: min(4035, 1610) = 1610.
    #[test]
    fn reservation_subtracts_only_an_eligible_car_s_headroom() {
        let s = manual_off();
        let mut w = resv_world(20.0);
        w.cable.status = Some("plugged_in".into());
        w.cable.set_current_a = Some(8.0);
        w.ev3.ac_limit = Some(80.0);
        w.ev3.soc = Some(79.0);
        assert_eq!(resv(&w, &s), 585.0, "EV3 below its limit");
        w.ev3.soc = Some(80.0);
        assert_eq!(resv(&w, &s), 1610.0, "EV3 at its limit");

        let mut w = resv_world(20.0);
        w.evse.plug = Some("Connected".into());
        w.evse.override_a = Some(8.0);
        w.evse.override_max_a = Some(16.0);
        w.buzz.target_soc = Some(80.0);
        w.buzz.soc = Some(79.0);
        assert_eq!(resv(&w, &s), 585.0, "Buzz below its target");
        w.buzz.soc = Some(80.0);
        assert_eq!(resv(&w, &s), 1610.0, "Buzz at its target");
    }

    /// `phase_bound = [0, batt_chg + (27 − maxphase − ev3_need_a − buzz_need_a) · 230]|max`,
    /// `need = [[mx − cur, 0]|max, 4]|min if drawing`. Both cars drawing at 14 of 16 A (need 2
    /// each, neither eligible), battery charging 500 W, worst phase 10 A: phase_bound = 500 +
    /// (27 − 10 − 2 − 2) · 230 = 3490, below auto = 5035 − (1000 − 1840 − 2300 − 500) = 8675.
    #[test]
    fn reservation_phase_bound_leaves_each_drawing_car_its_ramp() {
        let mut w = resv_world(10.0);
        w.solis.battery_power_w = Some(-500.0);
        w.cable.status = Some("charging".into());
        w.cable.current_a = Some(8.0);
        w.cable.set_current_a = Some(14.0);
        w.ev3.soc = Some(100.0);
        w.evse.plug = Some("Connected".into());
        w.evse.state = Some("Charging".into());
        w.evse.charge_current_a = Some(10.0);
        w.evse.override_a = Some(14.0);
        w.evse.override_max_a = Some(16.0);
        w.buzz.soc = Some(100.0);
        assert_eq!(resv(&w, &manual_off()), 3490.0);
    }

    /// `bz_drawing = ev_laadvermogen_smartevse_w > 200`, strict: a Buzz at 0.87 A (200 W) is
    /// not drawing and needs no ramp, so phase_bound = 17 · 230 = 3910; its 200 W still comes
    /// off the load: auto = 5035 − (1000 − 200) = 4235.
    #[test]
    fn a_buzz_at_200_w_is_not_drawing_for_the_reservation() {
        let mut w = resv_world(10.0);
        w.evse.plug = Some("Connected".into());
        w.evse.state = Some("Charging".into());
        w.evse.charge_current_a = Some(0.87);
        w.evse.override_a = Some(8.0);
        w.evse.override_max_a = Some(16.0);
        w.buzz.soc = Some(100.0);
        let s = manual_off();
        assert_eq!(compute(at(2026, 11, 7, 13, 50, 0), &w, &s, &Memory::default()).ev_laadvermogen_smartevse_w, 200.0);
        assert_eq!(resv(&w, &s), 3910.0);
    }

    /// `load_nonsteer` adds the battery's discharge back: discharging 500 W with P1 1000 →
    /// load 1500, auto = 5035 − 1500 = 3535 (< phase bound 3910). A manual 5000 W is capped
    /// by `manual_bound = batt_chg + (27 − maxphase) · 230` = 3910.
    #[test]
    fn reservation_counts_the_discharge_as_house_load_and_caps_the_manual_one() {
        let mut w = resv_world(10.0);
        w.solis.battery_power_w = Some(500.0);
        assert_eq!(resv(&w, &manual_off()), 3535.0);
        let s = Settings { batterij_reservering_actief: true, batterij_reservering_w: 5000.0, ..Settings::default() };
        assert_eq!(resv(&w, &s), 3910.0);
    }

    /// `Kwartier toegestaan vermogen` reads `now()`, so HA re-renders it on a change of the
    /// quarter meter or the month target and at the top of each minute — not every second.
    /// Recorder 2026-11-06 16:29:30: HA still showed 5265 W (the 1.35 × 3900 cap) for 25 s
    /// after `rem <= 30` should have clamped it to 3800 W, because the P1 import register
    /// stood still. Target 3.9 kW, quarter meter 0.5 kWh:
    /// - 16:29:20, rem 40: raw (0.95 − 0.5) / (40/3600) · 1000 = 40500 → capped 5265;
    /// - 16:29:30, rem 30, nothing changed: still 5265 (fresh would be 3800);
    /// - 16:29:35, meter 0.51 kWh: re-rendered, rem 25 → clamped to 3800.
    #[test]
    fn toegestaan_keeps_its_render_until_an_input_changes() {
        let mut w = World::default();
        w.p1.month_peak_w = Some(3900.0);
        w.p1.power_w = Some(-200.0);
        let s = Settings::default();
        let mut m = Memory::default();
        m.meters.kwartier_import = UtilityMeter::seeded(0.5, Some(at(2026, 11, 6, 16, 15, 0)));
        let (d, r) = compute_with(at(2026, 11, 6, 16, 29, 20), &w, &s, &m);
        assert_eq!(d.kwartier_toegestaan_vermogen_w, Some(5265.0));
        m.template_renders = r;
        let (d, r) = compute_with(at(2026, 11, 6, 16, 29, 30), &w, &s, &m);
        assert_eq!(d.kwartier_toegestaan_vermogen_w, Some(5265.0));
        assert_eq!(
            compute(at(2026, 11, 6, 16, 29, 30), &w, &s, &Memory { template_renders: Default::default(), ..m.clone() })
                .kwartier_toegestaan_vermogen_w,
            Some(3800.0),
            "a fresh render clamps"
        );
        m.template_renders = r;
        m.meters.kwartier_import = UtilityMeter::seeded(0.51, Some(at(2026, 11, 6, 16, 15, 0)));
        let (d, _) = compute_with(at(2026, 11, 6, 16, 29, 35), &w, &s, &m);
        assert_eq!(d.kwartier_toegestaan_vermogen_w, Some(3800.0));
    }

    /// `Kwartier gemiddelde` reads P1 only in its first 30 s; after that a P1 change does not
    /// re-render it (recorder 2026-11-03 02:15:33: 2936 W held while P1 went 504 → −927 W, until
    /// 02:16:00). Quarter 0.027 kWh at 02:15:33 (elapsed 33 s): 0.027 / (33/3600) · 1000 =
    /// 2945 W; at 02:15:38 P1 changed but the value stays; the minute re-render (HA: 02:16:00.233,
    /// 1614 W) shows at the 02:16:01 tick: 0.027 / (61/3600) · 1000 = 1593 W.
    #[test]
    fn gemiddelde_stops_reading_p1_after_30_s() {
        let mut w = World::default();
        w.p1.power_w = Some(504.0);
        let s = Settings::default();
        let mut m = Memory::default();
        m.meters.kwartier_import = UtilityMeter::seeded(0.027, Some(at(2026, 11, 3, 2, 15, 0)));
        // In the first 30 s it follows P1.
        let (d, r) = compute_with(at(2026, 11, 3, 2, 15, 28), &w, &s, &m);
        assert_eq!(d.kwartier_gemiddelde_w, 504.0);
        m.template_renders = r;
        w.p1.power_w = Some(495.0);
        let (d, r) = compute_with(at(2026, 11, 3, 2, 15, 29), &w, &s, &m);
        assert_eq!(d.kwartier_gemiddelde_w, 495.0, "P1 is an input while elapsed < 30");
        m.template_renders = r;
        w.p1.power_w = Some(504.0);
        let (d, r) = compute_with(at(2026, 11, 3, 2, 15, 33), &w, &s, &m);
        assert_eq!(d.kwartier_gemiddelde_w, 2945.0);
        m.template_renders = r;
        w.p1.power_w = Some(-927.0);
        let (d, r) = compute_with(at(2026, 11, 3, 2, 15, 38), &w, &s, &m);
        assert_eq!(d.kwartier_gemiddelde_w, 2945.0, "P1 is no longer read");
        m.template_renders = r;
        let (d, r) = compute_with(at(2026, 11, 3, 2, 16, 0), &w, &s, &m);
        assert_eq!(d.kwartier_gemiddelde_w, 2945.0, "second 0: automations still see the old render");
        m.template_renders = r;
        let (d, _) = compute_with(at(2026, 11, 3, 2, 16, 1), &w, &s, &m);
        assert_eq!(d.kwartier_gemiddelde_w, 1593.0);
    }
}
