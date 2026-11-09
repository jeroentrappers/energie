//! The common driver interface: [`Source`] produces readings, [`Actuator`] carries out commands.

use async_trait::async_trait;
use energie_core::command::Command;
use energie_core::world::{Airco, Buzz, Cable, Ev3, Evse, Fluvius, Omnik, P1, Prices, Solis, Vriezer, World};
use serde::Serialize;

use crate::auth::AuthStatus;
use crate::entity::{Entity, Value};
use crate::error::{DriverError, Result};

/// One device's contribution to the [`World`]: the typed data of one role slot
/// ([`crate::plugin::Slot::unavailable`] names the variant each slot takes).
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    P1(P1),
    Solis(Solis),
    /// A PV inverter (the core type is named after the first one, the Omnik).
    Omnik(Omnik),
    Evse(Evse),
    Cable(Cable),
    Prices(Prices),
    Ev3(Ev3),
    Buzz(Buzz),
    Airco(Airco),
    Vriezer(Vriezer),
    Fluvius(Fluvius),
    /// Nothing for the World: a driver whose values are only entities ([`Source::entities`]),
    /// such as a solar forecast the logic does not read yet.
    Empty,
}

impl Reading {
    /// Write this reading into its slot of the world, replacing what was there.
    pub fn apply_to(self, world: &mut World) {
        match self {
            Reading::P1(r) => world.p1 = r,
            Reading::Solis(r) => world.solis = r,
            Reading::Omnik(r) => world.omnik = r,
            Reading::Evse(r) => world.evse = r,
            Reading::Cable(r) => world.cable = r,
            Reading::Prices(r) => world.prices = r,
            Reading::Ev3(r) => world.ev3 = r,
            Reading::Buzz(r) => world.buzz = r,
            Reading::Airco(r) => world.airco = r,
            Reading::Vriezer(r) => world.vriezer = r,
            Reading::Fluvius(r) => world.fluvius = r,
            Reading::Empty => {}
        }
    }
}

/// Something that can be read: a device or a price feed.
#[async_trait]
pub trait Source: Send {
    /// Short name for logs (`"homewizard"`, `"solis"`, ...).
    fn name(&self) -> &'static str;

    /// Read the device now. An `Err` means nothing trustworthy was read.
    async fn read(&mut self) -> Result<Reading>;

    /// The HA `unavailable` reading for this source: every field `None`.
    fn unavailable(&self) -> Reading;

    /// Values beyond the typed reading, as entities keyed within this device (see
    /// [`crate::entity`]). Called after every successful read; the default is none.
    fn entities(&self) -> Vec<Entity> {
        Vec::new()
    }

    /// Time series of what is expected, by key within this device: `(key, [(unix s, value)])`,
    /// such as a solar forecast's `power_w`. Called after every successful read; the default is
    /// none.
    fn forecasts(&self) -> Vec<(String, Vec<(i64, f64)>)> {
        Vec::new()
    }

    /// The sign-in state of a driver that logs in with stored credentials; `None` (the
    /// default) for one that does not.
    fn auth(&self) -> Option<AuthStatus> {
        None
    }

    /// Replace the stored credentials with `fields` (kind-specific, validated by the driver),
    /// keep them in the state dir and sign in at once. The default takes none.
    async fn set_credentials(&mut self, _fields: &serde_json::Map<String, serde_json::Value>) -> Result<AuthStatus> {
        Err(DriverError::Config(format!("{} takes no credentials", self.name())))
    }
}

/// Read a source, falling back to its `unavailable` reading on error (HA semantics: an entity
/// whose integration fails to update goes `unavailable`). The error is returned alongside so the
/// caller can log it.
pub async fn read_or_unavailable(source: &mut dyn Source) -> (Reading, Option<DriverError>) {
    match source.read().await {
        Ok(r) => (r, None),
        Err(e) => (source.unavailable(), Some(e)),
    }
}

