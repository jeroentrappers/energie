//! The common client interface: [`CloudSource`] produces readings, [`CloudActuator`] carries out
//! commands, and every client lists the HA-style entities it provides.

use async_trait::async_trait;
use energie_core::command::{AircoUnit, Command};
use energie_core::world::World;
use energie_drivers::AuthStatus;
use serde_json::{Map, Value};

use crate::cupra::BuzzReading;
use crate::error::{CloudError, Result};
use crate::fluvius::FluviusReading;
use crate::forecast_solar::SolarForecast;
use crate::kia::Ev3Reading;
use crate::lgthinq::AircoReading;

/// HA platform of an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Sensor,
    BinarySensor,
    Number,
    Switch,
}

/// One entity a client provides, named as HA names it today so dashboards and the logic keep
/// working when the source changes.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityDescriptor {
    /// `sensor.ev3_ev_battery_level`, ...
    pub entity_id: String,
    pub kind: EntityKind,
    /// HA `unit_of_measurement`.
    pub unit: Option<&'static str>,
    /// The `World` field the entity feeds (`ev3.soc`), if the logic reads it.
    pub world_field: Option<&'static str>,
    /// The client can change it (`number`, `switch`).
    pub writable: bool,
}

impl EntityDescriptor {
    pub fn new(entity_id: impl Into<String>, kind: EntityKind, unit: Option<&'static str>) -> Self {
        EntityDescriptor { entity_id: entity_id.into(), kind, unit, world_field: None, writable: false }
    }

    /// Mark the `World` field this entity feeds.
    pub fn feeds(mut self, field: &'static str) -> Self {
        self.world_field = Some(field);
        self
    }

    pub fn writable(mut self) -> Self {
        self.writable = true;
        self
    }
}

/// An entity state. `None` in [`EntityState`] is HA's `unavailable`/`unknown`.
#[derive(Debug, Clone, PartialEq)]
pub enum EntityValue {
    Number(f64),
    Bool(bool),
    Text(String),
}

pub type EntityState = (String, Option<EntityValue>);

pub(crate) fn num(id: &str, v: Option<f64>) -> EntityState {
    (id.to_string(), v.map(EntityValue::Number))
}

pub(crate) fn flag(id: &str, v: Option<bool>) -> EntityState {
    (id.to_string(), v.map(EntityValue::Bool))
}

pub(crate) fn text(id: &str, v: Option<String>) -> EntityState {
    (id.to_string(), v.map(EntityValue::Text))
}

/// One client's contribution.
#[derive(Debug, Clone, PartialEq)]
pub enum CloudReading {
    Ev3(Ev3Reading),
    Buzz(BuzzReading),
    Airco(AircoReading),
    Fluvius(FluviusReading),
    /// Display only: the logic does not read the forecast.
    Forecast(SolarForecast),
}

impl CloudReading {
    /// Write this reading into its slot of the world (the forecast has none).
    pub fn apply_to(&self, world: &mut World) {
        match self {
            CloudReading::Ev3(r) => world.ev3 = r.world(),
            CloudReading::Buzz(r) => world.buzz = r.world(),
            CloudReading::Airco(r) => world.airco = r.world(),
            CloudReading::Fluvius(r) => world.fluvius = r.world(),
            CloudReading::Forecast(_) => {}
        }
    }

    /// The reading as entity states, in the order of the client's descriptors.
    pub fn entity_states(&self) -> Vec<EntityState> {
        match self {
            CloudReading::Ev3(r) => r.entity_states(),
            CloudReading::Buzz(r) => r.entity_states(),
            CloudReading::Airco(r) => r.entity_states(),
            CloudReading::Fluvius(r) => r.entity_states(),
            CloudReading::Forecast(r) => r.entity_states(),
        }
    }
}

/// A command a cloud client can carry out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudCommand {
    /// `number.ev3_ac_charging_limit`, % (50..=100 in steps of 10).
    Ev3AcLimit(u8),
    /// `switch.airco_beneden_power` / `switch.airco_zolder_power`.
    Airco { unit: AircoUnit, on: bool },
}

impl CloudCommand {
    /// The cloud command behind a controller command, if any.
    pub fn from_command(cmd: &Command) -> Option<CloudCommand> {
        match cmd {
            Command::Airco { unit, on } => Some(CloudCommand::Airco { unit: *unit, on: *on }),
            _ => None,
        }
    }
}

/// What [`CloudActuator::apply`] did with a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// Sent to the service.
    Done,
    /// Already in that state (as far as the last reading knows); nothing sent.
    Unchanged,
    /// Not for this client.
    NotMine,
}

/// Something that can be read from the cloud.
#[async_trait]
pub trait CloudSource: Send {
    /// Short name for logs (`"kia"`, `"cupra"`, ...).
    fn name(&self) -> &'static str;

    /// The entities this client provides.
    fn entities(&self) -> Vec<EntityDescriptor>;

    /// The current reading: from the cache inside the poll interval, from the service after it.
    /// An `Err` means nothing trustworthy is known ([`CloudError::Backoff`] while backing off).
    async fn read(&mut self) -> Result<CloudReading>;

    /// The HA `unavailable` reading: every field `None`.
    fn unavailable(&self) -> CloudReading;

    /// The sign-in state, for a client that logs in with a password ([`crate::login`]).
    fn auth(&self) -> Option<AuthStatus> {
        None
    }

