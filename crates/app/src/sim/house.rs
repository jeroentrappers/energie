//! A closed-loop physics model of the house: PV from the sun's elevation and a drifting cloud
//! cover, a base load with evening/morning bumps and kettle spikes, the Solis battery
//! following its RC commands (with the RC timeout) and integrating SoC, the SmartEVSE and the
//! Kolanky cable following their commands with a ramp, and a P1 meter that sums it all.
//!
//! It is deterministic for a given seed and steps in ≤ 1 s increments, so it can run in real
//! time behind the dashboard or fast-forward under a virtual clock in scenario tests.

use chrono::{Datelike, Duration, TimeZone, Timelike};
use energie_core::command::{AircoUnit, Command};
use energie_core::sun::site;
use energie_core::time::{TZ, Ts, quarter_start, secs};
use energie_core::world::{
    Airco, Buzz, Cable, Ev3, Evse, EvseMode, Fluvius, Omnik, P1, PriceSlot, Prices, RcMode, Solis, Vriezer, World,
};

use crate::config::SimConfig;

/// xorshift64*: small, fast, deterministic.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Sine of the sun's elevation at `t` (negative below the horizon).
pub fn sun_sin_elevation(t: Ts) -> f64 {
    let u = t.with_timezone(&chrono::Utc);
    let doy = f64::from(u.ordinal());
    let decl = (23.44f64).to_radians() * (2.0 * std::f64::consts::PI * (284.0 + doy) / 365.0).sin();
    let hours = f64::from(u.hour()) + f64::from(u.minute()) / 60.0 + f64::from(u.second()) / 3600.0;
    let (latitude, longitude) = site();
    let solar = hours + longitude / 15.0;
    let ha = (15.0 * (solar - 12.0)).to_radians();
    let lat = latitude.to_radians();
    lat.sin() * decl.sin() + lat.cos() * decl.cos() * ha.cos()
}

/// The synthetic day-ahead price (EUR/kWh, offtake incl. tariffs) at local hour `h`.
pub fn price_at(h: f64, day: u32) -> f64 {
    let bump = |c: f64, w: f64| (-((h - c) / w).powi(2)).exp();
    let wobble = 0.02 * (f64::from(day % 7) - 3.0) / 3.0;
    0.30 + 0.12 * bump(19.0, 2.0) + 0.06 * bump(8.0, 1.5) - 0.12 * bump(13.0, 2.5) + wobble
}

const VOLT: f64 = 230.0;
const BATTERY_MAX_W: f64 = 5000.0;
const BATTERY_MIN_SOC: f64 = 10.0;
const BUZZ_KWH: f64 = 77.0;
const EV3_KWH: f64 = 81.4;
const AIRCO_W: f64 = 900.0;

#[derive(Debug, Clone)]
pub struct House {
    cfg: SimConfig,
    rng: Rng,
    pub t: Ts,
    pub cloud: f64,
    spike: Option<(Ts, f64)>,
    pub load_w: f64,
    pub solis_pv_w: f64,
    pub omnik_w: f64,
    // Battery (Solis): + discharge / − charge.
    pub soc: f64,
    pub battery_w: f64,
    pub rc_mode: RcMode,
    pub rc_charge_w: f64,
    pub rc_discharge_w: f64,
    pub rc_timeout_min: f64,
    rc_since: Ts,
    pub allow_grid_charge: bool,
    pub feed_in_limit_on: bool,
    pub backflow_w: f64,
    // SmartEVSE (Buzz).
    pub buzz_plugged: bool,
    pub buzz_soc: f64,
    pub evse_mode: EvseMode,
    pub override_a: f64,
    pub evse_a: f64,
    // Kolanky cable (EV3).
    pub ev3_plugged: bool,
    pub ev3_soc: f64,
    pub cable_started: bool,
    /// The Kolanky quirk: after `Start charging` nothing happens until a current is set.
    pub cable_armed: bool,
    pub cable_set_a: f64,
    cable_pending: Option<(Ts, f64)>,
    pub cable_a: f64,
    pub airco: [bool; 2],
    // Grid.
    pub p1_w: f64,
    pub import_kwh: f64,
    pub export_kwh: f64,
    quarter: Ts,
    quarter_import_kwh: f64,
    pub month_peak_w: f64,
}

