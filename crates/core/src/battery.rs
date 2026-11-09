//! The battery brain: `sensor.batterij_setpoint` and `binary_sensor.dispatcher_off_target`.
//!
//! The live template is one long Jinja expression. It is ported here in two halves:
//!
//! 1. [`Brain::read`] is the template's *preamble*: every `{% set %}` that reads an entity and
//!    the four cascade flags it derives ([`Flags`]). The `soluna_dispatcher` bias update
//!    (`new_bias`) repeats the same preamble (with the plain `net_laden` flag, which its
//!    `track` only reads as `net_laden and soc < 100`, the template's `net_laden`), so it reads
//!    the same struct.
//! 2. [`setpoint`] is the *law*: the base self-consumption term followed by each `{% if %}` block
//!    of the template, in template order, one named step per block.
//!
//! Sign convention (as `sensor.solis_s6_eh1p_battery_power_net_2`): + discharge / − charge.

use crate::cascade;
use crate::derived::Derived;
use crate::jinja::{clamp, max2, min2, py_float, round};
use crate::memory::Memory;
use crate::settings::{BatterijNiveau, Consumer, NetNiveau, Settings};
use crate::time::{self, Ts};
use crate::world::{RcMode, World};

/// `{% set d = 3000 %}`: the forced dump rate into the car in `Ontlaad-budget`.
pub const DUMP_W: f64 = 3000.0;
/// `[5000, [-5000, d]|max]|min`: inverter limit both ways.
pub const MAX_W: f64 = 5000.0;
/// `[d, -220]|min`: the emergency trickle charge.
pub const TRICKLE_W: f64 = 220.0;
/// `soc <= 10` starts the trickle.
pub const TRICKLE_ON_SOC: f64 = 10.0;
/// `soc < 12` keeps a running trickle going.
pub const TRICKLE_OFF_SOC: f64 = 12.0;
/// `batt <= -110.0`: the battery is already taking (about half) the trickle.
pub const TRICKLING_BATT_W: f64 = -110.0;
/// `vloer = 15 if ochtend`.
pub const MORNING_FLOOR_SOC: f64 = 15.0;
/// `(27 - maxphase) * 230`: the per-conductor target of the grid-charge law.
pub const PHASE_TARGET_A: f64 = 27.0;
pub const PHASE_V: f64 = 230.0;
/// `allowed_w - 100` when `sensor.kwartier_toegestaan_vermogen` has no value.
pub const FALLBACK_MARGIN_W: f64 = 100.0;
/// `_yt`: gross surplus a higher car needs before the battery yields; 1400 W (≈ 6 A) when
/// neither the grid nor the battery may bridge, else 200 W.
pub const YIELD_SOLAR_ONLY_W: f64 = 1400.0;
pub const YIELD_BRIDGED_W: f64 = 200.0;
/// `new_bias`: integral gain, clamp and deadband.
pub const BIAS_KI: f64 = 0.1;
pub const BIAS_LIMIT_W: f64 = 200.0;
pub const BIAS_DEADBAND_W: f64 = 20.0;

/// The cascade flags the template derives from the cars and the two level selects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// `ev_higher_hungry`: a car ranked above the battery could take the solar surplus, so the
    /// battery yields it (`_solar_mode and _bruto > _yt and (_ev3_elig or _buzz_elig) and not
    /// _stalled`).
    pub ev_higher_hungry: bool,
    /// `ev_grid_laden`: `Max tot piek` is cheap and a wanting car is drawing.
    pub ev_grid_laden: bool,
    /// `ev_grid_parked`: `Max tot piek` is cheap, a car wants charge but none is drawing.
    pub ev_grid_parked: bool,
    /// `ev_batterij_auto_laden`: `Ontlaad-budget` and a car is drawing.
    pub ev_batterij_auto_laden: bool,
}

