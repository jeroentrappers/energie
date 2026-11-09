//! Solar position for `sun.sun`.
//!
//! HA's `sun` entity is not a sunrise/sunset test. Its state is
//! `round(astral.sun.elevation(t), 2) > −0.833` — the *refracted* elevation — but it only
//! re-evaluates at scheduled instants: at each solar event (dawn, sunrise, sunset, dusk, …) and,
//! in between, every 2 minutes during twilight and "small day" (`_PHASE_UPDATES`), counted from
//! the last event, with a sample dropped when the next would land within 1.25 × 2 min of the next
//! event. The refracted elevation crosses −0.833° about 2 minutes *before* astral's sunrise
//! (which sits at a geometric −0.789°), so HA flips on the first 2-minute sample after civil dawn
//! that is above the line, and in the evening on the first 2-minute sample after sunset that is
//! below it — 2–4 minutes after sunset. Reproduced here from those rules, and checked to the
//! second against the transitions in a live HA recorder.
//!
//! Geometric positions use the NOAA solar equations (Meeus, *Astronomical Algorithms*), term for
//! term what astral implements; refraction is astral's `refraction_at_zenith`. HA's configured
//! elevation is 0 m, so astral applies no observer-height correction.

use crate::jinja;
use crate::time::{self, Ts, epoch};

/// The default site, central Brussels. A real install sets its own with `[site]` in the config.
pub const LATITUDE: f64 = 50.846_7;
pub const LONGITUDE: f64 = 4.352_5;

static SITE: std::sync::OnceLock<(f64, f64)> = std::sync::OnceLock::new();

/// Set the site once, at start. A second call is ignored: the site never changes while running.
pub fn set_site(latitude: f64, longitude: f64) {
    let _ = SITE.set((latitude, longitude));
}

/// The configured `(latitude, longitude)`, or the default site.
pub fn site() -> (f64, f64) {
    SITE.get().copied().unwrap_or((LATITUDE, LONGITUDE))
}

/// Astral's `SUN_APPARENT_RADIUS`, degrees.
const SUN_APPARENT_RADIUS: f64 = 32.0 / (60.0 * 2.0);

/// Astral's `refraction_at_zenith`, as a function of the geometric elevation, in degrees.
pub fn refraction(elevation: f64) -> f64 {
    if elevation >= 85.0 {
        return 0.0;
    }
    let te = elevation.to_radians().tan();
    let arcsec = if elevation > 5.0 {
        58.1 / te - 0.07 / te.powi(3) + 0.000_086 / te.powi(5)
    } else if elevation > -0.575 {
        let step1 = -12.79 + elevation * 0.711;
        let step2 = 103.4 + elevation * step1;
        let step3 = -518.2 + elevation * step2;
        1735.0 + elevation * step3
    } else {
        -20.774 / te
    };
    arcsec / 3600.0
}

/// Elevation of the sun's centre at astral's sunrise/sunset, degrees (≈ −0.789).
pub fn horizon_deg() -> f64 {
    -(SUN_APPARENT_RADIUS + refraction(-SUN_APPARENT_RADIUS))
}

/// Geometric elevation of astral's dawn/dusk for `depression` degrees (−6 → ≈ −6.055).
fn event_elevation(depression: f64) -> f64 {
    -(depression + refraction(-depression))
}

