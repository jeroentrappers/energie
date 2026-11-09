//! When HA re-renders a template sensor that reads `now()`.
//!
//! A state-based template entity renders when an entity it read during its *last* render
//! changes state, and — because it calls `now()` — at the top of every minute; between those,
//! its state is the last render, however much `now()` moved on. The port recomputes every
//! template every tick; for the quarter-hour templates whose result depends on the clock this
//! matters: `Kwartier toegestaan vermogen` keeps its value while the P1 import register stands
//! still (exporting), so its `rem <= 30` clamp only lands at the next render, and
//! `Kwartier gemiddelde` stops reading P1 once it is past its first 30 s.
//!
//! [`Render::gate`] keeps that schedule: re-render when nothing was rendered yet, when an
//! input read last time has another value now, or when a minute boundary passed.

use serde::{Deserialize, Serialize};

use crate::time::{Ts, epoch};

/// HA's top-of-minute re-render lands a fraction of a second after the minute, after the
/// automations triggered at second 0 have read the old state (recorder 2026-11-03: `Kwartier
/// toegestaan vermogen` at 01:45:00.351 and 01:46:00.352, `Kwartier gemiddelde` at 02:16:00.233,
/// while `soluna_dispatcher` wrote the charge power from the previous render at 01:46:00.081).
/// With whole-second ticks the re-render shows at second 1.
pub const MINUTE_RENDER_DELAY_S: f64 = 0.2;

/// One template's last render: when, what it read, what it showed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Render {
    #[serde(with = "crate::time::serde_opt")]
    pub at: Option<Ts>,
    /// The values of the entities the last render read, in a template-specific order.
    pub inputs: Vec<Option<f64>>,
    pub value: f64,
}

/// What a fresh render produces: the value and the inputs it read.
pub struct Rendered {
    pub value: f64,
    pub inputs: Vec<Option<f64>>,
}

impl Render {
    /// The template's state at `now`. `current` are today's values of the entities the last
    /// render read (same order as [`Render::inputs`]), `render` renders it afresh.
    pub fn gate(&mut self, now: Ts, current: &[Option<f64>], render: impl FnOnce() -> Rendered) -> f64 {
        let minute = |t: f64| (t / 60.0).floor();
        let due = match self.at {
            None => true,
            Some(at) => {
                minute(epoch(now) - MINUTE_RENDER_DELAY_S) > minute(epoch(at)) || self.inputs.as_slice() != current
            }
        };
        if due {
            let r = render();
            *self = Render { at: Some(now), inputs: r.inputs, value: r.value };
        }
        self.value
    }
}

/// The gated templates (`Memory::template_renders`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Renders {
    /// `sensor.kwartiervermogen_projectie`: reads the quarter meter and P1.
    pub kwartiervermogen_projectie: Render,
    /// `sensor.kwartier_toegestaan_vermogen`: reads the quarter meter and the month target.
    pub kwartier_toegestaan_vermogen: Render,
    /// `sensor.kwartier_gemiddelde`: reads the quarter meter, and P1 only in the first 30 s.
    pub kwartier_gemiddelde: Render,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::at;

    /// A render that would produce `v` from `inputs`. One closure type for every call, so the
    /// renders a test expects to be skipped do not count as uncovered code.
    fn r(v: f64, inputs: &[Option<f64>]) -> impl FnOnce() -> Rendered {
        let inputs = inputs.to_vec();
        move || Rendered { value: v, inputs }
    }

    /// First call renders; same inputs within the minute keep the value; a changed input or
    /// the next minute renders again.
    #[test]
    fn renders_on_input_change_and_each_minute() {
        let mut g = Render::default();
        let t = at(2026, 11, 7, 16, 29, 10);
        let s = |x: i64| t + chrono::Duration::seconds(x);
        assert_eq!(g.gate(t, &[Some(1.0)], r(10.0, &[Some(1.0)])), 10.0);
        assert_eq!(g.gate(s(20), &[Some(1.0)], r(20.0, &[Some(1.0)])), 10.0, "16:29:30, nothing changed");
        assert_eq!(g.gate(s(25), &[Some(2.0)], r(30.0, &[Some(2.0)])), 30.0, "input changed");
        assert_eq!(g.gate(s(49), &[Some(2.0)], r(40.0, &[Some(2.0)])), 30.0);
        assert_eq!(g.gate(s(50), &[Some(2.0)], r(45.0, &[Some(2.0)])), 30.0, "16:30:00: not yet");
        assert_eq!(g.gate(s(51), &[Some(2.0)], r(50.0, &[Some(2.0)])), 50.0, "16:30:01");
        assert_eq!(g.at, Some(s(51)));
        assert_eq!(g.gate(s(52), &[Some(2.0)], r(55.0, &[Some(2.0)])), 50.0);
        // A render that read fewer entities compares only those.
        assert_eq!(g.gate(s(53), &[Some(3.0)], r(60.0, &[])), 60.0);
        assert_eq!(g.gate(s(54), &[], r(70.0, &[])), 60.0);
    }

    /// The renders survive a JSON round trip (they are skipped in `Memory`, but the type keeps
    /// its serde shape for diagnostics), and missing fields default.
    #[test]
    fn renders_round_trip_through_json() {
        let mut rs = Renders::default();
        rs.kwartier_toegestaan_vermogen.gate(at(2026, 11, 7, 13, 50, 0), &[Some(0.1)], r(5035.0, &[Some(0.1)]));
        let json = serde_json::to_string(&rs).unwrap();
        assert_eq!(serde_json::from_str::<Renders>(&json).unwrap(), rs);
        assert_eq!(serde_json::from_str::<Renders>("{}").unwrap(), Renders::default());
    }
}