impl Flags {
    /// The flag block of the template (shared verbatim by `soluna_dispatcher.new_bias`).
    pub fn read(w: &World, s: &Settings, m: &Memory, d: &Derived) -> Flags {
        let grid_max = cascade::grid_max(w, d, s);
        let batt_dump = s.ev_batterij_niveau == BatterijNiveau::OntlaadBudget;
        let zp = s.zonprioriteit;
        let solar_mode = !grid_max && !batt_dump;
        let yt = if s.ev_net_niveau == NetNiveau::Uit && s.ev_batterij_niveau == BatterijNiveau::Uit {
            YIELD_SOLAR_ONLY_W
        } else {
            YIELD_BRIDGED_W
        };

        let ev3_wants = cascade::ev3_wants(w);
        let ev3_drawing = cascade::ev3_power_above(w, 0.2);
        // Maxed at 15 A or more: no longer hungry.
        let ev3_maxed = ev3_drawing && w.cable.set_current_a.unwrap_or(0.0) >= 15.0;
        let ev3_elig = zp.above(Consumer::Ev3, Consumer::Batterij) && ev3_wants && cascade::ev3_ctrl(w) && !ev3_maxed;

        let buzz_wants = cascade::buzz_wants(w, m);
        let buzz_drawing = cascade::buzz_drawing(w);
        // Within 1.5 A of the charger's max: no longer hungry.
        let buzz_maxed = buzz_drawing && w.evse.override_a.unwrap_or(0.0) >= cascade::buzz_max_a(w) - 1.5;
        let buzz_elig =
            zp.above(Consumer::Buzz, Consumer::Batterij) && buzz_wants && cascade::buzz_ctrl(w) && !buzz_maxed;

        let any_wants = ev3_wants || buzz_wants;
        let any_drawing = ev3_drawing || buzz_drawing;
        Flags {
            ev_higher_hungry: solar_mode
                && d.pv_bruto_overschot_w > yt
                && (ev3_elig || buzz_elig)
                && !m.ev_hoger_laadstil,
            ev_grid_laden: grid_max && any_wants && any_drawing,
            ev_grid_parked: grid_max && any_wants && !any_drawing,
            ev_batterij_auto_laden: batt_dump && any_drawing,
        }
    }
}

/// Everything the setpoint law reads, as the template's preamble resolves it.
#[derive(Debug, Clone, PartialEq)]
pub struct Brain {
    /// `batt`: battery power, W (`|float(0)`).
    pub batt: f64,
    /// `p1`: the 90 s P1 median, falling back to the raw P1, W.
    pub p1: f64,
    /// `p1_fast`: the raw P1, W.
    pub p1_fast: f64,
    /// `soc`, % (`|float(0)`).
    pub soc: f64,
    /// `vloer`: 15 % in the 06–10 h morning window, else `input_number.batterij_reserve_soc`.
    pub vloer: f64,
    /// `bias`: `input_number.batterij_setpoint_bias`.
    pub bias: f64,
    /// `allowed`: the quarter-hour import setpoint, W (the budget sensor, or target − 100).
    pub allowed: f64,
    /// `maxphase`: worst import-direction conductor current, A.
    pub maxphase: f64,
    /// `net_laden`: `input_boolean.batterij_net_laden` and `soc < 100`, so at 100 % with the
    /// flag still on the cascade branches apply.
    pub net_laden: bool,
    /// `battery_bridges`: `ev_batterij_niveau == '6A-brug'`.
    pub battery_bridges: bool,
    /// `reserve_surplus` / `reserve_surplus_w`: the manual reservation toggle and amount.
    pub reserve_surplus: bool,
    pub reserve_surplus_w: f64,
    /// `reserve_grid_w`: `sensor.batterij_reservering_effectief`, W.
    pub reserve_grid_w: f64,
    /// `valid`: P1 power, SoC and battery power all have a value.
    pub valid: bool,
    pub flags: Flags,
}

