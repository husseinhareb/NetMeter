//! Calendar arithmetic in the user's local timezone.
//!
//! Two rules run through this file:
//!
//! * **A day is not 86_400_000 ms.** DST makes local days 23 or 25 hours long,
//!   so every boundary is computed from the IANA database, never by adding a
//!   constant.
//! * **Local midnight does not always exist.** Chile, Cuba and Iran move their
//!   DST transition to exactly midnight, so `00:00` on that date is skipped
//!   entirely and `chrono` correctly returns `MappedLocalTime::None`. An
//!   `.unwrap()` there is a crash on a date the user can pick from a calendar.
//!
//! Ranges are half-open, `[start, end)`. That makes 23- and 25-hour days fall
//! out of the arithmetic for free and guarantees adjacent periods neither
//! overlap nor leave a gap.

use super::types::{Granularity, UsagePeriod};
use chrono::{DateTime, Datelike, MappedLocalTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;

/// Resolve the system's IANA timezone.
///
/// Falls back to UTC rather than failing: inside a Flatpak or Snap sandbox
/// `/etc/localtime` may not be visible. Days are then reported in UTC, and the
/// fallback is logged.
pub fn system_timezone() -> Tz {
    match iana_time_zone::get_timezone() {
        Ok(name) => match name.parse::<Tz>() {
            Ok(tz) => tz,
            Err(_) => {
                tracing::warn!(timezone = %name, "unrecognized system timezone, using UTC");
                Tz::UTC
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "cannot determine system timezone, using UTC");
            Tz::UTC
        }
    }
}

/// The current wall-clock instant in epoch milliseconds.
pub fn now_utc_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// The first instant that actually exists on a given local calendar date.
///
/// Normally that is local midnight. On a spring-forward-at-midnight date the
/// clock jumps straight from 23:59:59 to 01:00:00, so we walk forward in
/// 15-minute steps until we find an instant the zone admits -- 15 minutes
/// because a few historical transitions were not whole hours.
pub fn day_start(tz: Tz, date: NaiveDate) -> DateTime<Utc> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .expect("00:00:00 is a valid time of day");
    resolve_local(tz, midnight).unwrap_or_else(|| {
        // No instant in the first five hours of this date exists in this zone.
        // No real transition comes close to that, but returning *something*
        // monotonic beats panicking inside a query.
        tracing::error!(%date, timezone = %tz.name(), "no representable local start of day");
        Utc.timestamp_millis_opt(0).single().unwrap_or_default()
    })
}

/// Map a naive local time onto a real instant, skipping forward over a DST gap.
fn resolve_local(tz: Tz, naive: NaiveDateTime) -> Option<DateTime<Utc>> {
    match tz.from_local_datetime(&naive) {
        MappedLocalTime::Single(t) => Some(t.with_timezone(&Utc)),
        // Fall-back overlap: this local time happens twice. Take the earlier
        // one, so that consecutive days still meet exactly and the repeated
        // hour is counted once, in the day it starts.
        MappedLocalTime::Ambiguous(earliest, _) => Some(earliest.with_timezone(&Utc)),
        // Spring-forward gap: this local time was skipped. Step forward.
        MappedLocalTime::None => {
            let mut probe = naive;
            for _ in 0..20 {
                probe += chrono::TimeDelta::minutes(15);
                if let Some(t) = tz.from_local_datetime(&probe).earliest() {
                    return Some(t.with_timezone(&Utc));
                }
            }
            None
        }
    }
}

/// The half-open instant range covering one local calendar date.
pub fn day_period(tz: Tz, date: NaiveDate) -> UsagePeriod {
    let start = day_start(tz, date);
    let end = match date.succ_opt() {
        Some(next) => day_start(tz, next),
        None => start, // year 262143; the range degenerates rather than panics
    };
    UsagePeriod::new(start.timestamp_millis(), end.timestamp_millis())
}

/// The local calendar date an instant falls on.
pub fn local_date(tz: Tz, utc_ms: i64) -> NaiveDate {
    to_local(tz, utc_ms).date_naive()
}

/// The `YYYY-MM-DD` key an instant belongs to. This is the string stored in
/// `usage_day.local_date` and compared with `BETWEEN` in every rollup query:
/// zero-padded ISO dates sort lexicographically exactly as they sort
/// chronologically, so no date parsing happens in SQL.
pub fn local_date_key(tz: Tz, utc_ms: i64) -> String {
    local_date(tz, utc_ms).format("%Y-%m-%d").to_string()
}

fn to_local(tz: Tz, utc_ms: i64) -> DateTime<Tz> {
    let utc = Utc
        .timestamp_millis_opt(utc_ms)
        .single()
        // Out-of-range only for timestamps ~262 000 years away; clamp rather
        // than panic, so a garbage clock cannot take down a query.
        .unwrap_or_else(|| Utc.timestamp_millis_opt(0).single().unwrap_or_default());
    utc.with_timezone(&tz)
}