    /// Store new credentials in the state dir and sign in with them at once; the status says
    /// how that went. Clients without a password login take none.
    async fn set_credentials(&mut self, _fields: &Map<String, Value>) -> Result<AuthStatus> {
        Err(CloudError::Config(format!("{} takes no credentials", self.name())))
    }
}

/// Something that carries out cloud commands.
#[async_trait]
pub trait CloudActuator: Send {
    /// Carry out `cmd` if it is for this client; others return `Ok(Applied::NotMine)`.
    async fn apply(&mut self, cmd: &CloudCommand) -> Result<Applied>;
}

/// Read a source, falling back to its `unavailable` reading on error (HA semantics), with the
/// error handed back for logging.
pub async fn read_or_unavailable(source: &mut dyn CloudSource) -> (CloudReading, Option<CloudError>) {
    match source.read().await {
        Ok(r) => (r, None),
        Err(e) => (source.unavailable(), Some(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use energie_core::world::{Airco, Buzz, Ev3, Fluvius};

    struct Flaky(bool);

    #[async_trait]
    impl CloudSource for Flaky {
        fn name(&self) -> &'static str {
            "flaky"
        }
        fn entities(&self) -> Vec<EntityDescriptor> {
            vec![EntityDescriptor::new("switch.x", EntityKind::Switch, None).feeds("airco.zolder_on").writable()]
        }
        async fn read(&mut self) -> Result<CloudReading> {
            if self.0 {
                Ok(CloudReading::Airco(AircoReading { beneden_on: Some(true), zolder_on: None }))
            } else {
                Err(CloudError::Status(500))
            }
        }
        fn unavailable(&self) -> CloudReading {
            CloudReading::Airco(AircoReading::default())
        }
    }

    /// Each reading lands in its own world slot; the forecast touches nothing.
    #[test]
    fn readings_fill_their_world_slot() {
        let mut w = World::default();
        CloudReading::Ev3(Ev3Reading { soc: Some(54.5), ..Ev3Reading::default() }).apply_to(&mut w);
        CloudReading::Buzz(BuzzReading { soc: Some(43.0), ..BuzzReading::default() }).apply_to(&mut w);
        CloudReading::Airco(AircoReading { beneden_on: Some(false), zolder_on: Some(true) }).apply_to(&mut w);
        CloudReading::Fluvius(FluviusReading { peak_kw: Some(3.892), ..FluviusReading::default() }).apply_to(&mut w);
        let before = w.clone();
        CloudReading::Forecast(SolarForecast::default()).apply_to(&mut w);
        assert_eq!(w, before);
        assert_eq!(w.ev3, Ev3 { soc: Some(54.5), ..Ev3::default() });
        assert_eq!(w.buzz, Buzz { soc: Some(43.0), ..Buzz::default() });
        assert_eq!(w.airco, Airco { beneden_on: Some(false), zolder_on: Some(true) });
        assert_eq!(w.fluvius, Fluvius { peak_kw: Some(3.892) });
    }

    /// Entity states come from the reading, `None` for unknown values.
    #[test]
    fn entity_states_per_reading() {
        let all = [
            CloudReading::Ev3(Ev3Reading::default()),
            CloudReading::Buzz(BuzzReading::default()),
            CloudReading::Airco(AircoReading::default()),
            CloudReading::Fluvius(FluviusReading::default()),
            CloudReading::Forecast(SolarForecast::default()),
        ];
        for r in all {
            let states = r.entity_states();
            assert!(!states.is_empty());
            assert!(states.iter().all(|(id, v)| id.contains('.') && v.is_none()), "{states:?}");
        }
        assert_eq!(num("sensor.a", Some(1.0)).1, Some(EntityValue::Number(1.0)));
        assert_eq!(flag("binary_sensor.a", Some(true)).1, Some(EntityValue::Bool(true)));
        assert_eq!(text("sensor.a", Some("x".into())).1, Some(EntityValue::Text("x".into())));
    }

    /// Only the airco command of the controllers is a cloud command.
    #[test]
    fn cloud_commands_from_controller_commands() {
        let on = Command::Airco { unit: AircoUnit::Zolder, on: true };
        assert_eq!(CloudCommand::from_command(&on), Some(CloudCommand::Airco { unit: AircoUnit::Zolder, on: true }));
        assert_eq!(CloudCommand::from_command(&Command::RefreshPrices), None);
    }

    /// A failed read becomes the all-`None` reading, with the error handed back.
    #[tokio::test]
    async fn failed_read_is_unavailable() {
        let mut ok = Flaky(true);
        assert_eq!(ok.name(), "flaky");
        let d = &ok.entities()[0];
        assert!(d.writable && d.world_field == Some("airco.zolder_on") && d.kind == EntityKind::Switch);
        let (r, e) = read_or_unavailable(&mut ok).await;
        assert_eq!(r, CloudReading::Airco(AircoReading { beneden_on: Some(true), zolder_on: None }));
        assert!(e.is_none());
        let (r, e) = read_or_unavailable(&mut Flaky(false)).await;
        assert_eq!(r, CloudReading::Airco(AircoReading::default()));
        assert!(matches!(e, Some(CloudError::Status(500))));
        assert_eq!(ok.auth(), None);
        let e = ok.set_credentials(&Map::new()).await.unwrap_err();
        assert!(matches!(e, CloudError::Config(m) if m == "flaky takes no credentials"));
    }
}