/// What [`Actuator::apply`] did with a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Applied {
    /// The command was for this device and has been sent.
    Done,
    /// The command was for this device but needed no write (HA would not write either, e.g. a
    /// number set to the value it already has, or a bit that is already in place).
    Unchanged,
    /// The command was accepted; part of it runs later (the cable's 3 s start sequence).
    Scheduled,
    /// The command is not for this device.
    NotMine,
}

/// One write on the wire, as a driver sends it (or, in a dry run, would send it).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Write {
    /// Where it goes: `holding 43135`, `dp 124`, `mqtt SmartEVSE/1234/Set/Mode`.
    pub target: String,
    /// The value as it goes on the wire (raw register value, DP value, payload).
    pub value: serde_json::Value,
    /// What the device held there before, as far as the driver knows (its cache).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<serde_json::Value>,
    /// Wire details beside the value (`qos 0, retain false`, `3 s after dp 124`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl Write {
    pub fn new(target: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        Write { target: target.into(), value: value.into(), before: None, note: None }
    }

    pub fn before(mut self, before: Option<impl Into<serde_json::Value>>) -> Self {
        self.before = before.map(Into::into);
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// A command whose wire writes the driver does not describe.
    pub fn command(cmd: &Command) -> Self {
        Write::new("command", serde_json::to_value(cmd).expect("commands serialise"))
    }
}

/// What a command did on the wire (or would do, for [`Actuator::plan`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub applied: Applied,
    /// The writes, in the order they are sent. Empty when nothing needs writing.
    pub writes: Vec<Write>,
}

/// Something that carries out commands.
#[async_trait]
pub trait Actuator: Send {
    /// Carry out `cmd` if it is for this device. Commands for other devices return
    /// `Ok(Applied::NotMine)` without side effects.
    async fn apply(&mut self, cmd: &Command) -> Result<Applied>;

    /// [`Actuator::apply`], reporting the writes it made. The default reports none.
    async fn execute(&mut self, cmd: &Command) -> Result<Outcome> {
        Ok(Outcome { applied: self.apply(cmd).await?, writes: Vec::new() })
    }

    /// What [`Actuator::execute`] would write now, without writing anything (the dry run). A
    /// driver may read the device to work it out. The default names the command instead of its
    /// wire writes.
    async fn plan(&mut self, cmd: &Command) -> Result<Outcome> {
        Ok(Outcome { applied: Applied::Done, writes: vec![Write::command(cmd)] })
    }

    /// Set one of this device's own settable entities (an [`Entity`] from [`Source::entities`]
    /// that carries a `control`), for values the control logic has no command for (the EV3's
    /// AC charging limit). The default sets nothing.
    async fn set(&mut self, _key: &str, _value: &Value) -> Result<Applied> {
        Ok(Applied::NotMine)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Flaky(bool);

    #[async_trait]
    impl Source for Flaky {
        fn name(&self) -> &'static str {
            "flaky"
        }
        async fn read(&mut self) -> Result<Reading> {
            if self.0 { Ok(Reading::Omnik(Omnik { power_w: Some(5.0) })) } else { Err(DriverError::Timeout) }
        }
        fn unavailable(&self) -> Reading {
            Reading::Omnik(Omnik::default())
        }
    }

