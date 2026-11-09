//! Wall-clock helpers. Everything runs in Europe/Brussels local time, like the HA install.

use chrono::{DateTime, Duration, NaiveTime, TimeZone, Timelike};
use chrono_tz::Tz;

pub type Ts = DateTime<Tz>;
pub const TZ: Tz = chrono_tz::Europe::Brussels;

/// Seconds since the Unix epoch, with sub-second precision.
pub fn epoch(t: Ts) -> f64 {
    t.timestamp() as f64 + f64::from(t.timestamp_subsec_millis()) / 1000.0
}

/// Seconds from `a` to `b` (positive when `b` is later).
pub fn secs(a: Ts, b: Ts) -> f64 {
    epoch(b) - epoch(a)
}

/// HA's `today_at('HH:MM:SS')`. On a DST gap the earliest valid instant is used.
pub fn today_at(now: Ts, h: u32, m: u32, s: u32) -> Ts {
    let naive = now.date_naive().and_time(NaiveTime::from_hms_opt(h, m, s).expect("valid time"));
    TZ.from_local_datetime(&naive).earliest().unwrap_or_else(|| TZ.from_utc_datetime(&naive))
}

/// `today_at('06:00') <= now < today_at('10:00')`: the morning window that lowers the floor.
pub fn is_ochtend(now: Ts) -> bool {
    today_at(now, 6, 0, 0) <= now && now < today_at(now, 10, 0, 0)
}

/// Start of the quarter-hour containing `now`.
pub fn quarter_start(now: Ts) -> Ts {
    let base = now.with_nanosecond(0).and_then(|t| t.with_second(0)).expect("valid");
    base - Duration::minutes(i64::from(base.minute() % 15))
}

/// The next quarter-hour boundary: what the `Kwartier import` utility meter reports as
/// `next_reset`.
pub fn next_quarter(now: Ts) -> Ts {
    quarter_start(now) + Duration::minutes(15)
}

/// A `time_pattern` trigger with a period of `period_s` seconds (a divisor of 3600) fired
/// between the previous tick and this one. Brussels offsets are whole hours, so aligning on the
/// epoch is the same as aligning on local wall-clock time.
pub fn crossed_period(prev: Ts, now: Ts, period_s: i64) -> bool {
    prev.timestamp().div_euclid(period_s) != now.timestamp().div_euclid(period_s)
}

/// A `time` trigger at `HH:MM:SS` local fired between the previous tick and this one.
pub fn crossed_daily(prev: Ts, now: Ts, h: u32, m: u32, s: u32) -> bool {
    let at = today_at(now, h, m, s);
    prev < at && at <= now
}

/// Serde for `Ts` as RFC 3339, read back into Brussels time. `DateTime<Tz>` has no serde impl.
pub mod serde_ts {
    use super::{TZ, Ts};
    use chrono::DateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Ts, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&t.to_rfc3339())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Ts, D::Error> {
        let raw = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&raw).map(|t| t.with_timezone(&TZ)).map_err(serde::de::Error::custom)
    }
}

