//! `kind = "static"`: a device that always reads the values written in its config. A stand-in
//! for a device that has no driver yet, and the test double for any role:
//!
//! ```toml
//! [[device]]
//! id = "ev3"
//! kind = "static"
//! role = "car:ev3"
//! values = { plugged = true, soc = 55.0 }
//! extra = { note = "stand-in until kia_connect lands" }
//! ```
//!
//! `values` are fields of the slot's World struct (unknown ones are refused); `extra` become
//! entities of the instance.

use async_trait::async_trait;
use energie_core::command::Command;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value as Json};

use crate::driver::{Actuator, Applied, Reading, Source};
use crate::entity::{Entity, Value};
use crate::error::{DriverError, Result};
use crate::plugin::{BuildCx, Driver, DriverKind, Slot};

pub struct StaticKind;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticTable {
    /// Fields of the slot's World struct.
    #[serde(default)]
    pub values: Map<String, Json>,
    /// Extra entities: key → number, bool or string.
    #[serde(default)]
    pub extra: Map<String, Json>,
}

/// `values` laid over the all-`None` struct, so only known fields can be set.
fn overlay<T: DeserializeOwned + serde::Serialize + Default>(values: &Map<String, Json>) -> Result<T> {
    let mut base = serde_json::to_value(T::default())?.as_object().cloned().unwrap_or_default();
    for (k, v) in values {
        if !base.contains_key(k) {
            let known: Vec<&str> = base.keys().map(String::as_str).collect();
            return Err(DriverError::Config(format!("values.{k}: no such field (known: {})", known.join(", "))));
        }
        base.insert(k.clone(), v.clone());
    }
    Ok(serde_json::from_value(Json::Object(base))?)
}

/// The reading for `slot` with `values` filled in.
pub fn reading(slot: Slot, values: &Map<String, Json>) -> Result<Reading> {
    Ok(match slot {
        Slot::GridMeter => Reading::P1(overlay(values)?),
        Slot::BatteryInverter => Reading::Solis(overlay(values)?),
        Slot::Pv => Reading::Omnik(overlay(values)?),
        Slot::ChargerBuzz => Reading::Evse(overlay(values)?),
        Slot::ChargerEv3 => Reading::Cable(overlay(values)?),
        Slot::CarBuzz => Reading::Buzz(overlay(values)?),
        Slot::CarEv3 => Reading::Ev3(overlay(values)?),
        Slot::LoadAirco => Reading::Airco(overlay(values)?),
        Slot::LoadVriezer => Reading::Vriezer(overlay(values)?),
        Slot::PriceFeed => Reading::Prices(overlay(values)?),
        Slot::CapacityPeak => Reading::Fluvius(overlay(values)?),
        Slot::SolarForecast if values.is_empty() => Reading::Empty,
        Slot::SolarForecast => {
            return Err(DriverError::Config("values: solar_forecast has no fields; use extra".into()));
        }
    })
}

/// The fixed device.
pub struct Static {
    reading: Reading,
    unavailable: Reading,
    extra: Vec<Entity>,
}

#[async_trait]
impl Source for Static {
    fn name(&self) -> &'static str {
        "static"
    }
    async fn read(&mut self) -> Result<Reading> {
        Ok(self.reading.clone())
    }
    fn unavailable(&self) -> Reading {
        self.unavailable.clone()
    }
    fn entities(&self) -> Vec<Entity> {
        self.extra.clone()
    }
}

#[async_trait]
impl Actuator for Static {
    async fn apply(&mut self, _cmd: &Command) -> Result<Applied> {
        Ok(Applied::NotMine)
    }
}

fn extra(t: &StaticTable) -> Result<Vec<Entity>> {
    t.extra
        .iter()
        .map(|(k, v)| {
            let value = Value::from_json(v)
                .filter(|v| *v != Value::Null)
                .ok_or_else(|| DriverError::Config(format!("extra.{k}: a number, bool or string")))?;
            if !crate::entity::is_topic_safe(k) {
                return Err(DriverError::Config(format!("extra.{k}: keys are [a-z0-9_]")));
            }
            Ok(Entity::sensor(k.clone(), k.replace('_', " "), value))
        })
        .collect()
}

