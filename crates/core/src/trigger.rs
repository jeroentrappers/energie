//! HA trigger semantics as small state machines.
//!
//! HA's `numeric_state`, `template` and `state ... to:` triggers fire on a *transition* into
//! the condition, optionally only after it has held for `for:`. A condition that is already
//! true when HA starts does not fire until it has been false once. [`Held`] reproduces that.

use serde::{Deserialize, Serialize};

use crate::time::{Ts, secs};

/// A transition trigger with an optional `for:` hold.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Held {
    /// When the current true-run started; `None` while false or not primed.
    #[serde(with = "crate::time::serde_opt")]
    since: Option<Ts>,
    /// The current true-run already fired.
    fired: bool,
    /// Seen false at least once since start (a run that was already true at start is ignored).
    primed: bool,
}

impl Held {
    /// Feed the condition for this tick; returns `true` exactly once per run, when the
    /// condition has held for `hold_s` seconds after becoming true.
    pub fn update(&mut self, cond: bool, now: Ts, hold_s: f64) -> bool {
        if !cond {
            self.since = None;
            self.fired = false;
            self.primed = true;
            return false;
        }
        if !self.primed || self.fired {
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if secs(since, now) >= hold_s {
            self.fired = true;
            return true;
        }
        false
    }

    /// A tracker that treats the first observation as a transition (HA `for:` on a condition
    /// that HA itself evaluated at start, e.g. after `homeassistant: start`).
    pub fn primed() -> Self {
        Held { since: None, fired: false, primed: true }
    }

    /// Seconds the condition has been true in the current run (0 when false or unprimed).
    pub fn held_for(&self, now: Ts) -> f64 {
        self.since.map_or(0.0, |s| secs(s, now))
    }
}

/// `trigger: state` without `to:`: fires when the value changes (not on the first sample).
pub fn changed<T: PartialEq>(prev: &T, cur: &T) -> bool {
    prev != cur
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::at;
    use chrono::Duration;

    #[test]
    fn already_true_at_start_does_not_fire() {
        let t = at(2026, 11, 7, 12, 0, 0);
        let mut h = Held::default();
        assert!(!h.update(true, t, 0.0));
        assert!(!h.update(true, t + Duration::seconds(100), 0.0));
        assert!(!h.update(false, t + Duration::seconds(101), 0.0));
        assert!(h.update(true, t + Duration::seconds(102), 0.0));
    }

    #[test]
    fn fires_once_after_hold_and_rearms_after_false() {
        let t = at(2026, 11, 7, 12, 0, 0);
        let mut h = Held::primed();
        assert!(!h.update(true, t, 10.0));
        assert_eq!(h.held_for(t + Duration::seconds(4)), 4.0);
        assert!(!h.update(true, t + Duration::seconds(9), 10.0));
        assert!(h.update(true, t + Duration::seconds(10), 10.0));
        assert!(!h.update(true, t + Duration::seconds(11), 10.0));
        assert!(!h.update(false, t + Duration::seconds(12), 10.0));
        assert_eq!(h.held_for(t + Duration::seconds(12)), 0.0);
        assert!(!h.update(true, t + Duration::seconds(13), 10.0));
        assert!(h.update(true, t + Duration::seconds(23), 10.0));
    }

    #[test]
    fn a_dip_resets_the_hold() {
        let t = at(2026, 11, 7, 12, 0, 0);
        let mut h = Held::primed();
        h.update(true, t, 10.0);
        h.update(false, t + Duration::seconds(5), 10.0);
        assert!(!h.update(true, t + Duration::seconds(6), 10.0));
        assert!(!h.update(true, t + Duration::seconds(15), 10.0));
        assert!(h.update(true, t + Duration::seconds(16), 10.0));
    }

    #[test]
    fn changed_compares() {
        assert!(changed(&1, &2));
        assert!(!changed(&"a", &"a"));
    }
}