impl Brain {
    /// The template preamble.
    pub fn read(now: Ts, w: &World, s: &Settings, m: &Memory, d: &Derived) -> Brain {
        let p1_fast = w.p1.power_w.unwrap_or(0.0);
        let tgt_w = d.capaciteit_maandtarget_kw * 1000.0;
        // The budget sensor already carries the margin; only the no-sensor fallback applies it.
        let allowed = match d.kwartier_toegestaan_vermogen_w {
            Some(a) => a,
            None => tgt_w - FALLBACK_MARGIN_W,
        };
        let soc = w.solis.soc.unwrap_or(0.0);
        Brain {
            batt: cascade::batt_w(w),
            p1: d.p1_mediaan_w.unwrap_or(p1_fast),
            p1_fast,
            soc,
            vloer: if time::is_ochtend(now) { MORNING_FLOOR_SOC } else { s.batterij_reserve_soc },
            bias: m.batterij_setpoint_bias,
            allowed,
            maxphase: cascade::maxphase(w),
            net_laden: m.batterij_net_laden && soc < 100.0,
            battery_bridges: s.ev_batterij_niveau == BatterijNiveau::Brug6A,
            reserve_surplus: s.batterij_reservering_actief,
            reserve_surplus_w: s.batterij_reservering_w,
            reserve_grid_w: d.batterij_reservering_effectief_w,
            valid: w.p1.power_w.is_some() && w.solis.soc.is_some() && w.solis.battery_power_w.is_some(),
            flags: Flags::read(w, s, m, d),
        }
    }

    /// `[0, -batt]|max`: what the battery is charging now. It is already inside `p1_fast`, so
    /// the headroom terms add it back.
    fn now_charge(&self) -> f64 {
        max2(0.0, -self.batt)
    }

    /// `allowed - p1_fast + ([0, -batt]|max)`: import headroom left for the battery, W.
    fn headroom(&self) -> f64 {
        self.allowed - self.p1_fast + self.now_charge()
    }

    /// `0 - ([0, [reserve_grid_w, headroom]|min]|max)`: the reservation, bounded by headroom.
    fn grid_reservation(&self) -> f64 {
        -max2(0.0, min2(self.reserve_grid_w, self.headroom()))
    }

    /// `reserve_grid_w > 0 and soc < 100`.
    fn grid_reservation_applies(&self) -> bool {
        self.reserve_grid_w > 0.0 && self.soc < 100.0
    }
}

/// Base law: `d = batt + p1 + bias`, drive the grid to zero.
fn base(b: &Brain) -> f64 {
    b.batt + b.p1 + b.bias
}

/// `ev_batterij_auto_laden and not net_laden`: dump a steady 3000 W into the drawing car,
/// replacing the base law so only the car's loop tracks P1.
fn dump_feed_forward(b: &Brain, d: f64) -> f64 {
    if b.flags.ev_batterij_auto_laden && !b.net_laden { DUMP_W } else { d }
}

/// `ev_higher_hungry and not net_laden`: yield the surplus to the higher-ranked car.
/// `6A-brug` may discharge to feed its bridge; otherwise the reservation (if on) charges at most
/// `reserve_surplus_w` from the leftover export; otherwise hold at 0.
fn yield_to_higher_ev(b: &Brain, d: f64) -> f64 {
    if !(b.flags.ev_higher_hungry && !b.net_laden) {
        return d;
    }
    if b.battery_bridges {
        max2(0.0, d)
    } else if b.reserve_surplus {
        max2(-b.reserve_surplus_w, min2(0.0, d))
    } else {
        0.0
    }
}

/// `ev_grid_laden and not net_laden and not ev_batterij_auto_laden`: a car is importing on
/// purpose, so never discharge into it; take the reservation from the remaining peak headroom.
fn grid_laden_hold(b: &Brain, d: f64) -> f64 {
    if !(b.flags.ev_grid_laden && !b.net_laden && !b.flags.ev_batterij_auto_laden) {
        return d;
    }
    let d = min2(0.0, d);
    if b.grid_reservation_applies() { min2(d, b.grid_reservation()) } else { d }
}