impl House {
    pub fn new(cfg: SimConfig, start: Ts) -> Self {
        House {
            rng: Rng::new(cfg.seed),
            t: start,
            cloud: cfg.cloudiness,
            spike: None,
            load_w: 0.0,
            solis_pv_w: 0.0,
            omnik_w: 0.0,
            soc: cfg.battery_soc,
            battery_w: 0.0,
            rc_mode: RcMode::None,
            rc_charge_w: 0.0,
            rc_discharge_w: 0.0,
            rc_timeout_min: 10.0,
            rc_since: start,
            allow_grid_charge: false,
            feed_in_limit_on: false,
            backflow_w: 5500.0,
            buzz_plugged: cfg.buzz_plugged,
            buzz_soc: cfg.buzz_soc,
            evse_mode: EvseMode::Normal,
            override_a: 6.0,
            evse_a: 0.0,
            ev3_plugged: cfg.ev3_plugged,
            ev3_soc: cfg.ev3_soc,
            cable_started: false,
            cable_armed: false,
            cable_set_a: 6.0,
            cable_pending: None,
            cable_a: 0.0,
            airco: [false; 2],
            p1_w: 0.0,
            import_kwh: 10_000.0,
            export_kwh: 8_000.0,
            quarter: quarter_start(start),
            quarter_import_kwh: 0.0,
            month_peak_w: 2500.0,
            cfg,
        }
    }

    /// Advance the physics to `now` in steps of at most 1 s. A jump of more than a day (a
    /// clock change) is skipped instead of integrated.
    pub fn step(&mut self, now: Ts) {
        if secs(self.t, now) > 86_400.0 {
            self.t = now - Duration::seconds(1);
        }
        // Step on the exact clock difference: a wall clock is sub-millisecond, and rounding the
        // step to whole milliseconds would never reach `now` (an endless loop).
        while self.t < now {
            let step = (now - self.t).min(Duration::seconds(1));
            self.advance(step);
        }
    }