/// Geometric solar elevation in degrees at `now` for a site at `lat`/`lon` (degrees, east +).
pub fn elevation(now: Ts, lat: f64, lon: f64) -> f64 {
    let jd = epoch(now) / 86_400.0 + 2_440_587.5;
    let t = (jd - 2_451_545.0) / 36_525.0;

    let l0 = (280.466_46 + t * (36_000.769_83 + t * 0.000_303_2)).rem_euclid(360.0);
    let m = 357.529_11 + t * (35_999.050_29 - 0.000_153_7 * t);
    let e = 0.016_708_634 - t * (0.000_042_037 + 0.000_000_126_7 * t);
    let mr = m.to_radians();
    let c = mr.sin() * (1.914_602 - t * (0.004_817 + 0.000_014 * t))
        + (2.0 * mr).sin() * (0.019_993 - 0.000_101 * t)
        + (3.0 * mr).sin() * 0.000_289;
    let true_long = l0 + c;
    let omega = (125.04 - 1_934.136 * t).to_radians();
    let lambda = (true_long - 0.005_69 - 0.004_78 * omega.sin()).to_radians();
    let eps0 = 23.0 + (26.0 + (21.448 - t * (46.815 + t * (0.000_59 - t * 0.001_813))) / 60.0) / 60.0;
    let eps = (eps0 + 0.002_56 * omega.cos()).to_radians();
    let decl = (eps.sin() * lambda.sin()).asin();

    let y = (eps / 2.0).tan().powi(2);
    let l0r = l0.to_radians();
    let eot_min = 4.0
        * (y * (2.0 * l0r).sin() - 2.0 * e * mr.sin() + 4.0 * e * y * mr.sin() * (2.0 * l0r).cos()
            - 0.5 * y * y * (4.0 * l0r).sin()
            - 1.25 * e * e * (2.0 * mr).sin())
        .to_degrees();

    let utc_min = epoch(now).rem_euclid(86_400.0) / 60.0;
    let true_solar_min = (utc_min + eot_min + 4.0 * lon).rem_euclid(1_440.0);
    let hour_angle = (true_solar_min / 4.0 - 180.0).to_radians();

    let latr = lat.to_radians();
    let cos_zenith = latr.sin() * decl.sin() + latr.cos() * decl.cos() * hour_angle.cos();
    90.0 - cos_zenith.clamp(-1.0, 1.0).acos().to_degrees()
}

/// `astral.sun.elevation(observer, t)` with refraction: what HA stores as `elevation`.
pub fn apparent_elevation(now: Ts) -> f64 {
    let (lat, lon) = site();
    let e = elevation(now, lat, lon);
    e + refraction(e)
}

/// HA's `ELEVATION_HORIZON`.
const HA_HORIZON: f64 = -0.833;
/// HA's position update interval during twilight and small day.
const SAMPLE_S: i64 = 120;

/// The instant in `[lo, hi]` where the geometric elevation crosses `target`, given that it is
/// on opposite sides of `target` at the two ends (bisection to 1 ms).
fn crossing(lo: Ts, hi: Ts, target: f64) -> Ts {
    let (lat, lon) = site();
    let rising = elevation(lo, lat, lon) < target;
    let (mut lo, mut hi) = (lo, hi);
    while (hi - lo).num_milliseconds() > 1 {
        let mid = lo + (hi - lo) / 2;
        let above = elevation(mid, lat, lon) > target;
        if above == rising { hi = mid } else { lo = mid }
    }
    hi
}

/// HA's sampled state on `[start, end)`: the last 2-minute sample at or before `now`, where a
/// sample is dropped once the next would come within 1.25 × 2 min of `end`.
fn sampled_above(start: Ts, end: Ts, now: Ts) -> bool {
    let k_now = (now - start).num_milliseconds().div_euclid(SAMPLE_S * 1000);
    // Sample k is scheduled from sample k−1 at u: only if u + 150 s <= end.
    let k_max = ((end - start).num_milliseconds() - 150_000).div_euclid(SAMPLE_S * 1000) + 1;
    let k = k_now.min(k_max.max(0));
    let u = start + chrono::Duration::seconds(k * SAMPLE_S);
    jinja::round(apparent_elevation(u), 2) > HA_HORIZON
}