/// `ev_grid_parked and not net_laden and not ev_batterij_auto_laden`: cars parked under the
/// peak; discharge only the overshoot over `allowed`, still absorb export, and take the
/// reservation from the headroom.
fn grid_parked_shave(b: &Brain, d: f64) -> f64 {
    // LIVE: `not ev_batterij_auto_laden` is dead here: parked needs no car drawing, the dump
    // flag needs one.
    if !(b.flags.ev_grid_parked && !b.net_laden && !b.flags.ev_batterij_auto_laden) {
        return d;
    }
    let d = min2(d, max2(0.0, b.p1_fast - b.allowed));
    if b.grid_reservation_applies() { min2(d, b.grid_reservation()) } else { d }
}

/// `net_laden and soc < 100`: grid-charge to full, steering import onto `allowed` with the
/// instantaneous P1, bounded by the worst conductor reaching 27 A and by 5000 W.
fn net_laden(b: &Brain, d: f64) -> f64 {
    if !(b.net_laden && b.soc < 100.0) {
        return d;
    }
    let phase_cap = max2(0.0, min2(MAX_W, b.now_charge() + (PHASE_TARGET_A - b.maxphase) * PHASE_V));
    max2(-phase_cap, b.batt + b.p1_fast - b.allowed)
}

/// `soc <= 10 or (_trickling and soc < 12)`: at least 220 W of charge, with hysteresis carried
/// by the battery already charging ≥ 110 W.
fn trickle(b: &Brain, d: f64) -> f64 {
    let trickling = b.batt <= TRICKLING_BATT_W;
    if b.soc <= TRICKLE_ON_SOC || (trickling && b.soc < TRICKLE_OFF_SOC) { min2(d, -TRICKLE_W) } else { d }
}

/// `soc >= 100`: no charge.
fn ceiling(b: &Brain, d: f64) -> f64 {
    if b.soc >= 100.0 { max2(0.0, d) } else { d }
}

/// `soc <= vloer`: no discharge.
fn floor(b: &Brain, d: f64) -> f64 {
    if b.soc <= b.vloer { min2(0.0, d) } else { d }
}

/// `sensor.batterij_setpoint`, W: every step of the template in order, then the validity gate,
/// the ±5000 W clamp and `round(0)`.
pub fn setpoint(b: &Brain) -> f64 {
    let steps: [fn(&Brain, f64) -> f64; 8] =
        [dump_feed_forward, yield_to_higher_ev, grid_laden_hold, grid_parked_shave, net_laden, trickle, ceiling, floor];
    let d = steps.iter().fold(base(b), |d, step| step(b, d));
    let out = if b.valid { clamp(d, -MAX_W, MAX_W) } else { 0.0 };
    // HA's `round(0)` returns an int, so the state never reads `-0`; `+ 0.0` drops the sign.
    round(out, 0) + 0.0
}

/// `track` in `soluna_dispatcher.new_bias`: the loop is plainly driving P1 to zero, so the
/// integral may accumulate. `ev_grid_parked` freezes it too: the parked-car shave keeps P1
/// non-zero on purpose.
pub fn bias_tracking(b: &Brain) -> bool {
    b.valid
        && !b.flags.ev_higher_hungry
        && !b.flags.ev_grid_laden
        && !b.flags.ev_grid_parked
        && !b.flags.ev_batterij_auto_laden
        && !(b.net_laden && b.soc < 100.0)
        && b.soc < 100.0
        && b.soc > b.vloer
}

/// `soluna_dispatcher.new_bias`: `bias + 0.1 * p1` clamped to ±200 W while tracking and
/// `|p1| >= 20`, else the bias unchanged; `round(1)` either way.
pub fn next_bias(b: &Brain) -> f64 {
    if !bias_tracking(b) || b.p1.abs() < BIAS_DEADBAND_W {
        round(b.bias, 1)
    } else {
        round(clamp(b.bias + BIAS_KI * b.p1, -BIAS_LIMIT_W, BIAS_LIMIT_W), 1)
    }
}

