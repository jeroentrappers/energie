//! User-facing knobs: the HA `input_select` / `input_number` / `input_boolean` helpers that a
//! person sets on the dashboard. Defaults are the live values of 2026-11-07.

use serde::{Deserialize, Serialize};

/// One of the three consumers in the solar-surplus cascade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Consumer {
    Ev3,
    Buzz,
    Batterij,
}

impl Consumer {
    pub fn label(self) -> &'static str {
        match self {
            Consumer::Ev3 => "EV3",
            Consumer::Buzz => "Buzz",
            Consumer::Batterij => "Batterij",
        }
    }
}

/// `input_select.zonprioriteit`: the cascade order, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Zonprioriteit(pub [Consumer; 3]);

impl Zonprioriteit {
    /// Position in the list; the templates compare `zp.find('X') < zp.find('Y')`.
    pub fn rank(self, c: Consumer) -> usize {
        self.0.iter().position(|x| *x == c).expect("all three consumers are listed")
    }

    /// `zp.find(a) < zp.find(b)`: `a` is ranked above `b`.
    pub fn above(self, a: Consumer, b: Consumer) -> bool {
        self.rank(a) < self.rank(b)
    }

    /// The HA option string, e.g. `Buzz → Batterij → EV3`.
    pub fn label(self) -> String {
        self.0.iter().map(|c| c.label()).collect::<Vec<_>>().join(" → ")
    }

    /// Parse an HA option string. Returns `None` unless it names each consumer exactly once.
    pub fn parse(s: &str) -> Option<Self> {
        let parts: Vec<Consumer> = s
            .split('→')
            .map(|p| match p.trim() {
                "EV3" => Some(Consumer::Ev3),
                "Buzz" => Some(Consumer::Buzz),
                "Batterij" => Some(Consumer::Batterij),
                _ => None,
            })
            .collect::<Option<_>>()?;
        let arr: [Consumer; 3] = parts.try_into().ok()?;
        let distinct = arr[0] != arr[1] && arr[1] != arr[2] && arr[0] != arr[2];
        distinct.then_some(Zonprioriteit(arr))
    }

    /// The six HA options, in the order the select lists them.
    pub fn options() -> [Zonprioriteit; 6] {
        use Consumer::*;
        [
            Zonprioriteit([Ev3, Buzz, Batterij]),
            Zonprioriteit([Buzz, Ev3, Batterij]),
            Zonprioriteit([Ev3, Batterij, Buzz]),
            Zonprioriteit([Buzz, Batterij, Ev3]),
            Zonprioriteit([Batterij, Ev3, Buzz]),
            Zonprioriteit([Batterij, Buzz, Ev3]),
        ]
    }
}

/// `input_select.ev_net_niveau`: how much the grid may contribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetNiveau {
    Uit,
    Brug6A,
    MaxTotPiek,
}

impl NetNiveau {
    pub fn label(self) -> &'static str {
        match self {
            NetNiveau::Uit => "Uit",
            NetNiveau::Brug6A => "6A-brug",
            NetNiveau::MaxTotPiek => "Max tot piek",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [NetNiveau::Uit, NetNiveau::Brug6A, NetNiveau::MaxTotPiek].into_iter().find(|v| v.label() == s)
    }
}

/// `input_select.ev_batterij_niveau`: how much the home battery gives the car.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BatterijNiveau {
    Uit,
    Brug6A,
    OntlaadBudget,
}