    fn advance(&mut self, step: Duration) {
        self.t += step;
        let dt = step.num_nanoseconds().expect("at most 1 s") as f64 / 1e9;
        let t = self.t;
        let local_h = f64::from(t.hour()) + f64::from(t.minute()) / 60.0;

        // Clouds drift around their mean.
        let noise = self.rng.uniform() - 0.5;
        self.cloud =
            (self.cloud + (self.cfg.cloudiness - self.cloud) * 0.002 * dt + noise * 0.03 * dt.sqrt()).clamp(0.0, 1.0);
        let irr = sun_sin_elevation(t).max(0.0).powf(1.2) * (1.0 - 0.75 * self.cloud);
        let mut solis_pv = self.cfg.solis_pv_peak_w * irr;
        self.omnik_w = (self.cfg.omnik_pv_peak_w * irr).round();

        // Base load, bumps, an occasional kettle.
        if self.spike.is_some_and(|(until, _)| t >= until) {
            self.spike = None;
        }
        if self.spike.is_none() && self.rng.uniform() < dt / 1800.0 {
            let len = 120.0 + 180.0 * self.rng.uniform();
            self.spike = Some((t + Duration::seconds(len as i64), 1800.0 + 400.0 * self.rng.uniform()));
        }
        let bump = |c: f64, w: f64| (-((local_h - c) / w).powi(2)).exp();
        let airco = self.airco.iter().filter(|on| **on).count() as f64 * AIRCO_W;
        self.load_w =
            250.0 + 350.0 * bump(19.0, 1.5) + 200.0 * bump(7.5, 1.0) + self.spike.map_or(0.0, |s| s.1) + airco;

        // SmartEVSE: follows the override with a ramp while the Buzz is plugged and not full.
        let evse_on = matches!(self.evse_mode, EvseMode::Normal | EvseMode::Smart | EvseMode::Solar);
        let evse_target = if self.buzz_plugged && self.buzz_soc < 100.0 && evse_on && self.override_a >= 6.0 {
            self.override_a.min(32.0)
        } else {
            0.0
        };
        self.evse_a = ramp(self.evse_a, evse_target, 2.0 * dt, 6.0 * dt);
        self.buzz_soc = (self.buzz_soc + self.evse_a * VOLT * dt / 3.6e6 / BUZZ_KWH * 100.0).min(100.0);

        // Kolanky cable: the script's delayed current set arrives; charge only once armed.
        if let Some((at, a)) = self.cable_pending
            && t >= at
        {
            self.cable_pending = None;
            self.set_cable_current(a);
        }
        let cable_target = if self.cable_started && self.cable_armed && self.ev3_plugged && self.ev3_soc < 100.0 {
            self.cable_set_a
        } else {
            0.0
        };
        self.cable_a = ramp(self.cable_a, cable_target, 1.0 * dt, 4.0 * dt);
        self.ev3_soc = (self.ev3_soc + self.cable_a * VOLT * dt / 3.6e6 / EV3_KWH * 100.0).min(100.0);

        // Battery: RC timeout, then the mode's target with a first-order lag.
        if self.rc_mode != RcMode::None && secs(self.rc_since, t) > self.rc_timeout_min * 60.0 {
            self.rc_mode = RcMode::None;
        }
        let consumers = self.load_w + self.evse_w() + self.cable_w();
        let net = consumers - solis_pv - self.omnik_w;
        let mut target = match self.rc_mode {
            RcMode::None => net,
            RcMode::ForceDischarge => self.rc_discharge_w,
            RcMode::ForceCharge if self.allow_grid_charge => -self.rc_charge_w,
            RcMode::ForceCharge => (-self.rc_charge_w).max(net.min(0.0)),
        }
        .clamp(-BATTERY_MAX_W, BATTERY_MAX_W);
        if self.soc <= BATTERY_MIN_SOC {
            target = target.min(0.0);
        }
        if self.soc >= 100.0 {
            target = target.max(0.0);
        }
        self.battery_w += (target - self.battery_w) * (dt / 3.0).min(1.0);
        self.soc = (self.soc - self.battery_w * dt / 3.6e6 / self.cfg.battery_kwh * 100.0).clamp(0.0, 100.0);

        // Export limit: the hybrid curtails its own strings.
        let export = -(consumers - solis_pv - self.omnik_w - self.battery_w);
        if self.feed_in_limit_on && export > self.backflow_w {
            solis_pv = (solis_pv - (export - self.backflow_w)).max(0.0);
        }
        self.solis_pv_w = solis_pv.round();

        // P1 and its registers; the month peak is the highest quarter-hour average.
        self.p1_w = (consumers - self.solis_pv_w - self.omnik_w - self.battery_w).round();
        let kwh = self.p1_w.abs() * dt / 3.6e6;
        if self.p1_w > 0.0 {
            self.import_kwh += kwh;
            self.quarter_import_kwh += kwh;
        } else {
            self.export_kwh += kwh;
        }
        let q = quarter_start(t);
        if q != self.quarter {
            if q.month() != self.quarter.month() {
                self.month_peak_w = 2500.0;
            }
            self.month_peak_w = self.month_peak_w.max(self.quarter_import_kwh * 4000.0);
            self.quarter = q;
            self.quarter_import_kwh = 0.0;
        }
    }

    fn set_cable_current(&mut self, a: f64) {
        self.cable_set_a = a.clamp(6.0, 16.0);
        if self.cable_started {
            self.cable_armed = true;
        }
    }

    pub fn evse_w(&self) -> f64 {
        self.evse_a * VOLT
    }

    pub fn cable_w(&self) -> f64 {
        self.cable_a * VOLT
    }

    /// Carry out a command at `now`.
    pub fn apply(&mut self, now: Ts, cmd: &Command) {
        use Command::*;
        match cmd {
            EvseCurrentDeciA(d) => self.override_a = f64::from(*d) / 10.0,
            EvseMode(m) => self.evse_mode = *m,
            // script.laadkabel_start_laden: Start, wait 3 s, set the current.
            CableStart { amps } => {
                self.cable_started = true;
                self.cable_armed = false;
                self.cable_pending = Some((now + Duration::seconds(3), f64::from(*amps)));
            }
            CableStop => {
                self.cable_started = false;
                self.cable_armed = false;
                self.cable_pending = None;
            }
            CableSetCurrent(a) => self.set_cable_current(f64::from(*a)),
            SolisRcMode(m) => {
                self.rc_mode = *m;
                self.rc_since = now;
            }
            SolisRcChargeW(w) => {
                self.rc_charge_w = f64::from(*w);
                self.rc_since = now;
            }
            SolisRcDischargeW(w) => {
                self.rc_discharge_w = f64::from(*w);
                self.rc_since = now;
            }
            SolisRcTimeoutMin(m) => self.rc_timeout_min = f64::from(*m),
            SolisAllowGridCharge(on) => self.allow_grid_charge = *on,
            SolisFeedInLimit(on) => self.feed_in_limit_on = *on,
            SolisBackflowW(w) => self.backflow_w = f64::from(*w),
            Airco { unit, on } => self.airco[usize::from(*unit == AircoUnit::Zolder)] = *on,
            CableReconnect | SolisReconnect | RefreshPrices | Notify { .. } | Persistent { .. } => {}
        }
    }

