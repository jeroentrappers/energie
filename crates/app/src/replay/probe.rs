//! Decisions compared at HA's own instants.
//!
//! The actuator timelines depend on *when* each side runs: `ev_laden_buzz` runs on a 25 s
//! debounce whose phase comes from `last_triggered`, which the recorder export does not have,
//! so Rust and HA run some seconds apart and see a ramping car at different currents. The
//! probe removes the phase: every HA change of the output is compared with what Rust *would*
//! set (the modulator law) on the state HA decided on: every row up to the change, with the
//! output itself still at its value before the change.

use serde::Serialize;

use super::fmt_ts;

/// Worst disagreements kept.
pub const KEEP_WORST: usize = 10;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProbeSample {
    pub at: String,
    pub ha: f64,
    pub rust: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProbeReport {
    /// HA changes compared (Rust had a value at the step before).
    pub samples: u64,
    /// Within the tolerance (0.05 A: the override's resolution).
    pub matches: u64,
    pub agreement_pct: Option<f64>,
    /// Within 0.5 A.
    pub within_half_pct: Option<f64>,
    pub mean_abs_err: Option<f64>,
    /// Largest errors first.
    pub worst: Vec<ProbeSample>,
}

#[derive(Debug, Default)]
pub struct Probe {
    tol: f64,
    samples: u64,
    matches: u64,
    half: u64,
    err_sum: f64,
    worst: Vec<(f64, ProbeSample)>,
}

impl Probe {
    pub fn new(tol: f64) -> Self {
        Probe { tol, ..Default::default() }
    }

    /// HA changed the output to `ha` at `t`; Rust would have set `rust`.
    pub fn compare(&mut self, t: f64, ha: f64, rust: f64) {
        let e = (rust - ha).abs();
        self.samples += 1;
        self.matches += u64::from(e <= self.tol + 1e-9);
        self.half += u64::from(e <= 0.5 + 1e-9);
        self.err_sum += e;
        if e > self.tol + 1e-9 {
            self.worst.push((e, ProbeSample { at: fmt_ts(t), ha, rust }));
            self.worst.sort_by(|a, b| b.0.total_cmp(&a.0));
            self.worst.truncate(KEEP_WORST);
        }
    }

    pub fn report(&self) -> ProbeReport {
        ProbeReport {
            samples: self.samples,
            matches: self.matches,
            agreement_pct: crate::shadow::pct(self.matches, self.samples),
            within_half_pct: crate::shadow::pct(self.half, self.samples),
            mean_abs_err: (self.samples > 0).then(|| self.err_sum / self.samples as f64),
            worst: self.worst.iter().map(|(_, s)| s.clone()).collect(),
        }
    }
}

/// A mode change HA made, and what Rust's `ev_laden_buzz` would have done on the same state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModeSample {
    pub at: String,
    pub ha: String,
    /// `null`: Rust would leave the mode alone.
    pub rust: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ModeReport {
    pub samples: u64,
    pub matches: u64,
    pub agreement_pct: Option<f64>,
    /// The first disagreements.
    pub examples: Vec<ModeSample>,
}

#[derive(Debug, Default)]
pub struct ModeProbe {
    r: ModeReport,
}

impl ModeProbe {
    pub fn compare(&mut self, t: f64, ha: &str, rust: Option<&str>) {
        self.r.samples += 1;
        if rust == Some(ha) {
            self.r.matches += 1;
        } else if self.r.examples.len() < KEEP_WORST {
            self.r.examples.push(ModeSample { at: fmt_ts(t), ha: ha.into(), rust: rust.map(Into::into) });
        }
        self.r.agreement_pct = crate::shadow::pct(self.r.matches, self.r.samples);
    }

    pub fn report(&self) -> ModeReport {
        self.r.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Errors are counted against the tolerance and 0.5 A; the worst are kept, largest first.
    #[test]
    fn compares_ha_changes_with_rusts_value() {
        let mut p = Probe::new(0.05);
        assert_eq!(p.report().samples, 0);
        p.compare(1.0, 10.04, 10.0);
        p.compare(2.0, 11.7, 12.0);
        p.compare(3.0, 6.0, 12.0);
        let r = p.report();
        assert_eq!((r.samples, r.matches), (3, 1));
        assert!((r.within_half_pct.unwrap() - 200.0 / 3.0).abs() < 1e-9);
        assert!((r.mean_abs_err.unwrap() - (0.04 + 0.3 + 6.0) / 3.0).abs() < 1e-9);
        assert_eq!(r.worst[0].ha, 6.0);
        assert_eq!(r.worst.len(), 2);
        for i in 0..20 {
            p.compare(10.0 + f64::from(i), 20.0 + f64::from(i), 0.0);
        }
        assert_eq!(p.report().worst.len(), KEEP_WORST);
        assert_eq!(Probe::new(0.05).report().agreement_pct, None);
    }

    /// A mode change matches when Rust would select the same option.
    #[test]
    fn mode_changes() {
        let mut p = ModeProbe::default();
        p.compare(0.0, "Normal", Some("Normal"));
        p.compare(1.0, "Pause", None);
        p.compare(2.0, "Pause", Some("Normal"));
        let r = p.report();
        assert_eq!((r.samples, r.matches, r.agreement_pct.map(|p| p.round())), (3, 1, Some(33.0)));
        assert_eq!(r.examples[0].rust, None);
        assert_eq!(r.examples[1].rust.as_deref(), Some("Normal"));
    }
}
