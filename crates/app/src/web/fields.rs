//! Dashboard metadata for every `Settings` field: Dutch label, input kind, unit and options.

use energie_core::settings::{BatterijNiveau, NetNiveau, Zonprioriteit};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Choice {
    /// The serde value POSTed back.
    pub value: Value,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    /// `select`, `number` or `bool`.
    pub kind: &'static str,
    pub unit: &'static str,
    pub step: f64,
    /// Range of a number, as the HA helper had it.
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub group: &'static str,
    pub options: Vec<Choice>,
}

/// The control groups, in dashboard order (as the HA dashboard groups them).
pub const MODUS: &str = "Modus & prioriteit";
pub const STURING: &str = "Sturing";
pub const DREMPELS: &str = "Instelbare drempels";
pub const AUTOS: &str = "Auto's (vergelijking)";

fn select(key: &'static str, label: &'static str, options: Vec<Choice>) -> Field {
    Field { key, label, kind: "select", unit: "", step: 0.0, min: None, max: None, group: MODUS, options }
}

/// `range` is `(min, max, step)` of the HA `input_number` the setting replaces.
fn number(
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    range: (f64, f64, f64),
    group: &'static str,
) -> Field {
    let (min, max, step) = range;
    Field { key, label, kind: "number", unit, step, min: Some(min), max: Some(max), group, options: vec![] }
}

fn toggle(key: &'static str, label: &'static str, group: &'static str) -> Field {
    Field { key, label, kind: "bool", unit: "", step: 0.0, min: None, max: None, group, options: vec![] }
}

fn choice<T: Serialize>(v: T, label: &str) -> Choice {
    Choice { value: json!(v), label: label.to_string() }
}

/// All settings, in dashboard order, with the labels of the HA dashboard.
pub fn fields() -> Vec<Field> {
    let zp = Zonprioriteit::options().into_iter().map(|z| choice(z, &z.label())).collect();
    let net =
        [NetNiveau::Uit, NetNiveau::Brug6A, NetNiveau::MaxTotPiek].into_iter().map(|n| choice(n, n.label())).collect();
    let bat = [BatterijNiveau::Uit, BatterijNiveau::Brug6A, BatterijNiveau::OntlaadBudget]
        .into_iter()
        .map(|n| choice(n, n.label()))
        .collect();
    vec![
        select("zonprioriteit", "Zonprioriteit", zp),
        select("ev_net_niveau", "Net (grid)", net),
        select("ev_batterij_niveau", "Batterij", bat),
        number("batterij_naar_auto_budget", "Batterij-dump budget", "kWh", (0.5, 60.0, 0.5), MODUS),
        toggle("pv_overschot_laden_actief", "EV-laden aan", STURING),
        toggle("peak_guard_actief", "Peak guard", STURING),
        toggle("soluna_absorptie_actief", "Batterijsturing", STURING),
        toggle("airco_overschot_koeling_actief", "Airco-dump", STURING),
        toggle("batterij_reservering_actief", "Batterij-reservering", STURING),
        number("batterij_reservering_w", "Reservering", "W", (0.0, 3000.0, 50.0), STURING),
        number("batterij_reserve_soc", "Batterij reserve (ontlaadvloer)", "%", (5.0, 90.0, 5.0), DREMPELS),
        number("capaciteit_maandtarget_instelling", "Capaciteitsdoel (optrekken)", "kW", (2.5, 12.0, 0.1), DREMPELS),
        number("goedkoop_laden_prijs_drempel", "Goedkoop laden: prijsdrempel", "€/kWh", (0.05, 0.6, 0.01), DREMPELS),
        number("buzz_laaddoel_soc", "Buzz laaddoel", "%", (50.0, 100.0, 5.0), DREMPELS),
        number("airco_dump_overschot_drempel", "Airco-dump: overschot-drempel", "W", (500.0, 4000.0, 100.0), DREMPELS),
        number("airco_dump_soc_drempel", "Airco-dump: SoC-drempel", "%", (50.0, 100.0, 1.0), DREMPELS),
        number("ev_benzineprijs", "Benzineprijs", "€/L", (0.0, 5.0, 0.01), AUTOS),
        number("ev3_ice_verbruik", "EV3 vergelijkbaar verbruik", "L/100 km", (0.0, 30.0, 0.1), AUTOS),
        number("buzz_ice_verbruik", "Buzz vergelijkbaar verbruik", "L/100 km", (0.0, 30.0, 0.1), AUTOS),
        number("ev3_odometer_start", "EV3 km-stand bij start", "km", (0.0, 1000000.0, 0.1), AUTOS),
        number("buzz_odometer_start", "Buzz km-stand bij start", "km", (0.0, 1000000.0, 0.1), AUTOS),
    ]
}

/// A derived value shown under a control (HA's "→ actief maandtarget" rows).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Readout {
    /// The control it follows.
    pub after: &'static str,
    pub label: &'static str,
    /// The `Derived` field.
    pub derived: &'static str,
    pub unit: &'static str,
    pub decimals: u32,
}

pub fn readouts() -> Vec<Readout> {
    let r = |after, label, derived, unit, decimals| Readout { after, label, derived, unit, decimals };
    vec![
        r("batterij_naar_auto_budget", "Batterij → auto: geleverd", "batterij_naar_auto_geleverd_kwh", "kWh", 2),
        r("capaciteit_maandtarget_instelling", "→ actief maandtarget", "capaciteit_maandtarget_kw", "kW", 3),
        r("capaciteit_maandtarget_instelling", "→ meerkost piek (€/maand)", "capaciteitspiek_meerkost_eur", "€", 2),
        r("batterij_reservering_w", "→ effectieve reservering", "batterij_reservering_effectief_w", "W", 0),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use energie_core::settings::Settings;

    /// Every Settings field has exactly one control on the dashboard.
    #[test]
    fn every_setting_has_one_control() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        let mut keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        let mut ours: Vec<_> = fields().iter().map(|f| f.key.to_string()).collect();
        keys.sort();
        ours.sort();
        assert_eq!(keys, ours);
    }

    /// Select options carry the serde value the settings API accepts.
    #[test]
    fn select_options_roundtrip_through_serde() {
        for f in fields().iter().filter(|f| f.kind == "select") {
            for o in &f.options {
                let mut s = serde_json::to_value(Settings::default()).unwrap();
                s[f.key] = o.value.clone();
                serde_json::from_value::<Settings>(s).unwrap();
            }
        }
        let zp = &fields()[0];
        assert_eq!(zp.options.len(), 6);
        assert_eq!(zp.options[3].label, "Buzz → Batterij → EV3");
    }

    /// Every readout follows an existing control and names a `Derived` field.
    #[test]
    fn readouts_follow_controls_and_name_derived_fields() {
        let derived = serde_json::to_value(energie_core::derived::Derived::default()).unwrap();
        let keys: Vec<_> = fields().iter().map(|f| f.key).collect();
        for r in readouts() {
            assert!(keys.contains(&r.after), "{}", r.after);
            assert!(derived.get(r.derived).is_some(), "{}", r.derived);
        }
        let groups: Vec<_> = fields().iter().map(|f| f.group).collect();
        assert_eq!(groups.iter().filter(|g| **g == MODUS).count(), 4);
    }
}