/// `binary_sensor.dispatcher_off_target`: the battery is more than 800 W away from its setpoint
/// while the dispatcher is on and the SoC is in the band where it can follow.
///
/// `has_value('sensor.batterij_setpoint')` is always true here: the template always renders a
/// number (0 when its inputs are invalid).
pub fn off_target(now: Ts, w: &World, s: &Settings, d: &Derived) -> bool {
    let sp = d.batterij_setpoint_w;
    let batt = cascade::batt_w(w);
    let soc = w.solis.soc.unwrap_or(50.0);
    let vloer = if time::is_ochtend(now) { MORNING_FLOOR_SOC } else { s.batterij_reserve_soc };
    let valid = w.solis.battery_power_w.is_some() && w.solis.soc.is_some();
    s.soluna_absorptie_actief && valid && (batt - sp).abs() > 800.0 && soc > vloer + 2.0 && soc < 95.0
}

/// Fill `d.batterij_setpoint_w` and `d.dispatcher_off_target`.
pub fn fill(now: Ts, w: &World, s: &Settings, m: &Memory, d: &mut Derived) {
    d.batterij_setpoint_w = setpoint(&Brain::read(now, w, s, m, d));
    d.dispatcher_off_target = off_target(now, w, s, d);
}

/// How HA prints the state of a sensor whose integration reports whole numbers as `int`
/// (the Solis Modbus registers, `sensor.batterij_setpoint` after `round(0)`, `number.*` with
/// step 1): `3066`, and a fraction as Python prints a float. `None` is `unavailable`.
pub fn fmt_num_state(x: Option<f64>) -> String {
    match x {
        None => "unavailable".to_string(),
        Some(v) if v.fract() == 0.0 && v.abs() < 1e16 => format!("{:.0}", v + 0.0),
        Some(v) => py_float(v),
    }
}

/// How HA prints the state of a sensor whose integration reports a Python `float` (the
/// HomeWizard P1 power, the price sensors): `3066.0`, `0.21152`, `-5e-05`.
pub fn fmt_float_state(x: Option<f64>) -> String {
    x.map_or_else(|| "unavailable".to_string(), py_float)
}

/// How HA prints `select.solis_s6_eh1p_rc_force_charge_discharge_2`.
pub fn fmt_rc_mode(m: Option<RcMode>) -> &'static str {
    match m {
        None => "unavailable",
        Some(RcMode::None) => "None",
        Some(RcMode::ForceCharge) => "Solis RC Force Battery Charge",
        Some(RcMode::ForceDischarge) => "Solis RC Force Battery Discharge",
    }
}

/// A tiny engine for automation tests: one automation, hand-built world/derived/settings/memory,
/// and the previous tick carried like [`crate::engine::Engine`] does.
#[cfg(test)]
pub(crate) mod rig {
    use crate::automations::{Automation, Cx};
    use crate::command::Command;
    use crate::derived::Derived;
    use crate::memory::Memory;
    use crate::settings::Settings;
    use crate::time::Ts;
    use crate::world::World;

    pub struct Rig<A: Automation> {
        pub a: A,
        pub w: World,
        pub d: Derived,
        pub s: Settings,
        pub m: Memory,
        prev: Option<(Ts, World, Derived, Settings)>,
    }

    impl<A: Automation> Rig<A> {
        pub fn new(a: A) -> Self {
            Rig {
                a,
                w: World::default(),
                d: Derived::default(),
                s: Settings::default(),
                m: Memory::default(),
                prev: None,
            }
        }

        /// One tick at `now`; the first tick is HA start.
        pub fn tick(&mut self, now: Ts) -> Vec<Command> {
            let started = self.prev.is_none();
            let (prev_now, pw, pd, ps) =
                self.prev.take().unwrap_or_else(|| (now, self.w.clone(), self.d.clone(), self.s.clone()));
            let mut out = Vec::new();
            let mut cx = Cx {
                now,
                prev_now,
                started,
                w: &self.w,
                pw: &pw,
                d: &self.d,
                pd: &pd,
                s: &mut self.s,
                ps: &ps,
                m: &mut self.m,
                out: &mut out,
            };
            self.a.step(&mut cx);
            self.prev = Some((now, self.w.clone(), self.d.clone(), self.s.clone()));
            out
        }
    }
}

#[cfg(test)]
mod tests;