    /// Today's 96 quarter-hour slots, plus tomorrow's from 13:00 (like the day-ahead auction).
    pub fn price_slots(&self) -> Vec<PriceSlot> {
        let day0 = TZ.from_local_datetime(&self.t.date_naive().and_hms_opt(0, 0, 0).expect("midnight")).earliest();
        let day0 = day0.expect("midnight exists in Brussels");
        let days = if self.t.hour() >= 13 { 2 } else { 1 };
        (0..96 * days)
            .map(|i| {
                let start = day0 + Duration::minutes(15 * i64::from(i));
                let h = f64::from(start.hour()) + f64::from(start.minute()) / 60.0;
                PriceSlot { start, price: (price_at(h, start.ordinal()) * 100_000.0).round() / 100_000.0 }
            })
            .collect()
    }

    /// The readings, as the devices would report them.
    pub fn world(&self) -> World {
        let slots = self.price_slots();
        let now_price = slots.iter().rev().find(|s| s.start <= self.t).map(|s| s.price);
        let today = &slots[..96];
        let (lo, hi) = today.iter().fold((f64::MAX, f64::MIN), |(lo, hi), s| (lo.min(s.price), hi.max(s.price)));
        let pct = now_price.map(|p| ((p - lo) / (hi - lo) * 1000.0).round() / 10.0);
        let third = self.load_w / 3.0;
        let phase = |w: f64| Some((w / VOLT * 10.0).round() / 10.0);
        let plug = if self.buzz_plugged { "Connected" } else { "Disconnected" };
        let (state, id) = match (self.buzz_plugged, self.evse_a > 0.5) {
            (true, true) => ("Charging", "C"),
            (true, false) => ("Connected to EV", "B"),
            (false, _) => ("Ready to Charge", "A"),
        };
        let cable_status = if !self.ev3_plugged {
            "available"
        } else if self.cable_a > 0.5 {
            "charging"
        } else if self.ev3_soc >= 100.0 {
            "charged"
        } else {
            "plugged_in"
        };
        World {
            p1: P1 {
                power_w: Some(self.p1_w),
                phase_a: [
                    phase(third + self.evse_w() - self.solis_pv_w - self.battery_w),
                    phase(third + self.cable_w()),
                    phase(third - self.omnik_w),
                ],
                import_kwh: Some((self.import_kwh * 1000.0).round() / 1000.0),
                export_kwh: Some((self.export_kwh * 1000.0).round() / 1000.0),
                month_peak_w: Some(self.month_peak_w.round()),
            },
            solis: Solis {
                soc: Some(self.soc.round()),
                battery_power_w: Some(self.battery_w.round()),
                pv_power_w: Some(self.solis_pv_w),
                rc_mode: Some(self.rc_mode),
                rc_charge_w: Some(self.rc_charge_w),
                rc_discharge_w: Some(self.rc_discharge_w),
                rc_timeout_min: Some(self.rc_timeout_min),
                allow_grid_charge: Some(self.allow_grid_charge),
                feed_in_limit_on: Some(self.feed_in_limit_on),
                backflow_w: Some(self.backflow_w),
            },
            omnik: Omnik { power_w: Some(self.omnik_w) },
            evse: Evse {
                plug: Some(plug.into()),
                state: Some(state.into()),
                state_id: Some(id.into()),
                mode: Some(self.evse_mode),
                override_a: Some(self.override_a),
                override_max_a: Some(29.0),
                charge_current_a: Some((self.evse_a * 10.0).round() / 10.0),
            },
            cable: Cable {
                status: Some(cable_status.into()),
                current_a: Some((self.cable_a * 10.0).round() / 10.0),
                voltage_v: Some(VOLT),
                power_kw: Some((self.cable_w() / 10.0).round() / 100.0),
                set_current_a: Some(self.cable_set_a),
                set_current_max_a: Some(16.0),
            },
            ev3: Ev3 {
                plugged: Some(self.ev3_plugged),
                soc: Some((self.ev3_soc * 10.0).round() / 10.0),
                ac_limit: Some(100.0),
                odometer_km: Some(8634.1),
            },
            buzz: Buzz { soc: Some(self.buzz_soc.floor()), target_soc: Some(100.0), odometer_km: Some(10019.0) },
            prices: Prices {
                afname_now: now_price,
                afname_pct_in_range: pct,
                afname_has_tomorrow: slots.len() > 96,
                injectie_now: now_price.map(|p| ((p * 0.55 - 0.02) * 10_000.0).round() / 10_000.0),
                injectie_has_tomorrow: slots.len() > 96,
                epex_now: now_price.map(|p| ((p * 0.6 - 0.05) * 10_000.0).round() / 10_000.0),
                backup_now: None,
                afname_slots: slots,
            },
            airco: Airco { beneden_on: Some(self.airco[0]), zolder_on: Some(self.airco[1]) },
            vriezer: Vriezer::default(),
            fluvius: Fluvius { peak_kw: Some((self.month_peak_w / 1000.0 * 1000.0).round() / 1000.0) },
        }
    }
}

