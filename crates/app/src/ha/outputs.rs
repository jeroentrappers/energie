//! The HA entities that are the *outputs* of the logic: every `Derived` field and the HA
//! entity that shows the same value, with the default comparison tolerance. The shadow
//! comparator checks Rust's `Derived` against these.

use serde_json::Value;

use super::entity::{EntityState, parse_time};
use crate::config::Tolerance;
use crate::shadow::Val;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Num,
    /// `true` when the HA state equals this string (`on`, `above_horizon`).
    Bool(&'static str),
    /// Compared case-insensitively (`buzz` vs `Buzz`).
    Text,
    /// A timestamp, compared in epoch seconds.
    Time,
    /// `true` when the entity has a value (`has_value`), whatever it is.
    Available,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Output {
    /// The `Derived` field (its serde name).
    pub field: &'static str,
    pub entity: &'static str,
    /// Compare this attribute instead of the state.
    pub attr: Option<&'static str>,
    pub kind: Kind,
    pub tol: Tolerance,
}

impl Output {
    /// HA's value, or `None` when HA does not have the entity at all.
    pub fn ha_val(&self, e: Option<&EntityState>) -> Option<Val> {
        let e = e?;
        let raw: Option<String> = match self.attr {
            Some(a) => e.attr(a).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())),
            None => e.value().map(str::to_string),
        };
        Some(match (self.kind, raw) {
            (Kind::Available, raw) => Val::Bool(raw.is_some()),
            (_, None) => Val::Null,
            (Kind::Num, Some(s)) => s.trim().parse::<f64>().map(Val::Num).unwrap_or(Val::Null),
            (Kind::Bool(on), Some(s)) => Val::Bool(s == on),
            (Kind::Text, Some(s)) => Val::Text(s.to_lowercase()),
            (Kind::Time, Some(s)) => {
                parse_time(&s).map(|t| Val::Num(energie_core::time::epoch(t))).unwrap_or(Val::Null)
            }
        })
    }

    /// Rust's value of this field from the serialised `Derived`.
    pub fn rust_val(&self, v: Option<&Value>) -> Val {
        match v {
            Some(Value::Number(n)) => n.as_f64().map(Val::Num).unwrap_or(Val::Null),
            Some(Value::Bool(b)) => Val::Bool(*b),
            Some(Value::String(s)) if self.kind == Kind::Time => {
                parse_time(s).map(|t| Val::Num(energie_core::time::epoch(t))).unwrap_or(Val::Null)
            }
            Some(Value::String(s)) => Val::Text(s.to_lowercase()),
            _ => Val::Null,
        }
    }
}

/// Default tolerance from the unit suffix of the field; cumulative meters also get 0.1 %.
fn default_tol(field: &str, kind: Kind) -> Tolerance {
    let abs = if kind == Kind::Time {
        1.0
    } else if field.ends_with("_kwh") {
        0.01
    } else if field.ends_with("_kw") {
        0.005
    } else if field.ends_with("_w") {
        5.0
    } else if field.ends_with("_eur") {
        0.01
    } else if field.ends_with("_pct") {
        0.5
    } else {
        0.001
    };
    let meter = field.ends_with("_kwh") || field.ends_with("_eur") && !field.contains("per_");
    Tolerance { abs, rel: if meter { 0.001 } else { 0.0 } }
}

fn o(field: &'static str, entity: &'static str, kind: Kind) -> Output {
    Output { field, entity, attr: None, kind, tol: default_tol(field, kind) }
}

/// A per-100 km figure or power: a ratio, not a meter, so no relative allowance.
fn ratio(mut out: Output) -> Output {
    out.tol.rel = 0.0;
    out
}

/// The nine per-car sensors (`sensor.<car>_laadvermogen_zon` ...).
macro_rules! car_outputs {
    ($car:literal) => {
        [
            o(concat!($car, "_laadvermogen_zon_kw"), concat!("sensor.", $car, "_laadvermogen_zon"), Kind::Num),
            o(concat!($car, "_laadvermogen_net_kw"), concat!("sensor.", $car, "_laadvermogen_net"), Kind::Num),
            o(concat!($car, "_laadvermogen_totaal_kw"), concat!("sensor.", $car, "_laadvermogen_totaal"), Kind::Num),
            o(concat!($car, "_laadkost_per_uur_eur"), concat!("sensor.", $car, "_laadkost_per_uur"), Kind::Num),
            o(concat!($car, "_laad_zonaandeel_pct"), concat!("sensor.", $car, "_laad_zonaandeel"), Kind::Num),
            o(concat!($car, "_laadkost_per_100km_eur"), concat!("sensor.", $car, "_laadkost_per_100km"), Kind::Num),
            ratio(o(
                concat!($car, "_verbruik_per_100km_kwh"),
                concat!("sensor.", $car, "_verbruik_per_100km"),
                Kind::Num,
            )),
            o(concat!($car, "_ice_kost_per_100km_eur"), concat!("sensor.", $car, "_ice_kost_per_100km"), Kind::Num),
            o(concat!($car, "_besparing_per_100km_eur"), concat!("sensor.", $car, "_besparing_per_100km"), Kind::Num),
        ]
    };
}