impl BatterijNiveau {
    pub fn label(self) -> &'static str {
        match self {
            BatterijNiveau::Uit => "Uit",
            BatterijNiveau::Brug6A => "6A-brug",
            BatterijNiveau::OntlaadBudget => "Ontlaad-budget",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [BatterijNiveau::Uit, BatterijNiveau::Brug6A, BatterijNiveau::OntlaadBudget]
            .into_iter()
            .find(|v| v.label() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub zonprioriteit: Zonprioriteit,
    pub ev_net_niveau: NetNiveau,
    pub ev_batterij_niveau: BatterijNiveau,
    /// `input_number.goedkoop_laden_prijs_drempel`, EUR/kWh: the grid gate.
    pub goedkoop_laden_prijs_drempel: f64,
    /// `input_number.capaciteit_maandtarget_instelling`, kW: raise-only target.
    pub capaciteit_maandtarget_instelling: f64,
    /// `input_number.batterij_naar_auto_budget`, kWh.
    pub batterij_naar_auto_budget: f64,
    /// `input_number.batterij_reserve_soc`, %.
    pub batterij_reserve_soc: f64,
    /// `input_boolean.batterij_reservering_actief`.
    pub batterij_reservering_actief: bool,
    /// `input_number.batterij_reservering_w`, W.
    pub batterij_reservering_w: f64,
    /// `input_boolean.pv_overschot_laden_actief`: EV master switch.
    pub pv_overschot_laden_actief: bool,
    /// `input_boolean.peak_guard_actief`.
    pub peak_guard_actief: bool,
    /// `input_boolean.soluna_absorptie_actief`: the battery dispatcher executes the setpoint.
    pub soluna_absorptie_actief: bool,
    /// `input_boolean.airco_overschot_koeling_actief`.
    pub airco_overschot_koeling_actief: bool,
    /// `input_number.airco_dump_overschot_drempel`, W.
    pub airco_dump_overschot_drempel: f64,
    /// `input_number.airco_dump_soc_drempel`, %.
    pub airco_dump_soc_drempel: f64,
    /// `input_number.buzz_laaddoel_soc`, % (only a trigger of the grid-charge decision).
    pub buzz_laaddoel_soc: f64,
    /// `input_number.ev_benzineprijs`, EUR/L.
    pub ev_benzineprijs: f64,
    /// `input_number.ev3_ice_verbruik`, L/100 km.
    pub ev3_ice_verbruik: f64,
    /// `input_number.buzz_ice_verbruik`, L/100 km.
    pub buzz_ice_verbruik: f64,
    /// `input_number.ev3_odometer_start`, km.
    pub ev3_odometer_start: f64,
    /// `input_number.buzz_odometer_start`, km.
    pub buzz_odometer_start: f64,
}

impl Default for Settings {
    fn default() -> Self {
        use Consumer::*;
        Settings {
            zonprioriteit: Zonprioriteit([Buzz, Batterij, Ev3]),
            ev_net_niveau: NetNiveau::MaxTotPiek,
            ev_batterij_niveau: BatterijNiveau::Uit,
            goedkoop_laden_prijs_drempel: 0.38,
            capaciteit_maandtarget_instelling: 3.9,
            batterij_naar_auto_budget: 5.0,
            batterij_reserve_soc: 15.0,
            batterij_reservering_actief: true,
            batterij_reservering_w: 700.0,
            pv_overschot_laden_actief: true,
            peak_guard_actief: true,
            soluna_absorptie_actief: true,
            airco_overschot_koeling_actief: false,
            airco_dump_overschot_drempel: 800.0,
            airco_dump_soc_drempel: 98.0,
            buzz_laaddoel_soc: 100.0,
            ev_benzineprijs: 1.75,
            ev3_ice_verbruik: 6.5,
            buzz_ice_verbruik: 9.0,
            ev3_odometer_start: 1967.2,
            buzz_odometer_start: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Consumer::*;

    #[test]
    fn zonprioriteit_rank_and_label_roundtrip() {
        for zp in Zonprioriteit::options() {
            assert_eq!(Zonprioriteit::parse(&zp.label()), Some(zp));
        }
        let zp = Zonprioriteit([Buzz, Batterij, Ev3]);
        assert_eq!(zp.label(), "Buzz → Batterij → EV3");
        assert!(zp.above(Buzz, Batterij));
        assert!(zp.above(Batterij, Ev3));
        assert!(!zp.above(Ev3, Buzz));
        // `zp.find('X') < zp.find('X')` is false: nothing outranks itself.
        assert!(!zp.above(Batterij, Batterij));
        assert_eq!(zp.rank(Ev3), 2);
    }

    #[test]
    fn zonprioriteit_rejects_bad_strings() {
        assert_eq!(Zonprioriteit::parse("Buzz → Buzz → EV3"), None);
        assert_eq!(Zonprioriteit::parse("Buzz → EV3"), None);
        assert_eq!(Zonprioriteit::parse("Buzz → EV3 → Auto"), None);
        assert_eq!(Zonprioriteit::parse("Buzz → EV3 → Buzz"), None);
        assert_eq!(Zonprioriteit::parse("EV3 → Buzz → EV3"), None);
    }

    #[test]
    fn niveau_labels_roundtrip() {
        for n in [NetNiveau::Uit, NetNiveau::Brug6A, NetNiveau::MaxTotPiek] {
            assert_eq!(NetNiveau::parse(n.label()), Some(n));
        }
        for n in [BatterijNiveau::Uit, BatterijNiveau::Brug6A, BatterijNiveau::OntlaadBudget] {
            assert_eq!(BatterijNiveau::parse(n.label()), Some(n));
        }
        assert_eq!(NetNiveau::parse("x"), None);
        assert_eq!(BatterijNiveau::parse("x"), None);
    }

    #[test]
    fn defaults_are_the_live_values() {
        let s = Settings::default();
        assert_eq!(s.zonprioriteit.label(), "Buzz → Batterij → EV3");
        assert_eq!(s.ev_net_niveau, NetNiveau::MaxTotPiek);
        assert_eq!(s.goedkoop_laden_prijs_drempel, 0.38);
        let back: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(back, s);
    }
}