/// The start of the UTC clock hour containing an instant.
///
/// Hour buckets are keyed in UTC on purpose. A DST shift moves *local* hour
/// labels around, but a UTC hour is always exactly 3_600_000 ms, so hourly
/// buckets never overlap or vanish. The local date is stored alongside so that
/// day rollups need no timezone conversion at query time.
pub fn hour_start_ms(utc_ms: i64) -> i64 {
    utc_ms.div_euclid(3_600_000) * 3_600_000
}

/// Parse an inclusive calendar key of the form `YYYY`, `YYYY-MM`, `YYYY-MM-DD`
/// or `YYYY-MM-DDTHH` and return the half-open instant range it covers.
///
/// This is the single place the API's string keys become instants.
pub fn period_for_key(tz: Tz, key: &str) -> Option<UsagePeriod> {
    let key = key.trim();
    match key.len() {
        4 => {
            let year: i32 = key.parse().ok()?;
            let start = day_start(tz, NaiveDate::from_ymd_opt(year, 1, 1)?);
            let end = day_start(tz, NaiveDate::from_ymd_opt(year.checked_add(1)?, 1, 1)?);
            Some(UsagePeriod::new(
                start.timestamp_millis(),
                end.timestamp_millis(),
            ))
        }
        7 => {
            let (y, m) = key.split_once('-')?;
            let (year, month): (i32, u32) = (y.parse().ok()?, m.parse().ok()?);
            let start = day_start(tz, NaiveDate::from_ymd_opt(year, month, 1)?);
            let (ny, nm) = if month == 12 {
                (year.checked_add(1)?, 1)
            } else {
                (year, month + 1)
            };
            let end = day_start(tz, NaiveDate::from_ymd_opt(ny, nm, 1)?);
            Some(UsagePeriod::new(
                start.timestamp_millis(),
                end.timestamp_millis(),
            ))
        }
        10 => {
            let date = NaiveDate::parse_from_str(key, "%Y-%m-%d").ok()?;
            Some(day_period(tz, date))
        }
        13 => {
            // "YYYY-MM-DDTHH" identifies a UTC hour bucket.
            let dt = NaiveDateTime::parse_from_str(&format!("{key}:00:00"), "%Y-%m-%dT%H:%M:%S")
                .ok()?;
            let start = Utc.from_utc_datetime(&dt).timestamp_millis();
            Some(UsagePeriod::new(start, start + 3_600_000))
        }
        _ => None,
    }
}

/// The bucket key an instant belongs to, at a given granularity.
pub fn bucket_key(tz: Tz, granularity: Granularity, utc_ms: i64) -> String {
    match granularity {
        Granularity::Hour => {
            let h = hour_start_ms(utc_ms);
            to_utc_datetime(h).format("%Y-%m-%dT%H").to_string()
        }
        _ => {
            let d = local_date_key(tz, utc_ms);
            d[..granularity.key_len().min(d.len())].to_string()
        }
    }
}

fn to_utc_datetime(utc_ms: i64) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(utc_ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_millis_opt(0).single().unwrap_or_default())
}