    /// Each reading lands in its own slot of the world and leaves the others alone.
    #[test]
    fn readings_fill_their_world_slot() {
        let mut w = World::default();
        Reading::P1(P1 { power_w: Some(1.0), ..P1::default() }).apply_to(&mut w);
        Reading::Solis(Solis { soc: Some(2.0), ..Solis::default() }).apply_to(&mut w);
        Reading::Omnik(Omnik { power_w: Some(3.0) }).apply_to(&mut w);
        Reading::Evse(Evse { plug: Some("Connected".into()), ..Evse::default() }).apply_to(&mut w);
        Reading::Cable(Cable { voltage_v: Some(230.0), ..Cable::default() }).apply_to(&mut w);
        Reading::Prices(Prices { afname_now: Some(0.3), ..Prices::default() }).apply_to(&mut w);
        Reading::Ev3(Ev3 { soc: Some(55.0), ..Ev3::default() }).apply_to(&mut w);
        Reading::Buzz(Buzz { soc: Some(40.0), ..Buzz::default() }).apply_to(&mut w);
        Reading::Airco(Airco { zolder_on: Some(true), ..Airco::default() }).apply_to(&mut w);
        Reading::Vriezer(Vriezer { power_w: Some(80.0), ..Vriezer::default() }).apply_to(&mut w);
        Reading::Fluvius(Fluvius { peak_kw: Some(3.9) }).apply_to(&mut w);
        let before = w.clone();
        Reading::Empty.apply_to(&mut w);
        assert_eq!(w, before, "an entities-only reading leaves the world alone");
        assert_eq!(w.p1.power_w, Some(1.0));
        assert_eq!(w.solis.soc, Some(2.0));
        assert_eq!(w.omnik.power_w, Some(3.0));
        assert_eq!(w.evse.plug.as_deref(), Some("Connected"));
        assert_eq!(w.cable.voltage_v, Some(230.0));
        assert_eq!(w.prices.afname_now, Some(0.3));
        assert_eq!((w.ev3.soc, w.buzz.soc, w.airco.zolder_on), (Some(55.0), Some(40.0), Some(true)));
        assert_eq!((w.vriezer.power_w, w.fluvius.peak_kw), (Some(80.0), Some(3.9)));
    }

    /// A failed read becomes the all-`None` reading, with the error handed back for logging.
    #[tokio::test]
    async fn failed_read_is_unavailable() {
        let mut ok = Flaky(true);
        assert_eq!(ok.name(), "flaky");
        let (r, e) = read_or_unavailable(&mut ok).await;
        assert_eq!(r, Reading::Omnik(Omnik { power_w: Some(5.0) }));
        assert!(e.is_none());
        let (r, e) = read_or_unavailable(&mut Flaky(false)).await;
        assert_eq!(r, Reading::Omnik(Omnik::default()));
        assert!(matches!(e, Some(DriverError::Timeout)));
        assert!(ok.entities().is_empty(), "no extra entities by default");
        assert!(ok.forecasts().is_empty(), "no forecasts by default");
        assert_eq!(ok.auth(), None, "no sign-in by default");
        let e = ok.set_credentials(&serde_json::Map::new()).await.unwrap_err();
        assert!(matches!(e, DriverError::Config(m) if m == "flaky takes no credentials"));
    }

    #[async_trait]
    impl Actuator for Flaky {
        async fn apply(&mut self, _: &Command) -> Result<Applied> {
            Ok(Applied::NotMine)
        }
    }

    /// A device sets none of its entities unless it says so.
    #[tokio::test]
    async fn entities_are_not_settable_by_default() {
        let mut d = Flaky(true);
        assert_eq!(d.apply(&Command::RefreshPrices).await.unwrap(), Applied::NotMine);
        assert_eq!(d.set("ac_limit", &Value::Number(80.0)).await.unwrap(), Applied::NotMine);
    }

    /// A driver that does not describe its writes: `execute` is `apply` with no writes, and the
    /// dry run names the command instead of writing anything.
    #[tokio::test]
    async fn default_execute_and_plan() {
        let mut d = Flaky(true);
        let cmd = Command::RefreshPrices;
        assert_eq!(d.execute(&cmd).await.unwrap(), Outcome { applied: Applied::NotMine, writes: vec![] });
        let plan = d.plan(&cmd).await.unwrap();
        assert_eq!(plan.applied, Applied::Done);
        assert_eq!(plan.writes, vec![Write::new("command", "RefreshPrices")]);
        let w = Write::new("dp 1", 2).before(None::<i32>).note("n");
        assert_eq!(serde_json::to_value(&w).unwrap(), serde_json::json!({"target": "dp 1", "value": 2, "note": "n"}));
    }
}