impl DriverKind for StaticKind {
    type Config = StaticTable;
    const NAME: &'static str = "static";
    const SLOTS: &'static [Slot] = &Slot::ALL;

    fn check(t: &StaticTable) -> Result<()> {
        extra(t).map(drop)
    }

    fn build(t: StaticTable, cx: &BuildCx) -> Result<Box<dyn Driver>> {
        let (reading, unavailable) = match cx.slots {
            [] => (Reading::Empty, Reading::Empty),
            [slot] => (reading(*slot, &t.values)?, slot.unavailable()),
            _ => return Err(DriverError::Config("a static device fills one slot".into())),
        };
        Ok(Box::new(Static { reading, unavailable, extra: extra(&t)? }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::factory;
    use energie_core::world::{Ev3, P1};
    use serde_json::json;

    fn table(v: Json) -> Map<String, Json> {
        v.as_object().unwrap().clone()
    }

    /// Values fill the slot's struct; unset fields stay `None`; unknown fields are refused.
    #[test]
    fn values_fill_the_slot_struct() {
        assert_eq!(
            reading(Slot::CarEv3, &table(json!({"plugged": true, "soc": 55.0}))).unwrap(),
            Reading::Ev3(Ev3 { plugged: Some(true), soc: Some(55.0), ..Ev3::default() })
        );
        let e = reading(Slot::GridMeter, &table(json!({"vermogen": 1}))).unwrap_err().to_string();
        assert!(e.contains("values.vermogen: no such field") && e.contains("power_w"), "{e}");
        assert!(reading(Slot::GridMeter, &table(json!({"power_w": "veel"}))).is_err(), "wrong type");
        for s in Slot::ALL.into_iter().filter(|s| *s != Slot::SolarForecast) {
            assert_eq!(reading(s, &Map::new()).unwrap(), s.unavailable(), "{s:?}");
        }
        assert_eq!(reading(Slot::SolarForecast, &Map::new()).unwrap(), Reading::Empty);
        assert!(reading(Slot::SolarForecast, &table(json!({"x": 1}))).is_err());
    }

    /// Built for its slot it reads its values and exposes `extra` as entities; it takes no
    /// commands; one slot at most.
    #[tokio::test]
    async fn reads_its_values_and_extras() {
        let f = factory::<StaticKind>();
        let t = json!({"values": {"power_w": 1200.0}, "extra": {"note": "x", "count": 2, "on": true}});
        f.check(&t).unwrap();
        let mut d = f.build(&t, &BuildCx { id: "m", slots: &[Slot::GridMeter], state_dir: None }).unwrap();
        assert_eq!(d.name(), "static");
        assert_eq!(d.read().await.unwrap(), Reading::P1(P1 { power_w: Some(1200.0), ..P1::default() }));
        assert_eq!(d.unavailable(), Slot::GridMeter.unavailable());
        let keys: Vec<_> = d.entities().into_iter().map(|e| (e.key, e.value)).collect();
        assert_eq!(keys[0], ("count".to_string(), Value::Number(2.0)));
        assert_eq!(keys.len(), 3);
        assert_eq!(d.apply(&Command::RefreshPrices).await.unwrap(), Applied::NotMine);
        let mut none = f.build(&json!({}), &BuildCx { id: "m", slots: &[], state_dir: None }).unwrap();
        assert_eq!((none.read().await.unwrap(), none.unavailable()), (Reading::Empty, Reading::Empty));
        let two = BuildCx { id: "m", slots: &[Slot::Pv, Slot::GridMeter], state_dir: None };
        assert!(f.build(&json!({}), &two).err().unwrap().to_string().contains("one slot"));
        assert!(f.check(&json!({"extra": {"x": [1]}})).unwrap_err().to_string().contains("extra.x"));
        assert!(f.check(&json!({"extra": {"x": null}})).is_err());
        assert!(f.check(&json!({"extra": {"Bad Key": 1}})).unwrap_err().to_string().contains("[a-z0-9_]"));
    }
}
