//! The series the dashboard charts: the one place that says what is recorded, how each value is
//! read from a tick, which tiers keep it, and which HA entity holds the same value (to seed the
//! store from the recorder). The store itself only knows the ids; the entity registry adds the
//! series the `[history] extra_series` globs pick ([`super::History::extra_values`]), and a
//! `solar_forecast` device's `power_w` forecast becomes [`PV_FORECAST`] ([`forecasts`]).

use std::sync::{Arc, LazyLock};

use energie_core::derived::Derived;
use energie_core::memory::Memory;
use energie_core::time::Ts;
use energie_core::world::World;
use energie_drivers::entity::Entity;
use energie_drivers::plugin::Slot;

use super::{Meta, SharedHistory};
use crate::plug::DeviceView;

/// What one tick offers to read from.
pub struct Tick<'a> {
    pub world: &'a World,
    pub derived: &'a Derived,
    pub memory: &'a Memory,
}

pub type Read = fn(&Tick) -> Option<f64>;

pub struct Series {
    pub id: &'static str,
    pub label: &'static str,
    pub unit: &'static str,
    pub decimals: u32,
    pub quarter: bool,
    pub day: bool,
    /// The HA entity with the same value in the same unit.
    pub ha: Option<&'static str>,
    /// `None`: not sampled; its points come from outside (the solar forecast hook).
    pub read: Option<Read>,
}

/// Which tiers besides recent and minute.
#[derive(Clone, Copy)]
enum Keep {
    Minute,
    Quarter,
    Day,
    Both,
}

fn s(
    id: &'static str,
    label: &'static str,
    unit: &'static str,
    decimals: u32,
    keep: Keep,
    ha: Option<&'static str>,
    read: Read,
) -> Series {
    let (quarter, day) = match keep {
        Keep::Minute => (false, false),
        Keep::Quarter => (true, false),
        Keep::Day => (false, true),
        Keep::Both => (true, true),
    };
    Series { id, label, unit, decimals, quarter, day, ha, read: Some(read) }
}

/// The id of the solar forecast series, set through [`super::History::set_forecast`] from the
/// `solar_forecast` device (the dashboard shows the line once one has a forecast).
pub const PV_FORECAST: &str = "pv_forecast_w";