/// `is_state('sun.sun', 'above_horizon')` at `now`, sampled the way HA's `sun` entity samples.
pub fn above_horizon(now: Ts) -> bool {
    let morning = time::today_at(now, 0, 0, 0);
    let noon = time::today_at(now, 12, 0, 0);
    let night = time::today_at(now, 23, 59, 59);
    let civil = event_elevation(6.0);
    let dawn = crossing(morning, noon, civil);
    let sunrise = crossing(morning, noon, horizon_deg());
    let sunset = crossing(noon, night, horizon_deg());
    let dusk = crossing(noon, night, civil);
    if now < dawn || now >= dusk {
        false
    } else if now < sunrise {
        sampled_above(dawn, sunrise, now)
    } else if now < sunset {
        true
    } else {
        sampled_above(sunset, dusk, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::at;
    use chrono::Datelike;
    use chrono::Duration;

    type Hms = (u32, u32, u32);

    /// Sunrise/sunset in local time from `astral` 3.2 (what HA uses) for the default site at
    /// elevation 0.
    const ASTRAL: [((i32, u32, u32), Hms, Hms); 7] = [
        ((2026, 11, 7), (7, 44, 49), (17, 6, 57)),
        ((2026, 11, 8), (7, 46, 32), (17, 5, 23)),
        ((2026, 6, 21), (5, 29, 9), (21, 59, 39)),
        ((2026, 12, 21), (8, 42, 52), (16, 38, 26)),
        ((2026, 3, 29), (7, 25, 42), (20, 9, 57)),
        ((2026, 10, 25), (7, 22, 31), (17, 29, 57)),
        ((2026, 1, 1), (8, 45, 23), (16, 47, 6)),
    ];

    /// The event solver finds astral's sunrise and sunset (the anchors of HA's sampling) to
    /// within 2 s, including on both DST-change days and at the solstices.
    #[test]
    fn sunrise_and_sunset_are_astrals() {
        let tol = 2_000;
        for ((y, mo, d), (rh, rm, rs), (sh, sm, ss)) in ASTRAL {
            let day = at(y, mo, d, 12, 0, 0);
            let (morning, noon, night) =
                (time::today_at(day, 0, 0, 0), time::today_at(day, 12, 0, 0), time::today_at(day, 23, 59, 59));
            let rise = crossing(morning, noon, horizon_deg());
            let set = crossing(noon, night, horizon_deg());
            assert!((rise - at(y, mo, d, rh, rm, rs)).num_milliseconds().abs() < tol, "{y}-{mo}-{d} sunrise {rise}");
            assert!((set - at(y, mo, d, sh, sm, ss)).num_milliseconds().abs() < tol, "{y}-{mo}-{d} sunset {set}");
        }
    }

    /// Regression values for the default site: up on the first 2-minute sample after civil dawn
    /// whose refracted elevation, rounded to 0.01°, is above −0.833° (≈ 1½ min before astral's
    /// sunrise); down on the first sample after sunset that is not (≈ 4 min after it). The rule
    /// was checked to the second against the `sun.sun` transitions of a live HA recorder. One
    /// second either side of each instant.
    #[test]
    fn flips_on_the_sampled_instants() {
        let ha = [
            ((2026, 11, 3), (7, 36, 25), (17, 17, 35)),
            ((2026, 11, 4), (7, 38, 1), (17, 15, 53)),
            ((2026, 11, 5), (7, 39, 38), (17, 14, 13)),
            ((2026, 11, 6), (7, 41, 14), (17, 12, 35)),
        ];
        let s = Duration::seconds(1);
        for ((y, mo, d), (uh, um, us), (dh, dm, ds)) in ha {
            let up = at(y, mo, d, uh, um, us);
            let down = at(y, mo, d, dh, dm, ds);
            assert!(!above_horizon(up - s - s), "{y}-{mo}-{d} before the morning flip");
            assert!(above_horizon(up + s), "{y}-{mo}-{d} after the morning flip");
            assert!(above_horizon(down - s - s), "{y}-{mo}-{d} before the evening flip");
            assert!(!above_horizon(down + s), "{y}-{mo}-{d} after the evening flip");
        }
        assert!(!above_horizon(at(2026, 11, 7, 7, 42, 46)));
        assert!(above_horizon(at(2026, 11, 7, 7, 42, 49)));
    }

    /// HA's sampling rules (`sun/entity.py`: state = `round(elevation, 2) > −0.833`, samples at
    /// each event and every 2 min in twilight / small day from it, a sample dropped within
    /// 1.25 × 2 min of the next event): regression values for the default site on the 1st and
    /// 15th of every month of 2026 (ms). The rules were checked against astral 3.2 itself.
    #[test]
    fn flips_match_has_rules_on_astral_across_the_year() {
        let ms = |t: Ts, h: u32, m: u32, s: u32, milli: i64| {
            let ref_t = at(t.year(), t.month(), t.day(), h, m, s) + Duration::milliseconds(milli);
            (ref_t, Duration::milliseconds(500))
        };
        let rows = [
            ((2026, 1, 1), (8, 43, 29, 402), (16, 51, 6, 326)),
            ((2026, 1, 15), (8, 36, 33, 68), (17, 9, 20, 832)),
            ((2026, 2, 1), (8, 16, 53, 93), (17, 37, 48, 845)),
            ((2026, 2, 15), (7, 54, 43, 857), (18, 2, 46, 917)),
            ((2026, 3, 1), (7, 25, 44, 292), (18, 27, 17, 978)),
            ((2026, 3, 15), (6, 55, 37, 787), (18, 50, 56, 669)),
            ((2026, 4, 1), (7, 17, 1, 380), (20, 18, 51, 507)),
            ((2026, 4, 15), (6, 47, 10, 556), (20, 41, 38, 619)),
            ((2026, 5, 1), (6, 14, 47, 451), (21, 7, 28, 486)),
            ((2026, 5, 15), (5, 52, 39, 91), (21, 28, 58, 130)),
            ((2026, 6, 1), (5, 33, 47, 767), (21, 50, 41, 798)),
            ((2026, 6, 15), (5, 27, 47, 91), (22, 1, 37, 390)),
            ((2026, 7, 1), (5, 30, 51, 238), (22, 3, 5, 355)),
            ((2026, 7, 15), (5, 44, 5, 865), (21, 54, 1, 496)),
            ((2026, 8, 1), (6, 8, 29, 148), (21, 31, 54, 425)),
            ((2026, 8, 15), (6, 28, 29, 156), (21, 7, 0, 658)),
            ((2026, 9, 1), (6, 55, 21, 39), (20, 31, 46, 21)),
            ((2026, 9, 15), (7, 16, 13, 412), (20, 0, 35, 578)),
            ((2026, 10, 1), (7, 41, 40, 355), (19, 24, 31, 179)),
            ((2026, 10, 15), (8, 3, 53, 58), (18, 54, 6, 740)),
            ((2026, 11, 1), (7, 33, 11, 476), (17, 21, 2, 234)),
            ((2026, 11, 15), (7, 57, 23, 458), (16, 59, 17, 929)),
            ((2026, 12, 1), (8, 22, 8, 591), (16, 43, 41, 564)),
            ((2026, 12, 15), (8, 36, 40, 31), (16, 40, 30, 955)),
        ];
        for ((y, mo, d), (uh, um, us, umi), (dh, dm, ds, dmi)) in rows {
            let day = at(y, mo, d, 12, 0, 0);
            let (up, tol) = ms(day, uh, um, us, umi);
            let (down, _) = ms(day, dh, dm, ds, dmi);
            assert!(!above_horizon(up - tol), "{y}-{mo}-{d} up early");
            assert!(above_horizon(up + tol), "{y}-{mo}-{d} up late");
            assert!(above_horizon(down - tol), "{y}-{mo}-{d} down early");
            assert!(!above_horizon(down + tol), "{y}-{mo}-{d} down late");
        }
    }

    /// The dropped sample: a 2-minute update is not scheduled when it would land within
    /// 1.25 × 2 min of the next event. On ~21 mornings a year the last sample before sunrise is
    /// still just below −0.833° and the next one is dropped, so HA flips at the sunrise event
    /// itself; at the default site for example on 2026-02-17 (sunrise 07:51:36.63) and
    /// 2026-04-04 (sunrise 07:12:24.18).
    #[test]
    fn a_dropped_sample_delays_the_flip_to_sunrise() {
        assert!(!above_horizon(at(2026, 2, 17, 7, 51, 20)), "before sunrise, after the dropped sample");
        assert!(above_horizon(at(2026, 2, 17, 7, 51, 37)));
        assert!(!above_horizon(at(2026, 4, 4, 7, 12, 10)));
        assert!(above_horizon(at(2026, 4, 4, 7, 12, 25)));
    }

    /// Outside twilight the state follows the sun: deep night is down, midday is up; between
    /// sunrise and sunset it is up without sampling, from civil dusk to civil dawn it is down.
    #[test]
    fn day_and_night_need_no_sampling() {
        assert!(!above_horizon(at(2026, 11, 7, 3, 0, 0)));
        assert!(above_horizon(at(2026, 11, 7, 12, 0, 0)));
        assert!(!above_horizon(at(2026, 11, 7, 23, 0, 0)));
    }

    /// Refraction is astral's piecewise formula: 0 from 85°, the polynomial near the horizon,
    /// `−20.774/tan` below −0.575°, the `58.1/tan…` series above 5°.
    #[test]
    fn refraction_is_astrals_piecewise_formula() {
        assert_eq!(refraction(85.0), 0.0);
        assert!((refraction(-SUN_APPARENT_RADIUS) - 0.522_45).abs() < 1e-4);
        assert!((refraction(-6.0) - 20.774 / 6f64.to_radians().tan() / 3600.0).abs() < 1e-12);
        assert!((refraction(10.0) - 0.0884).abs() < 1e-3);
        // astral.sun.elevation at 2026-11-07 11:00 UTC: 22.59679611 (22.55821440 unrefracted).
        // The geometric part matches astral to 1e-7°, so 5e-7 pins every refraction term
        // (the smallest, 0.000086/tan⁵, is 1.9e-6° here).
        assert!((apparent_elevation(at(2026, 11, 7, 12, 0, 0)) - 22.596796106).abs() < 5e-7);
    }

    /// Midday is up, midnight is down, and astral's elevation at 14:07 on 2026-11-07 (19.27°)
    /// agrees.
    #[test]
    fn noon_midnight_and_live_elevation() {
        assert!(above_horizon(at(2026, 11, 7, 13, 46, 12)));
        assert!(!above_horizon(at(2026, 11, 7, 0, 0, 0)));
        assert!(!above_horizon(at(2026, 6, 21, 23, 59, 59)));
        let el = elevation(at(2026, 11, 7, 14, 7, 11), LATITUDE, LONGITUDE);
        assert!((el - 19.27).abs() < 0.3, "elevation {el}");
    }

    /// At the summer solstice the sun peaks near 90 − 50.85 + 23.44 ≈ 62.6°, and at the
    /// winter solstice near 15.6°.
    #[test]
    fn solstice_noon_elevations() {
        let summer = elevation(at(2026, 6, 21, 13, 45, 0), LATITUDE, LONGITUDE);
        let winter = elevation(at(2026, 12, 21, 12, 41, 0), LATITUDE, LONGITUDE);
        assert!((summer - 62.5).abs() < 0.2, "summer {summer}");
        assert!((winter - 15.6).abs() < 0.2, "winter {winter}");
    }

    /// Astral's horizon: 16′ radius + 1880.8″ refraction = 0.2667 + 0.5224 = 0.7891°. The
    /// reference is astral 3.2's `-(SUN_APPARENT_RADIUS + refraction_at_zenith(90 + 16′))`.
    #[test]
    fn horizon_is_astrals_not_the_textbook_value() {
        assert!((horizon_deg() + 0.789_107_135_341_563_5).abs() < 1e-12, "{}", horizon_deg());
    }

    /// `astral.sun.elevation(observer, t, with_refraction=False)` (astral 3.2, the library HA's
    /// `sun` integration uses) for this location at elevation 0, at UTC instants: sunrise and
    /// sunset ±1 min on the 15th of every month of 2026, plus noon/night instants in other
    /// years (1600–2500) so the slow t², t³ terms of the series are exercised too.
    const ASTRAL_ELEVATION: [(i32, u32, u32, u32, u32, u32, f64); 55] = [
        (2026, 1, 15, 7, 38, 3, -0.9210542441843899),
        (2026, 1, 15, 7, 40, 3, -0.6579417336158144),
        (2026, 1, 15, 16, 4, 20, -0.6559177712915556),
        (2026, 1, 15, 16, 6, 20, -0.9189481513244147),
        (2026, 2, 15, 6, 54, 23, -0.9408622176938906),
        (2026, 2, 15, 6, 56, 23, -0.6424451168542049),
        (2026, 2, 15, 16, 57, 46, -0.63788235718512),
        (2026, 2, 15, 16, 59, 46, -0.9359588652313136),
        (2026, 3, 15, 5, 55, 57, -0.9485303912639864),
        (2026, 3, 15, 5, 57, 57, -0.6326190799880607),
        (2026, 3, 15, 17, 45, 56, -0.6298018137297703),
        (2026, 3, 15, 17, 47, 56, -0.9449279116124387),
        (2026, 4, 15, 4, 47, 39, -0.9423746281782996),
        (2026, 4, 15, 4, 49, 39, -0.6393884231585645),
        (2026, 4, 15, 18, 36, 38, -0.6366397703346962),
        (2026, 4, 15, 18, 38, 38, -0.938317851554757),
        (2026, 5, 15, 3, 52, 53, -0.9231323526163635),
        (2026, 5, 15, 3, 54, 53, -0.6549386170083977),
        (2026, 5, 15, 19, 23, 58, -0.6552405045973018),
        (2026, 5, 15, 19, 25, 58, -0.92211311309687),
        (2026, 6, 15, 3, 27, 47, -0.9109599920912643),
        (2026, 6, 15, 3, 29, 47, -0.6693235595297296),
        (2026, 6, 15, 19, 56, 37, -0.6674718256145127),
        (2026, 6, 15, 19, 58, 37, -0.908837414719855),
        (2026, 7, 15, 3, 45, 28, -0.9167223220784848),
        (2026, 7, 15, 3, 47, 28, -0.6639266868526192),
        (2026, 7, 15, 19, 49, 1, -0.6609936996708115),
        (2026, 7, 15, 19, 51, 1, -0.9148185812169345),
        (2026, 8, 15, 4, 29, 7, -0.9342773878711768),
        (2026, 8, 15, 4, 31, 7, -0.6454426016600507),
        (2026, 8, 15, 19, 2, 0, -0.6422960634922532),
        (2026, 8, 15, 19, 4, 0, -0.9325366380422935),
        (2026, 9, 15, 5, 17, 3, -0.9475216573258081),
        (2026, 9, 15, 5, 19, 3, -0.6337528672077042),
        (2026, 9, 15, 17, 55, 35, -0.6301850210700621),
        (2026, 9, 15, 17, 57, 35, -0.9449600339194859),
        (2026, 10, 15, 6, 4, 44, -0.9452633105899366),
        (2026, 10, 15, 6, 6, 44, -0.63751746976871),
        (2026, 10, 15, 16, 49, 6, -0.6331604956967709),
        (2026, 10, 15, 16, 51, 6, -0.9413794628087686),
        (2026, 11, 15, 6, 57, 25, -0.9279376697843134),
        (2026, 11, 15, 6, 59, 25, -0.6522986427452508),
        (2026, 11, 15, 15, 54, 17, -0.6492226585950505),
        (2026, 11, 15, 15, 56, 17, -0.9249862796524866),
        (2026, 12, 15, 7, 37, 48, -0.9160911625950519),
        (2026, 12, 15, 7, 39, 48, -0.6657854206701757),
        (2026, 12, 15, 15, 35, 30, -0.66212843248573),
        (2026, 12, 15, 15, 37, 30, -0.9124293679477944),
        (2026, 6, 21, 11, 45, 0, 62.59112494315716),
        (2026, 12, 21, 23, 0, 0, -61.467486367550805),
        (2030, 3, 20, 12, 0, 0, 39.0789502385247),
        (2100, 9, 1, 7, 30, 0, 23.10605244988274),
        (1950, 2, 1, 10, 0, 0, 17.328048565875818),
        (2500, 6, 1, 9, 0, 0, 47.508529854416835),
        (1600, 12, 1, 13, 0, 0, 14.684956146246037),
    ];

    /// The NOAA series matches astral's term for term: the elevation agrees to 1e-7° (well
    /// under a millisecond of sun motion) at every reference instant.
    #[test]
    fn elevation_matches_astral_term_for_term() {
        use chrono::TimeZone;
        for (y, mo, d, h, mi, s, want) in ASTRAL_ELEVATION {
            let t = chrono::Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap().with_timezone(&crate::time::TZ);
            let got = elevation(t, LATITUDE, LONGITUDE);
            assert!((got - want).abs() < 1e-7, "{t}: {got} vs astral {want}");
        }
    }

    /// The pole gets polar day and night: the zenith clamp keeps acos defined.
    #[test]
    fn polar_day_and_night() {
        assert!(elevation(at(2026, 6, 21, 0, 0, 0), 90.0, 0.0) > 20.0);
        assert!(elevation(at(2026, 12, 21, 12, 0, 0), 90.0, 0.0) < -20.0);
    }
}