/// Every bucket key from `from` to `to` inclusive, in chronological order.
///
/// The API returns a *dense* series -- a day the machine was switched off still
/// gets a zero bucket -- so the frontend never has to do DST-correct date
/// arithmetic in JavaScript to work out which bar is missing.
pub fn enumerate_keys(
    tz: Tz,
    granularity: Granularity,
    from: &UsagePeriod,
    to: &UsagePeriod,
) -> Vec<(String, UsagePeriod)> {
    let mut out = Vec::new();
    let mut cursor = from.start_utc_ms;
    let limit = to.end_utc_ms;
    // Bounded so a pathological range (a clock stepped to year 3000) cannot
    // allocate without end. 20 000 buckets is ~55 years of days or ~2 years of
    // hours, well past any view the product offers.
    const MAX_BUCKETS: usize = 20_000;

    while cursor < limit && out.len() < MAX_BUCKETS {
        let period = match granularity {
            Granularity::Hour => {
                let s = hour_start_ms(cursor);
                UsagePeriod::new(s, s + 3_600_000)
            }
            Granularity::Day => day_period(tz, local_date(tz, cursor)),
            Granularity::Month => {
                let d = local_date(tz, cursor);
                let start = day_start(tz, NaiveDate::from_ymd_opt(d.year(), d.month(), 1)
                    .unwrap_or(d));
                let (ny, nm) = if d.month() == 12 {
                    (d.year() + 1, 1)
                } else {
                    (d.year(), d.month() + 1)
                };
                let end = NaiveDate::from_ymd_opt(ny, nm, 1)
                    .map(|n| day_start(tz, n))
                    .unwrap_or(start);
                UsagePeriod::new(start.timestamp_millis(), end.timestamp_millis())
            }
            Granularity::Year => {
                let d = local_date(tz, cursor);
                let start = NaiveDate::from_ymd_opt(d.year(), 1, 1)
                    .map(|n| day_start(tz, n))
                    .unwrap_or_else(|| day_start(tz, d));
                let end = NaiveDate::from_ymd_opt(d.year() + 1, 1, 1)
                    .map(|n| day_start(tz, n))
                    .unwrap_or(start);
                UsagePeriod::new(start.timestamp_millis(), end.timestamp_millis())
            }
        };
        out.push((bucket_key(tz, granularity, cursor), period));
        // Advance past the end of the bucket we just emitted. `max` guards
        // against a degenerate zero-length period looping forever.
        cursor = period.end_utc_ms.max(cursor + 1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz(name: &str) -> Tz {
        name.parse().expect("test timezone exists")
    }

    fn hours(p: UsagePeriod) -> i64 {
        p.duration_ms() / 3_600_000
    }

    #[test]
    fn an_ordinary_day_is_24_hours() {
        let t = tz("Europe/Paris");
        let d = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        assert_eq!(hours(day_period(t, d)), 24);
    }

    #[test]
    fn spring_forward_day_is_23_hours() {
        let t = tz("Europe/Paris");
        let d = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        assert_eq!(hours(day_period(t, d)), 23);
    }

    #[test]
    fn fall_back_day_is_25_hours() {
        let t = tz("Europe/Paris");
        let d = NaiveDate::from_ymd_opt(2026, 10, 25).unwrap();
        assert_eq!(hours(day_period(t, d)), 25);
    }

    #[test]
    fn midnight_dst_transition_does_not_panic_and_gives_a_23_hour_day() {
        // Santiago moves to DST at 00:00, so 2024-09-08T00:00 local does not
        // exist. This is the case that turns a naive .unwrap() into a crash.
        let t = tz("America/Santiago");
        let d = NaiveDate::from_ymd_opt(2024, 9, 8).unwrap();
        let naive = d.and_hms_opt(0, 0, 0).unwrap();
        assert!(
            matches!(t.from_local_datetime(&naive), MappedLocalTime::None),
            "precondition: local midnight is skipped on this date"
        );
        assert_eq!(hours(day_period(t, d)), 23);
    }

    #[test]
    fn days_tile_exactly_with_no_gap_or_overlap_across_a_dst_weekend() {
        let t = tz("Europe/Paris");
        for (m, d) in [(3, 28), (3, 29), (3, 30), (10, 24), (10, 25), (10, 26)] {
            let date = NaiveDate::from_ymd_opt(2026, m, d).unwrap();
            let next = date.succ_opt().unwrap();
            assert_eq!(
                day_period(t, date).end_utc_ms,
                day_period(t, next).start_utc_ms,
                "{date} must end exactly where {next} begins"
            );
        }
    }

    #[test]
    fn an_instant_belongs_to_exactly_one_day() {
        let t = tz("Europe/Paris");
        // Walk every 20 minutes across the fall-back weekend and assert each
        // instant lands inside the day its key names. The repeated 02:00-03:00
        // hour is the interesting part.
        let start = day_period(t, NaiveDate::from_ymd_opt(2026, 10, 24).unwrap()).start_utc_ms;
        let end = day_period(t, NaiveDate::from_ymd_opt(2026, 10, 26).unwrap()).end_utc_ms;
        let mut ms = start;
        while ms < end {
            let key = local_date_key(t, ms);
            let date = NaiveDate::parse_from_str(&key, "%Y-%m-%d").unwrap();
            assert!(
                day_period(t, date).contains(ms),
                "{ms} keyed as {key} but falls outside that day"
            );
            ms += 20 * 60 * 1000;
        }
    }

    #[test]
    fn utc_is_supported_as_a_fallback_zone() {
        let t = Tz::UTC;
        let d = NaiveDate::from_ymd_opt(2026, 3, 29).unwrap();
        assert_eq!(hours(day_period(t, d)), 24, "UTC never shifts");
    }

    #[test]
    fn half_hour_offset_zones_work() {
        let t = tz("Asia/Kolkata");
        let d = NaiveDate::from_ymd_opt(2026, 5, 5).unwrap();
        let p = day_period(t, d);
        assert_eq!(hours(p), 24);
        // UTC+05:30 means the local day starts at 18:30 UTC the day before.
        assert_eq!(local_date_key(t, p.start_utc_ms), "2026-05-05");
        assert_eq!(local_date_key(t, p.end_utc_ms - 1), "2026-05-05");
        assert_eq!(local_date_key(t, p.start_utc_ms - 1), "2026-05-04");
    }

    #[test]
    fn southern_hemisphere_dst_runs_the_other_way() {
        let t = tz("Australia/Sydney");
        assert_eq!(
            hours(day_period(t, NaiveDate::from_ymd_opt(2026, 10, 4).unwrap())),
            23
        );
        assert_eq!(
            hours(day_period(t, NaiveDate::from_ymd_opt(2026, 4, 5).unwrap())),
            25
        );
    }

    #[test]
    fn hour_buckets_are_always_exactly_one_hour() {
        assert_eq!(hour_start_ms(0), 0);
        assert_eq!(hour_start_ms(3_599_999), 0);
        assert_eq!(hour_start_ms(3_600_000), 3_600_000);
        // Negative instants (pre-1970 clock during boot) floor correctly rather
        // than truncating toward zero, which would put -1 ms in the 0 bucket.
        assert_eq!(hour_start_ms(-1), -3_600_000);
    }

    #[test]
    fn month_and_year_keys_round_trip() {
        let t = tz("Europe/Paris");
        let sep = period_for_key(t, "2026-09").expect("valid month key");
        assert_eq!(local_date_key(t, sep.start_utc_ms), "2026-09-01");
        assert_eq!(local_date_key(t, sep.end_utc_ms - 1), "2026-09-30");

        let yr = period_for_key(t, "2026").expect("valid year key");
        assert_eq!(local_date_key(t, yr.start_utc_ms), "2026-01-01");
        assert_eq!(local_date_key(t, yr.end_utc_ms - 1), "2026-12-31");

        let dec = period_for_key(t, "2026-12").expect("december rolls the year");
        assert_eq!(local_date_key(t, dec.end_utc_ms - 1), "2026-12-31");
    }

    #[test]
    fn leap_day_is_a_real_day() {
        let t = tz("Europe/Paris");
        let feb = period_for_key(t, "2028-02").expect("leap february");
        assert_eq!(local_date_key(t, feb.end_utc_ms - 1), "2028-02-29");
    }

    #[test]
    fn malformed_keys_are_rejected_rather_than_guessed() {
        let t = Tz::UTC;
        for bad in ["", "20", "2026-13", "2026-02-30", "not-a-date", "2026-1-1"] {
            assert!(period_for_key(t, bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn dense_day_series_includes_every_day_in_range() {
        let t = tz("Europe/Paris");
        let from = period_for_key(t, "2026-03-27").unwrap();
        let to = period_for_key(t, "2026-04-02").unwrap();
        let keys = enumerate_keys(t, Granularity::Day, &from, &to);
        assert_eq!(keys.len(), 7, "7 days inclusive, DST weekend included");
        assert_eq!(keys[0].0, "2026-03-27");
        assert_eq!(keys[6].0, "2026-04-02");
        // Buckets tile the range exactly.
        for w in keys.windows(2) {
            assert_eq!(w[0].1.end_utc_ms, w[1].1.start_utc_ms);
        }
    }

    #[test]
    fn dense_month_series_spans_a_year_boundary() {
        let t = tz("Europe/Paris");
        let from = period_for_key(t, "2025-11").unwrap();
        let to = period_for_key(t, "2026-02").unwrap();
        let keys: Vec<_> = enumerate_keys(t, Granularity::Month, &from, &to)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(keys, ["2025-11", "2025-12", "2026-01", "2026-02"]);
    }

    #[test]
    fn dense_hour_series_covers_a_25_hour_local_day() {
        let t = tz("Europe/Paris");
        let day = period_for_key(t, "2026-10-25").unwrap();
        let keys = enumerate_keys(t, Granularity::Hour, &day, &day);
        assert_eq!(keys.len(), 25, "the fall-back day really has 25 hours");
    }

    #[test]
    fn enumerating_a_reversed_range_yields_nothing_rather_than_looping() {
        let t = Tz::UTC;
        let from = period_for_key(t, "2026-09-10").unwrap();
        let to = period_for_key(t, "2026-09-01").unwrap();
        assert!(enumerate_keys(t, Granularity::Day, &from, &to).is_empty());
    }

    #[test]
    fn enumeration_is_bounded_for_an_absurd_range() {
        let t = Tz::UTC;
        let from = period_for_key(t, "1970-01-01").unwrap();
        let to = period_for_key(t, "9999").unwrap();
        assert!(enumerate_keys(t, Granularity::Hour, &from, &to).len() <= 20_000);
    }

    #[test]
    fn year_keys_for_an_instant_match_the_local_year_not_the_utc_one() {
        // 2026-01-01T00:30 in Sydney is still 2025-12-31 in UTC.
        let t = tz("Australia/Sydney");
        let ms = day_period(t, NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()).start_utc_ms + 1;
        assert_eq!(bucket_key(t, Granularity::Year, ms), "2026");
        assert_eq!(bucket_key(t, Granularity::Day, ms), "2026-01-01");
    }
}