/// Every series, in store order.
pub static SERIES: LazyLock<Vec<Series>> = LazyLock::new(|| {
    use Keep::*;
    vec![
        s("p1_w", "Net (+afn/−inj)", "W", 0, Quarter, Some("sensor.p1_meter_vermogen"), |t| t.world.p1.power_w),
        s("afname_w", "Actueel (afname)", "W", 0, Minute, None, |t| t.world.p1.power_w.map(|p| p.max(0.0))),
        s("pv_w", "Zon", "W", 0, Quarter, Some("sensor.pv_vermogen_totaal"), |t| Some(t.derived.pv_vermogen_totaal_w)),
        s("solis_pv_w", "Solis", "W", 0, Quarter, Some("sensor.solis_s6_eh1p_total_pv_power_2"), |t| {
            t.world.solis.pv_power_w
        }),
        s(
            "omnik_w",
            "Omnik",
            "W",
            0,
            Quarter,
            Some("sensor.omnik_hoofddak_inverter_omnik_hoofddak_current_power_production"),
            |t| t.world.omnik.power_w,
        ),
        s(
            "batterij_w",
            "Batterij (+ontl/−laad)",
            "W",
            0,
            Quarter,
            Some("sensor.solis_s6_eh1p_battery_power_net_2"),
            |t| t.world.solis.battery_power_w,
        ),
        s("soc_pct", "SOC", "%", 1, Both, Some("sensor.solis_s6_eh1p_battery_soc_2"), |t| t.world.solis.soc),
        s("huis_w", "Huis", "W", 0, Quarter, Some("sensor.huisverbruik_vermogen"), |t| {
            Some(t.derived.huisverbruik_vermogen_w)
        }),
        s("huis_basis_w", "Huis (basis)", "W", 0, Quarter, Some("sensor.huisverbruik_excl_stuurbaar"), |t| {
            Some(t.derived.huisverbruik_excl_stuurbaar_w)
        }),
        s("ev3_w", "EV3", "W", 0, Quarter, Some("sensor.ev_laadkabel_vermogen_w"), |t| {
            Some(t.derived.ev_laadkabel_vermogen_w)
        }),
        s("buzz_w", "Buzz", "W", 0, Quarter, Some("sensor.ev_laadvermogen_smartevse_w"), |t| {
            Some(t.derived.ev_laadvermogen_smartevse_w)
        }),
        s("batterij_setpoint_w", "Batterijdoel", "W", 0, Minute, Some("sensor.batterij_setpoint"), |t| {
            Some(t.derived.batterij_setpoint_w)
        }),
        s(
            "kwartier_gemiddelde_w",
            "Gemiddelde dit kwartier",
            "W",
            0,
            Minute,
            Some("sensor.kwartier_gemiddelde"),
            |t| Some(t.derived.kwartier_gemiddelde_w),
        ),
        s("kwartier_projectie_w", "Projectie", "W", 0, Minute, Some("sensor.kwartiervermogen_projectie"), |t| {
            Some(t.derived.kwartiervermogen_projectie_w)
        }),
        s(
            "kwartier_vastgelegd_kw",
            "Kwartiervermogen",
            "kW",
            3,
            Both,
            Some("sensor.kwartiervermogen_vastgelegd"),
            |t| Some(t.derived.kwartiervermogen_vastgelegd_kw),
        ),
        s("maandtarget_kw", "Target (Fluvius)", "kW", 3, Both, Some("sensor.capaciteit_maandtarget"), |t| {
            Some(t.derived.capaciteit_maandtarget_kw)
        }),
        s("toegestaan_w", "Mag nu", "W", 0, Minute, Some("sensor.kwartier_toegestaan_vermogen"), |t| {
            t.derived.kwartier_toegestaan_vermogen_w
        }),
        s("kost_per_uur_eur", "Kost-snelheid", "€/u", 4, Quarter, Some("sensor.kost_per_uur"), |t| {
            Some(t.derived.kost_per_uur_eur)
        }),
        s("energiekost_vandaag_eur", "Energiekost vandaag", "€", 3, Both, Some("sensor.energiekost_vandaag"), |t| {
            t.derived.energiekost_vandaag_eur
        }),
        s("afname_prijs", "Afname (all-in)", "€/kWh", 5, Quarter, Some("sensor.stroomprijs_nu"), |t| {
            t.derived.stroomprijs_nu
        }),
        s(
            "injectie_prijs",
            "Injectie",
            "€/kWh",
            5,
            Quarter,
            Some("sensor.injectie_current_electricity_market_price"),
            |t| t.world.prices.injectie_now,
        ),
        s("maandpiek_w", "Maandpiek", "W", 0, Day, Some("sensor.p1_meter_piekverbruik_huidige_maand"), |t| {
            t.world.p1.month_peak_w
        }),
        s("pv_dag_kwh", "PV productie", "kWh", 3, Day, Some("sensor.pv_energie_dag"), |t| t.derived.pv_energie_dag_kwh),
        s("huis_dag_kwh", "Verbruik", "kWh", 3, Day, Some("sensor.huisverbruik_dag"), |t| {
            t.derived.huisverbruik_dag_kwh
        }),
        s("import_dag_kwh", "Netimport", "kWh", 3, Day, Some("sensor.netimport_dag"), |t| t.derived.netimport_dag_kwh),
        s("injectie_dag_kwh", "Injectie", "kWh", 3, Day, Some("sensor.injectie_dag"), |t| t.derived.injectie_dag_kwh),
        s("batterij_geladen_dag_kwh", "Batterij geladen", "kWh", 3, Day, Some("sensor.batterij_geladen_dag"), |t| {
            t.derived.batterij_geladen_dag_kwh
        }),
        s("batterij_ontladen_dag_kwh", "Batterij ontladen", "kWh", 3, Day, Some("sensor.batterij_ontladen_dag"), |t| {
            t.derived.batterij_ontladen_dag_kwh
        }),
        s("zelfverbruik_pct", "Zelfverbruik (van PV)", "%", 0, Day, Some("sensor.zelfverbruik_vandaag"), |t| {
            Some(t.derived.zelfverbruik_vandaag_pct)
        }),
        s(
            "zelfvoorziening_pct",
            "Zelfvoorziening (van verbruik)",
            "%",
            0,
            Day,
            Some("sensor.zelfvoorziening_vandaag"),
            |t| Some(t.derived.zelfvoorziening_vandaag_pct),
        ),
        s("ev3_laadenergie_dag_kwh", "EV3 geladen", "kWh", 3, Day, Some("sensor.ev3_laadenergie_dag"), |t| {
            t.derived.ev3_laadenergie_dag_kwh
        }),
        s("ev3_laadkost_dag_eur", "EV3 netkost", "€", 3, Day, Some("sensor.ev3_laadkost_dag"), |t| {
            t.derived.ev3_laadkost_dag_eur
        }),
        s("buzz_laadenergie_dag_kwh", "Buzz geladen", "kWh", 3, Day, Some("sensor.buzz_laadenergie_dag"), |t| {
            t.derived.buzz_laadenergie_dag_kwh
        }),
        s("buzz_laadkost_dag_eur", "Buzz netkost", "€", 3, Day, Some("sensor.buzz_laadkost_dag"), |t| {
            t.derived.buzz_laadkost_dag_eur
        }),
        s("nachtdekking_pct", "Nachtdekking", "%", 0, Day, Some("input_number.nachtdekking"), |t| {
            Some(t.memory.nachtdekking)
        }),
        s("vriezer_w", "Vriezer", "W", 0, Minute, Some("sensor.vriezer_vermogen"), |t| t.world.vriezer.power_w),
        Series {
            id: PV_FORECAST,
            label: "PV forecast",
            unit: "W",
            decimals: 0,
            quarter: false,
            day: false,
            ha: None,
            read: None,
        },
    ]
});