/// Serde for `Option<Ts>`.
pub mod serde_opt {
    use super::{TZ, Ts};
    use chrono::DateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<Ts>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_some(&t.to_rfc3339()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Ts>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|raw| {
                DateTime::parse_from_rfc3339(&raw).map(|t| t.with_timezone(&TZ)).map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

/// Serde for `BTreeMap<String, Ts>`.
pub mod serde_map {
    use std::collections::BTreeMap;

    use super::{TZ, Ts};
    use chrono::DateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(m: &BTreeMap<String, Ts>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(m.iter().map(|(k, v)| (k, v.to_rfc3339())))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, Ts>, D::Error> {
        BTreeMap::<String, String>::deserialize(d)?
            .into_iter()
            .map(|(k, raw)| {
                DateTime::parse_from_rfc3339(&raw).map(|t| (k, t.with_timezone(&TZ))).map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[cfg(test)]
pub(crate) fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> Ts {
    TZ.with_ymd_and_hms(y, mo, d, h, mi, s).single().expect("unambiguous")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_and_secs() {
        let a = at(2026, 11, 7, 12, 0, 0);
        let b = a + Duration::milliseconds(1500);
        assert_eq!(secs(a, b), 1.5);
        assert_eq!(epoch(a), a.timestamp() as f64);
    }

    #[test]
    fn today_at_and_morning_window() {
        let n = at(2026, 11, 7, 7, 30, 0);
        assert_eq!(today_at(n, 6, 0, 0), at(2026, 11, 7, 6, 0, 0));
        assert!(is_ochtend(n));
        assert!(is_ochtend(at(2026, 11, 7, 6, 0, 0)));
        assert!(!is_ochtend(at(2026, 11, 7, 10, 0, 0)));
        assert!(!is_ochtend(at(2026, 11, 7, 5, 59, 59)));
    }

    #[test]
    fn today_at_inside_a_dst_gap_falls_back_to_utc_reading() {
        // 2026-03-29 02:30 does not exist in Brussels.
        let n = at(2026, 3, 29, 12, 0, 0);
        let t = today_at(n, 2, 30, 0);
        assert_eq!(t.timestamp(), at(2026, 3, 29, 4, 30, 0).timestamp());
    }

    #[test]
    fn quarter_boundaries() {
        let n = at(2026, 11, 7, 13, 52, 17) + Duration::milliseconds(300);
        assert_eq!(quarter_start(n), at(2026, 11, 7, 13, 45, 0));
        assert_eq!(next_quarter(n), at(2026, 11, 7, 14, 0, 0));
        assert_eq!(next_quarter(at(2026, 11, 7, 14, 0, 0)), at(2026, 11, 7, 14, 15, 0));
    }

    #[test]
    fn time_pattern_crossings() {
        let a = at(2026, 11, 7, 13, 0, 29);
        assert!(crossed_period(a, a + Duration::seconds(1), 30));
        assert!(!crossed_period(a + Duration::seconds(1), a + Duration::seconds(2), 30));
        // /10 minutes fires on the wall-clock ten.
        assert!(crossed_period(at(2026, 11, 7, 13, 9, 59), at(2026, 11, 7, 13, 10, 0), 600));
    }

    #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
    struct Holder {
        #[serde(with = "serde_ts")]
        t: Ts,
        #[serde(with = "serde_opt")]
        o: Option<Ts>,
        #[serde(with = "serde_map")]
        m: std::collections::BTreeMap<String, Ts>,
    }

    #[test]
    fn serde_roundtrip_keeps_brussels_time() {
        let t = at(2026, 7, 1, 12, 0, 0);
        let h = Holder { t, o: Some(t), m: [("a".to_string(), t)].into() };
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("2026-07-01T12:00:00+02:00"));
        assert_eq!(serde_json::from_str::<Holder>(&json).unwrap(), h);
        let none = Holder { t, o: None, m: Default::default() };
        let json = serde_json::to_string(&none).unwrap();
        assert_eq!(serde_json::from_str::<Holder>(&json).unwrap(), none);
        assert!(serde_json::from_str::<Holder>(r#"{"t":"x","o":null,"m":{}}"#).is_err());
        assert!(serde_json::from_str::<Holder>(r#"{"t":"2026-07-01T12:00:00Z","o":"x","m":{}}"#).is_err());
        assert!(serde_json::from_str::<Holder>(r#"{"t":"2026-07-01T12:00:00Z","o":null,"m":{"a":"x"}}"#).is_err());
    }

    #[test]
    fn daily_crossings() {
        assert!(crossed_daily(at(2026, 11, 7, 22, 59, 59), at(2026, 11, 7, 23, 0, 0), 23, 0, 0));
        assert!(!crossed_daily(at(2026, 11, 7, 23, 0, 0), at(2026, 11, 7, 23, 0, 1), 23, 0, 0));
    }
}