/// Move `x` toward `target` by at most `up` (rising) or `down` (falling).
fn ramp(x: f64, target: f64, up: f64, down: f64) -> f64 {
    if target > x { (x + up).min(target) } else { (x - down).max(target) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use energie_core::time::TZ;

    fn at(h: u32, m: u32) -> Ts {
        TZ.with_ymd_and_hms(2026, 6, 21, h, m, 0).unwrap()
    }

    fn house(start: Ts) -> House {
        House::new(SimConfig { cloudiness: 0.0, ..Default::default() }, start)
    }

    /// The sun is up at noon in June and down at midnight; the noon elevation in Aalst is
    /// about 62°.
    #[test]
    fn the_sun_follows_the_day() {
        let noon = sun_sin_elevation(at(13, 40)).asin().to_degrees();
        assert!((60.0..64.0).contains(&noon), "{noon}");
        assert!(sun_sin_elevation(at(0, 0)) < 0.0);
    }

    /// On a wall clock the model is stepped to sub-millisecond times; it lands exactly on them
    /// (rounding the step to whole milliseconds used to loop forever on a 0 ms step).
    #[test]
    fn steps_to_sub_millisecond_times() {
        let mut h = house(at(12, 0));
        let t = at(12, 0) + Duration::nanoseconds(1_500_300);
        h.step(t);
        assert_eq!(h.t, t);
        h.step(t + Duration::nanoseconds(300));
        assert_eq!(h.t, t + Duration::nanoseconds(300));
    }

    /// The noise generator is deterministic and stays in [0, 1).
    #[test]
    fn rng_is_deterministic() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..1000 {
            let x = a.uniform();
            assert_eq!(x, b.uniform());
            assert!((0.0..1.0).contains(&x));
        }
        assert!(Rng::new(0).uniform() > 0.0);
    }

    /// P1 is consumers minus production minus battery discharge, and the import register
    /// integrates it.
    #[test]
    fn p1_sums_everything() {
        let mut h = house(at(12, 0));
        h.step(at(12, 10));
        let w = h.world();
        let sum = h.load_w + h.evse_w() + h.cable_w() - h.solis_pv_w - h.omnik_w - h.battery_w;
        assert!((w.p1.power_w.unwrap() - sum).abs() <= 1.0);
        assert!(h.solis_pv_w > 1000.0, "midday sun: {}", h.solis_pv_w);
        let start = h.import_kwh + h.export_kwh;
        h.step(at(12, 20));
        assert!(h.import_kwh + h.export_kwh > start);
    }

    /// In self-consumption the battery takes the surplus at noon and covers the load at night.
    #[test]
    fn battery_self_consumption() {
        let mut h = house(at(9, 0));
        h.buzz_plugged = false;
        h.ev3_plugged = false;
        h.step(at(9, 5));
        assert!(h.battery_w < -500.0, "charging from surplus: {}", h.battery_w);
        assert!(h.p1_w.abs() < 50.0, "P1 near zero: {}", h.p1_w);
        let mut n = house(at(2, 0));
        n.step(at(2, 5));
        assert!(n.battery_w > 100.0, "covering the night load: {}", n.battery_w);
    }

    /// RC force-charge from the grid needs the grid-charge switch; the RC mode drops back to
    /// None after its timeout.
    #[test]
    fn rc_force_charge_and_timeout() {
        let mut h = house(at(2, 0));
        h.apply(at(2, 0), &Command::SolisRcTimeoutMin(2));
        h.apply(at(2, 0), &Command::SolisRcChargeW(3000));
        h.apply(at(2, 0), &Command::SolisRcMode(RcMode::ForceCharge));
        h.step(at(2, 1));
        assert!(h.battery_w > -10.0, "no grid charge without the switch: {}", h.battery_w);
        h.apply(at(2, 1), &Command::SolisAllowGridCharge(true));
        h.step(at(2, 2));
        assert!(h.battery_w < -2900.0, "{}", h.battery_w);
        assert!(h.p1_w > 2900.0);
        h.step(at(2, 4));
        assert_eq!(h.rc_mode, RcMode::None, "timed out");
        h.apply(at(2, 4), &Command::SolisRcDischargeW(1500));
        h.apply(at(2, 4), &Command::SolisRcMode(RcMode::ForceDischarge));
        h.step(at(2, 5));
        assert!((h.battery_w - 1500.0).abs() < 10.0);
    }

    /// The battery stops discharging at the floor and stops charging when full.
    #[test]
    fn battery_soc_limits() {
        let mut h = house(at(2, 0));
        h.soc = 10.0;
        h.step(at(2, 1));
        assert!(h.battery_w <= 0.0);
        let mut f = house(at(12, 0));
        f.soc = 100.0;
        f.step(at(12, 1));
        assert!(f.battery_w >= 0.0);
    }

    /// The SmartEVSE ramps to the override while the Buzz is plugged, and stops on Off.
    #[test]
    fn evse_follows_override_with_a_ramp() {
        let mut h = house(at(2, 0));
        h.apply(at(2, 0), &Command::EvseCurrentDeciA(145));
        h.step(at(2, 0) + Duration::seconds(3));
        assert!(h.evse_a > 0.0 && h.evse_a < 14.5, "ramping: {}", h.evse_a);
        h.step(at(2, 1));
        assert!((h.evse_a - 14.5).abs() < 1e-9);
        assert_eq!(h.world().evse.state.as_deref(), Some("Charging"));
        assert_eq!(h.world().evse.charge_current_a, Some(14.5));
        h.apply(at(2, 1), &Command::EvseMode(EvseMode::Off));
        h.step(at(2, 2));
        assert_eq!(h.evse_a, 0.0);
        assert_eq!(h.world().evse.state_id.as_deref(), Some("B"));
        h.buzz_plugged = false;
        assert_eq!(h.world().evse.plug.as_deref(), Some("Disconnected"));
        assert_eq!(h.world().evse.state_id.as_deref(), Some("A"));
    }

    /// Kolanky quirk: `Start charging` alone does nothing until a current is set; the start
    /// script sets it 3 s later; stop ends the session.
    #[test]
    fn cable_needs_a_current_after_start() {
        let mut h = house(at(2, 0));
        // A bare start (as if the 3 s set never came): nothing.
        h.cable_started = true;
        h.step(at(2, 1));
        assert_eq!(h.cable_a, 0.0);
        assert_eq!(h.world().cable.status.as_deref(), Some("plugged_in"));
        // Setting the current arms it.
        h.apply(at(2, 1), &Command::CableSetCurrent(10));
        h.step(at(2, 2));
        assert_eq!(h.cable_a, 10.0);
        assert_eq!(h.world().cable.status.as_deref(), Some("charging"));
        h.apply(at(2, 2), &Command::CableStop);
        h.step(at(2, 3));
        assert_eq!(h.cable_a, 0.0);
        // The start script: start, then the current after 3 s.
        h.apply(at(2, 3), &Command::CableStart { amps: 8 });
        h.step(at(2, 3) + Duration::seconds(2));
        assert_eq!(h.cable_a, 0.0, "not before the current is set");
        h.step(at(2, 4));
        assert_eq!(h.cable_a, 8.0);
        assert!(h.world().p1.phase_a[1].unwrap() > 8.0);
        h.ev3_plugged = false;
        assert_eq!(h.world().cable.status.as_deref(), Some("available"));
    }

    /// A full EV3 stops drawing and reports `charged`.
    #[test]
    fn full_ev3_is_charged() {
        let mut h = house(at(2, 0));
        h.ev3_soc = 100.0;
        h.apply(at(2, 0), &Command::CableStart { amps: 8 });
        h.step(at(2, 1));
        assert_eq!(h.cable_a, 0.0);
        assert_eq!(h.world().cable.status.as_deref(), Some("charged"));
    }

    /// The export limit curtails the hybrid's own PV.
    #[test]
    fn feed_in_limit_curtails_pv() {
        let mut h = house(at(13, 0));
        h.buzz_plugged = false;
        h.ev3_plugged = false;
        h.soc = 100.0;
        h.apply(at(13, 0), &Command::SolisBackflowW(0));
        h.apply(at(13, 0), &Command::SolisFeedInLimit(true));
        h.step(at(13, 5));
        assert!(h.p1_w > -h.omnik_w - 50.0, "export limited to roughly the Omnik: {}", h.p1_w);
    }

    /// Airco units add load; unobservable commands change nothing.
    #[test]
    fn airco_and_no_op_commands() {
        let mut h = house(at(2, 0));
        h.step(at(2, 1));
        let base = h.load_w;
        h.apply(at(2, 1), &Command::Airco { unit: AircoUnit::Zolder, on: true });
        h.apply(at(2, 1), &Command::Airco { unit: AircoUnit::Beneden, on: true });
        h.step(at(2, 1) + Duration::seconds(1));
        assert!(h.load_w - base > 1700.0);
        assert_eq!(h.world().airco.zolder_on, Some(true));
        let before = format!("{:?}", h.world());
        for c in [Command::CableReconnect, Command::SolisReconnect, Command::RefreshPrices, Command::notify("a", "b")] {
            h.apply(at(2, 1), &c);
        }
        assert_eq!(format!("{:?}", h.world()), before);
    }

    /// Prices: today's 96 slots, tomorrow's from 13:00; the current slot is today's.
    #[test]
    fn day_ahead_prices() {
        let mut h = house(at(12, 0));
        let w = h.world();
        assert_eq!(w.prices.afname_slots.len(), 96);
        assert!(!w.prices.afname_has_tomorrow);
        assert!(w.prices.afname_now.is_some());
        let pct = w.prices.afname_pct_in_range.unwrap();
        assert!((0.0..=100.0).contains(&pct));
        h.step(at(13, 0));
        let w = h.world();
        assert_eq!(w.prices.afname_slots.len(), 192);
        assert!(w.prices.injectie_has_tomorrow);
        assert!(price_at(19.0, 1) > price_at(13.0, 1), "evening above midday");
    }

    /// The month peak follows the highest quarter average and resets on a new month; a
    /// clock jump of days is skipped, not integrated.
    #[test]
    fn month_peak_and_clock_jumps() {
        let start = TZ.with_ymd_and_hms(2026, 6, 30, 23, 30, 0).unwrap();
        let mut h = house(start);
        h.apply(start, &Command::SolisAllowGridCharge(true));
        h.apply(start, &Command::SolisRcTimeoutMin(60));
        h.apply(start, &Command::SolisRcChargeW(5000));
        h.apply(start, &Command::SolisRcMode(RcMode::ForceCharge));
        h.soc = 20.0;
        h.step(start + Duration::minutes(16));
        assert!(h.month_peak_w > 4000.0, "{}", h.month_peak_w);
        h.step(start + Duration::minutes(31));
        assert!(h.month_peak_w > 4000.0, "July starts over but this quarter is heavy too");
        let far = start + Duration::days(3);
        h.step(far);
        assert_eq!(h.t, far);
        assert_eq!(ramp(5.0, 0.0, 1.0, 2.0), 3.0);
    }
}