/// The store's view of [`SERIES`].
pub fn meta() -> Vec<Meta> {
    SERIES
        .iter()
        .map(|s| Meta {
            id: s.id.into(),
            label: s.label.into(),
            unit: s.unit.into(),
            decimals: s.decimals,
            sampled: s.read.is_some(),
            quarter: s.quarter,
            day: s.day,
        })
        .collect()
}

/// One value per series (store order) from a tick.
pub fn sample(t: &Tick) -> Vec<Option<f64>> {
    SERIES.iter().map(|s| s.read.and_then(|r| r(t))).collect()
}

/// Record one tick: the curated series, then the entities the `extra_series` globs pick.
pub fn record(h: &SharedHistory, now: Ts, world: &World, derived: &Derived, memory: &Memory, entities: &[Entity]) {
    let mut values = sample(&Tick { world, derived, memory });
    let mut h = h.lock().expect("history lock");
    h.extra_values(entities, &mut values);
    h.record(now.timestamp(), &values);
}

/// The forecast key of a `solar_forecast` device that becomes [`PV_FORECAST`].
pub const FORECAST_KEY: &str = "power_w";

/// Hand the `solar_forecast` devices' power forecast to the store (the last one wins).
pub fn forecasts(h: &SharedHistory, devices: &[Arc<dyn DeviceView>]) {
    for d in devices.iter().filter(|d| d.slots().contains(&Slot::SolarForecast)) {
        for (key, points) in d.forecasts() {
            if key == FORECAST_KEY {
                h.lock().expect("history lock").set_forecast(PV_FORECAST, points).expect("a curated series");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{History, Retention};
    use crate::plug::tests::t0;
    use std::sync::{Arc, Mutex};

    /// Ids are unique and every sampled series reads its value from the tick.
    #[test]
    fn every_series_reads_from_the_tick() {
        let mut ids: Vec<_> = SERIES.iter().map(|s| s.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), SERIES.len());
        let mut w = World::default();
        w.p1.power_w = Some(-1200.0);
        w.p1.month_peak_w = Some(3892.0);
        w.solis.pv_power_w = Some(2000.0);
        w.solis.battery_power_w = Some(-500.0);
        w.solis.soc = Some(60.0);
        w.omnik.power_w = Some(700.0);
        w.prices.injectie_now = Some(0.21);
        w.vriezer.power_w = Some(80.0);
        let d = Derived {
            pv_vermogen_totaal_w: 2700.0,
            huisverbruik_vermogen_w: 1000.0,
            kwartier_toegestaan_vermogen_w: Some(4100.0),
            stroomprijs_nu: Some(0.3),
            energiekost_vandaag_eur: Some(1.5),
            ..Default::default()
        };
        let m = Memory { nachtdekking: 80.0, ..Default::default() };
        let v = sample(&Tick { world: &w, derived: &d, memory: &m });
        let get = |id: &str| v[SERIES.iter().position(|s| s.id == id).unwrap()];
        assert_eq!(get("p1_w"), Some(-1200.0));
        assert_eq!(get("afname_w"), Some(0.0));
        assert_eq!(get("pv_w"), Some(2700.0));
        assert_eq!(get("toegestaan_w"), Some(4100.0));
        assert_eq!(get("nachtdekking_pct"), Some(80.0));
        assert_eq!(get("vriezer_w"), Some(80.0));
        assert_eq!(get(PV_FORECAST), None);
        assert_eq!(v.iter().filter(|x| x.is_some()).count(), 26);
        let meta = meta();
        assert_eq!(meta.len(), SERIES.len());
        assert!(!meta.last().unwrap().sampled);
        assert!(meta.iter().filter(|m| m.quarter).count() <= 16, "the quarter tier stays small");
    }

    struct Forecaster(Slot);
    impl DeviceView for Forecaster {
        fn id(&self) -> &str {
            "fc"
        }
        fn kind(&self) -> &str {
            "forecast_solar"
        }
        fn slots(&self) -> &[Slot] {
            std::slice::from_ref(&self.0)
        }
        fn world(&self) -> World {
            World::default()
        }
        fn extra(&self) -> Vec<Entity> {
            Vec::new()
        }
        fn health(&self) -> crate::plug::DriverHealth {
            crate::plug::DriverHealth::default()
        }
        fn forecasts(&self) -> Vec<(String, Vec<(i64, f64)>)> {
            vec![("power_w".into(), vec![(900, 1200.0)]), ("energy_wh".into(), vec![(900, 5.0)])]
        }
    }

    /// The `power_w` forecast of a `solar_forecast` device becomes the PV forecast series;
    /// other keys and other devices' forecasts are not used.
    #[test]
    fn the_solar_forecast_device_feeds_the_forecast_series() {
        let ret = Retention { dir: None, minute_s: 86_400, quarter_s: 86_400, day_s: 86_400 };
        let h: SharedHistory = Arc::new(Mutex::new(History::open(meta(), &ret, 0).unwrap()));
        let car: Arc<dyn DeviceView> = Arc::new(Forecaster(Slot::CarEv3));
        assert_eq!(
            (car.id(), car.kind(), car.world(), car.extra(), car.health().available),
            ("fc", "forecast_solar", World::default(), vec![], false)
        );
        forecasts(&h, &[car]);
        let want = [(PV_FORECAST.to_string(), crate::history::tier::Pick::Mean)];
        assert_eq!(h.lock().unwrap().query(&want, 0, 1800, 900).unwrap().series[0].v, vec![None, None]);
        forecasts(&h, &[Arc::new(Forecaster(Slot::SolarForecast))]);
        assert_eq!(h.lock().unwrap().query(&want, 0, 1800, 900).unwrap().series[0].v, vec![None, Some(1200.0)]);
    }

    /// `record` feeds the store at the tick's time.
    #[test]
    fn record_feeds_the_store() {
        let ret = Retention { dir: None, minute_s: 86_400, quarter_s: 86_400, day_s: 86_400 };
        let h: SharedHistory = Arc::new(Mutex::new(History::open(meta(), &ret, 0).unwrap()));
        let (w, d, m) = (World::default(), Derived::default(), Memory::default());
        record(&h, t0(), &w, &d, &m, &[]);
        assert_eq!(h.lock().unwrap().stats.samples, 1);
    }
}