/// One row per `Derived` field.
pub fn outputs() -> Vec<Output> {
    use Kind::*;
    let mut v = vec![
        o("sun_above_horizon", "sun.sun", Bool("above_horizon")),
        o("kwartier_import_kwh", "sensor.p1_meter_kwartier_import", Num),
        Output {
            field: "kwartier_next_reset",
            entity: "sensor.p1_meter_kwartier_import",
            attr: Some("next_reset"),
            kind: Time,
            tol: default_tol("kwartier_next_reset", Time),
        },
        o("p1_mediaan_w", "sensor.p1_meter_p1_mediaan_90s", Num),
        o("batterij_ontladen_energie_kwh", "sensor.batterij_ontladen_energie", Num),
        o("batterij_geladen_energie_kwh", "sensor.batterij_geladen_energie", Num),
        o("pv_energie_totaal_kwh", "sensor.pv_energie_totaal", Num),
        o("huisverbruik_energie_kwh", "sensor.huisverbruik_energie", Num),
        o("energiekost_cumulatief_eur", "sensor.energiekost_cumulatief", Num),
        o("ev3_laadenergie_zon_kwh", "sensor.ev3_laadenergie_zon", Num),
        o("ev3_laadenergie_net_kwh", "sensor.ev3_laadenergie_net", Num),
        o("ev3_laadenergie_totaal_kwh", "sensor.ev3_laadenergie_totaal", Num),
        o("ev3_laadkost_net_eur", "sensor.ev3_laadkost_net", Num),
        o("buzz_laadenergie_zon_kwh", "sensor.buzz_laadenergie_zon", Num),
        o("buzz_laadenergie_net_kwh", "sensor.buzz_laadenergie_net", Num),
        o("buzz_laadenergie_totaal_kwh", "sensor.buzz_laadenergie_totaal", Num),
        o("buzz_laadkost_net_eur", "sensor.buzz_laadkost_net", Num),
        o("pv_energie_dag_kwh", "sensor.pv_energie_dag", Num),
        o("huisverbruik_dag_kwh", "sensor.huisverbruik_dag", Num),
        o("netimport_dag_kwh", "sensor.netimport_dag", Num),
        o("injectie_dag_kwh", "sensor.injectie_dag", Num),
        o("batterij_geladen_dag_kwh", "sensor.batterij_geladen_dag", Num),
        o("batterij_ontladen_dag_kwh", "sensor.batterij_ontladen_dag", Num),
        o("energiekost_vandaag_eur", "sensor.energiekost_vandaag", Num),
        o("ev3_laadkost_dag_eur", "sensor.ev3_laadkost_dag", Num),
        o("ev3_laadkost_maand_eur", "sensor.ev3_laadkost_maand", Num),
        o("ev3_laadenergie_dag_kwh", "sensor.ev3_laadenergie_dag", Num),
        o("ev3_laadenergie_maand_kwh", "sensor.ev3_laadenergie_maand", Num),
        o("buzz_laadkost_dag_eur", "sensor.buzz_laadkost_dag", Num),
        o("buzz_laadkost_maand_eur", "sensor.buzz_laadkost_maand", Num),
        o("buzz_laadenergie_dag_kwh", "sensor.buzz_laadenergie_dag", Num),
        o("buzz_laadenergie_maand_kwh", "sensor.buzz_laadenergie_maand", Num),
        o("kwartiervermogen_vastgelegd_kw", "sensor.kwartiervermogen_vastgelegd", Num),
        o("kwartiervermogen_projectie_w", "sensor.kwartiervermogen_projectie", Num),
        o("pv_vermogen_totaal_w", "sensor.pv_vermogen_totaal", Num),
        o("huisverbruik_vermogen_w", "sensor.huisverbruik_vermogen", Num),
        o("batterij_beschikbare_energie_kwh", "sensor.batterij_beschikbare_energie", Num),
        o("stroomprijs_niveau", "sensor.stroomprijs_niveau", Text),
        o("capaciteitspiek_risico", "binary_sensor.capaciteitspiek_risico", Bool("on")),
        o("kwartierprijs_volgend", "sensor.kwartierprijs_volgend", Num),
        o("vaste_kosten_per_maand_eur", "sensor.vaste_kosten_per_maand", Num),
        o("capaciteitstarief_deze_maand_eur", "sensor.capaciteitstarief_deze_maand", Num),
        o("pv_overschot_beschikbaar_w", "sensor.pv_overschot_beschikbaar", Num),
        o("huisverbruik_excl_stuurbaar_w", "sensor.huisverbruik_excl_stuurbaar", Num),
        o("piek_verschil_fluvius_vs_p1_w", "sensor.piek_verschil_fluvius_vs_p1", Num),
        o("capaciteit_maandtarget_kw", "sensor.capaciteit_maandtarget", Num),
        o("pv_bruto_overschot_w", "sensor.pv_bruto_overschot", Num),
        o("kost_per_uur_eur", "sensor.kost_per_uur", Num),
        o("solis_offline", "binary_sensor.solis_offline", Bool("on")),
        o("ev_actieve_laadauto", "sensor.ev_actieve_laadauto", Text),
        o("ev_laadvermogen_zon_kw", "sensor.ev_laadvermogen_zon", Num),
        o("ev_laadvermogen_net_kw", "sensor.ev_laadvermogen_net", Num),
    ];
    v.extend(car_outputs!("ev3"));
    v.extend(car_outputs!("buzz"));
    v.extend([
        o("ev_laadkabel_vermogen_w", "sensor.ev_laadkabel_vermogen_w", Num),
        o("ev_laadvermogen_smartevse_kw", "sensor.ev_laadvermogen_smartevse", Num),
        o("ev_laadvermogen_smartevse_w", "sensor.ev_laadvermogen_smartevse_w", Num),
        o("ev_laadvermogen_smartevse_beschikbaar", "sensor.ev_laadvermogen_smartevse", Available),
        o("injectie_effectieve_terugleververgoeding", "sensor.injectie_effectieve_terugleververgoeding", Num),
        o("batterij_naar_auto_geleverd_kwh", "sensor.batterij_naar_auto_geleverd_kwh", Num),
        o("capaciteitspiek_meerkost_eur", "sensor.capaciteitspiek_meerkost", Num),
        o("batterij_ontlaadvermogen_w", "sensor.batterij_ontlaadvermogen", Num),
        o("batterij_laadvermogen_w", "sensor.batterij_laadvermogen", Num),
        o("zelfverbruik_vandaag_pct", "sensor.zelfverbruik_vandaag", Num),
        o("zelfvoorziening_vandaag_pct", "sensor.zelfvoorziening_vandaag", Num),
        o("zelfverbruik_vandaag_kwh", "sensor.zelfverbruik_vandaag_kwh", Num),
        o("kwartier_toegestaan_vermogen_w", "sensor.kwartier_toegestaan_vermogen", Num),
        o("kwartier_gemiddelde_w", "sensor.kwartier_gemiddelde", Num),
        o("kwartier_index", "sensor.kwartier_index", Num),
        o("batterij_reservering_effectief_w", "sensor.batterij_reservering_effectief", Num),
        o("stroomprijs_nu", "sensor.stroomprijs_nu", Num),
        o("gefactureerde_piek_kw", "sensor.gefactureerde_piek", Num),
        o("capaciteitstarief_factuur_deze_maand_eur", "sensor.capaciteitstarief_factuur_deze_maand", Num),
        o("batterij_setpoint_w", "sensor.batterij_setpoint", Num),
        o("dispatcher_off_target", "binary_sensor.dispatcher_off_target", Bool("on")),
    ]);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::mapping::tests::fixture;
    use energie_core::derived::Derived;
    use serde_json::json;

    /// Every `Derived` field has exactly one output row, in the struct's order.
    #[test]
    fn every_derived_field_has_one_output() {
        let json = serde_json::to_value(Derived::default()).unwrap();
        let fields: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        let mut rows: Vec<_> = outputs().iter().map(|o| o.field.to_string()).collect();
        let mut sorted = fields.clone();
        sorted.sort();
        rows.sort();
        assert_eq!(rows, sorted);
    }

    /// `Available` compares `has_value`: any state counts, `unavailable`/`unknown` do not.
    #[test]
    fn available_compares_has_value() {
        let o = outputs().into_iter().find(|o| o.field == "ev_laadvermogen_smartevse_beschikbaar").unwrap();
        assert_eq!(o.kind, Kind::Available);
        let up = EntityState::new("sensor.ev_laadvermogen_smartevse", "0.0");
        let down = EntityState::new("sensor.ev_laadvermogen_smartevse", "unavailable");
        assert_eq!(o.ha_val(Some(&up)), Some(Val::Bool(true)));
        assert_eq!(o.ha_val(Some(&down)), Some(Val::Bool(false)));
        assert_eq!(o.ha_val(None), None);
    }

    /// Every output entity exists in the live HA.
    #[test]
    fn every_output_entity_exists_in_the_live_snapshot() {
        let f = fixture();
        let missing: Vec<_> = outputs().iter().filter(|o| !f.contains_key(o.entity)).map(|o| o.entity).collect();
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// HA values read typed: numbers, binary states, lower-cased text and timestamps.
    #[test]
    fn ha_values_read_typed_from_the_live_snapshot() {
        let f = fixture();
        let get = |field: &str| {
            let o = outputs().into_iter().find(|o| o.field == field).unwrap();
            o.ha_val(f.get(o.entity))
        };
        assert_eq!(get("kwartiervermogen_projectie_w"), Some(Val::Num(3085.0)));
        assert_eq!(get("sun_above_horizon"), Some(Val::Bool(true)));
        assert_eq!(get("capaciteitspiek_risico"), Some(Val::Bool(false)));
        assert_eq!(get("ev_actieve_laadauto"), Some(Val::Text("buzz".into())));
        let next = parse_time("2026-11-07T14:00:00+01:00").unwrap();
        assert_eq!(get("kwartier_next_reset"), Some(Val::Num(energie_core::time::epoch(next))));
    }

    /// Missing entities, unavailable states and bad numbers are told apart.
    #[test]
    fn missing_unavailable_and_garbage() {
        let num = o("x_w", "sensor.x", Kind::Num);
        assert_eq!(num.ha_val(None), None);
        assert_eq!(num.ha_val(Some(&EntityState::new("sensor.x", "unavailable"))), Some(Val::Null));
        assert_eq!(num.ha_val(Some(&EntityState::new("sensor.x", "abc"))), Some(Val::Null));
        let t = Output { attr: Some("next_reset"), ..o("t", "sensor.x", Kind::Time) };
        let e = EntityState::new("sensor.x", "1").with_attr("next_reset", json!("nooit"));
        assert_eq!(t.ha_val(Some(&e)), Some(Val::Null));
        let e = EntityState::new("sensor.x", "1").with_attr("next_reset", json!(5));
        assert_eq!(t.ha_val(Some(&e)), Some(Val::Null));
    }

    /// Rust values come from the serialised `Derived` with the same typing.
    #[test]
    fn rust_values_from_serialised_derived() {
        let num = o("x_w", "sensor.x", Kind::Num);
        assert_eq!(num.rust_val(Some(&json!(5.5))), Val::Num(5.5));
        assert_eq!(num.rust_val(Some(&json!(true))), Val::Bool(true));
        assert_eq!(num.rust_val(Some(&json!("Buzz"))), Val::Text("buzz".into()));
        assert_eq!(num.rust_val(Some(&Value::Null)), Val::Null);
        assert_eq!(num.rust_val(None), Val::Null);
        let t = o("t", "sensor.x", Kind::Time);
        assert_eq!(t.rust_val(Some(&json!("1970-01-01T00:00:10+00:00"))), Val::Num(10.0));
        assert_eq!(t.rust_val(Some(&json!("x"))), Val::Null);
    }

    /// Defaults follow the unit suffix; cumulative meters also get a relative allowance.
    #[test]
    fn default_tolerances_follow_units() {
        assert_eq!(default_tol("a_w", Kind::Num), Tolerance { abs: 5.0, rel: 0.0 });
        assert_eq!(default_tol("a_kw", Kind::Num), Tolerance { abs: 0.005, rel: 0.0 });
        assert_eq!(default_tol("a_kwh", Kind::Num), Tolerance { abs: 0.01, rel: 0.001 });
        assert_eq!(default_tol("a_eur", Kind::Num), Tolerance { abs: 0.01, rel: 0.001 });
        assert_eq!(default_tol("a_per_uur_eur", Kind::Num), Tolerance { abs: 0.01, rel: 0.0 });
        assert_eq!(default_tol("a_pct", Kind::Num), Tolerance { abs: 0.5, rel: 0.0 });
        assert_eq!(default_tol("a", Kind::Num), Tolerance { abs: 0.001, rel: 0.0 });
        assert_eq!(default_tol("a", Kind::Time), Tolerance { abs: 1.0, rel: 0.0 });
    }
}
