//! Small internal helpers shared across modules.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::Request;
use serde::de::DeserializeOwned;

/// Rounds a floating point value to three decimal places.
pub(crate) fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Current unix time in seconds (never panics, saturates at zero).
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// A well-formed CMS slug: 1-64 characters of `[a-z0-9-]`, no
/// leading/trailing dash and no double dash.
///
/// Shared by the admin form validation ([`crate::routes::cms`]) and the
/// `[cms] default_page` configuration check so both enforce the exact
/// same shape — a config typo and a form typo fail identically.
pub(crate) fn valid_slug(slug: &str) -> bool {
    (1..=64).contains(&slug.len())
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Formats unix seconds as `YYYY-MM-DD HH:MM` (UTC, no dependencies).
///
/// The CMS listings pass the result to the views as `*_h` fields so
/// templates never depend on the JS engine's locale support.
pub(crate) fn format_timestamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = time_of_day / 3_600;
    let minute = (time_of_day % 3_600) / 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

/// Formats unix seconds as `YYYY-MM-DD` (UTC) — the `<lastmod>` shape
/// the sitemap protocol expects (F7). Shares `civil_from_days` with
/// [`format_timestamp`], so the two projections never disagree.
pub(crate) fn iso_date(seconds: i64) -> String {
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Formats unix seconds as an RFC 3339 UTC datetime (F10) — the
/// `<updated>` shape Atom requires. Same civil-date source as
/// [`iso_date`], down to the second.
pub(crate) fn iso_datetime(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

/// Formats unix seconds as an RFC 822/1123 date (F10) — the `<pubDate>`
/// shape RSS 2.0 requires, always UTC (`+0000`). 1970-01-01 was a
/// Thursday, which anchors the weekday arithmetic.
pub(crate) fn rfc2822_date(seconds: i64) -> String {
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let weekday = WEEKDAYS[((days + 4).rem_euclid(7)) as usize];
    format!(
        "{}, {:02} {} {year:04} {:02}:{:02}:{:02} +0000",
        weekday,
        day,
        MONTHS[(month - 1) as usize],
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

/// Parses the `datetime-local` value the admin forms post (F11):
/// `YYYY-MM-DDTHH:MM` (what browsers send) or `YYYY-MM-DDTHH:MM:SS`
/// (accepted for programmatic posts), read as **UTC** — the panel
/// clock, the same one every `*_h` field displays. `None` on any
/// malformed shape or out-of-calendar value (month 13, February 30,
/// hour 24, years outside 1970..=9999): the caller turns it into a
/// Spanish form error.
pub(crate) fn parse_datetime_local(raw: &str) -> Option<i64> {
    let (date, time) = raw.split_once('T')?;

    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() {
        return None;
    }

    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let second: i64 = match time_parts.next() {
        Some(raw) => raw.parse().ok()?,
        None => 0,
    };
    if time_parts.next().is_some() {
        return None;
    }

    if !(1970..=9999).contains(&year) {
        return None;
    }
    if !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if !(0..24).contains(&hour) {
        return None;
    }
    if !(0..60).contains(&minute) {
        return None;
    }
    if !(0..60).contains(&second) {
        return None;
    }

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Formats unix seconds as the `datetime-local` value the admin forms
/// round-trip (F11): `YYYY-MM-DDTHH:MM`, UTC, minute precision — the
/// shape browsers put back into `<input type="datetime-local">`.
pub(crate) fn format_datetime_local(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60
    )
}

/// How many days a civil month has (Gregorian leap rule) — the
/// calendar `parse_datetime_local` enforces.
fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            u32::from(leap) + 28
        }
        _ => 0,
    }
}

/// Normalises a page's schedule on save (F11): a published page
/// carries no schedule (the flag is the whole truth — the forms clear
/// the field), and a draft's schedule only survives while it is still
/// in the future. An elapsed schedule is spent: unchecking
/// «Publicada» on a formerly scheduled page unpublishes for real
/// instead of resurrecting the old date.
pub(crate) fn normalize_schedule(
    is_published: bool,
    publish_at: Option<i64>,
    now: i64,
) -> Option<i64> {
    match (is_published, publish_at) {
        (false, Some(at)) if at > now => Some(at),
        _ => None,
    }
}

/// Converts a count of days since 1970-01-01 into a civil date
/// (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        (month_prime + 3) as u32
    } else {
        (month_prime - 9) as u32
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Converts a civil date into a count of days since 1970-01-01
/// (Howard Hinnant's algorithm) — the exact inverse of
/// [`civil_from_days`], so a date that round-trips through both is
/// the same day by construction (the property the `datetime-local`
/// pair below is tested against).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    // Mar=0 .. Feb=11, the month numbering the algorithm counts by.
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime as i64 + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Reads a bounded `application/x-www-form-urlencoded` body and decodes
/// it as `T`, answering the human-readable failure (the caller renders
/// it on the browser-facing error page: the sender is a form, and a
/// JSON envelope would help nobody).
///
/// Shared by the auth form flows and the CMS admin forms.
pub(crate) async fn read_form<T: DeserializeOwned>(
    request: Request,
    max_body: usize,
) -> Result<T, String> {
    let bytes = axum::body::to_bytes(request.into_body(), max_body)
        .await
        .map_err(|error| format!("could not read the form body: {error}"))?;

    serde_urlencoded::from_bytes(&bytes)
        .map_err(|error| format!("the form fields could not be decoded: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_to_three_decimals() {
        assert_eq!(round3(1.23456), 1.235);
        assert_eq!(round3(0.0), 0.0);
        assert_eq!(round3(2.9999), 3.0);
    }

    #[test]
    fn slugs_validate_the_documented_shape() {
        assert!(valid_slug("a"));
        assert!(valid_slug("inicio"));
        assert!(valid_slug("pagina-2-de-la-guia"));

        assert!(!valid_slug(""), "empty");
        assert!(!valid_slug("-inicio"), "leading dash");
        assert!(!valid_slug("inicio-"), "trailing dash");
        assert!(!valid_slug("pagina--2"), "double dash");
        assert!(!valid_slug("Inicio"), "uppercase");
        assert!(!valid_slug("pagina_2"), "underscore");
        assert!(!valid_slug("página"), "non-ascii");
        assert!(!valid_slug(&"x".repeat(65)), "too long");
    }

    #[test]
    fn timestamps_format_as_utc_datetime() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00");
        assert_eq!(format_timestamp(1_788_739_200), "2026-09-07 00:00");
        assert_eq!(format_timestamp(951_782_400), "2000-02-29 00:00");
    }

    #[test]
    fn iso_dates_match_the_sitemap_lastmod_shape() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_date(1_788_739_200), "2026-09-07");
        assert_eq!(iso_date(1_788_739_199), "2026-09-06");
        assert_eq!(iso_date(951_782_400), "2000-02-29");
    }

    #[test]
    fn iso_datetime_carries_the_rfc3339_shape() {
        // The epoch, verbatim.
        assert_eq!(iso_datetime(0), "1970-01-01T00:00:00Z");
        // One second past midnight on a leap day.
        assert_eq!(iso_datetime(951_782_401), "2000-02-29T00:00:01Z");
        // 2026-09-12 15:15:38 UTC (the F10 probe timestamp).
        assert_eq!(iso_datetime(1_789_226_138), "2026-09-12T15:15:38Z");
    }

    #[test]
    fn rfc2822_dates_carry_the_weekday_and_utc_offset() {
        // 1970-01-01 was a Thursday — the formula's anchor.
        assert_eq!(rfc2822_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        // Leap day, one second in.
        assert_eq!(rfc2822_date(951_782_401), "Tue, 29 Feb 2000 00:00:01 +0000");
        // 2026-09-12 15:15:38 UTC is a Saturday (the probe output).
        assert_eq!(
            rfc2822_date(1_789_226_138),
            "Sat, 12 Sep 2026 15:15:38 +0000"
        );
        // Year-end rollover: 2024-12-31 23:59:59 is a Tuesday.
        assert_eq!(
            rfc2822_date(1_735_689_599),
            "Tue, 31 Dec 2024 23:59:59 +0000"
        );
    }

    #[test]
    fn datetime_local_round_trips_through_the_civil_pair() {
        // Minute-aligned samples: the formatted value carries minute
        // precision, so the round trip is exact on them (epoch, a leap
        // day, the F10 probe minute and a Gregorian century turn).
        for seconds in [
            0,
            951_782_400,     // 2000-02-29 00:00
            1_709_164_800,   // 2024-02-29 00:00
            1_789_226_100,   // 2026-09-12 15:15:00 (the F10 probe minute)
            4_102_444_800,   // 2100-01-01 00:00 (a non-leap century turn)
        ] {
            assert_eq!(seconds % 60, 0, "the samples are minute-aligned");
            let formatted = format_datetime_local(seconds);
            assert_eq!(
                parse_datetime_local(&formatted),
                Some(seconds),
                "{formatted}"
            );
        }
    }

    #[test]
    fn datetime_local_parses_seconds_and_rejects_bad_calendars() {
        // Seconds ride along when posted.
        assert_eq!(parse_datetime_local("1970-01-01T00:00:01"), Some(1));
        assert_eq!(
            parse_datetime_local("2026-09-12T15:15:38"),
            Some(1_789_226_138)
        );
        assert_eq!(
            parse_datetime_local("2026-09-12T15:15"),
            Some(1_789_226_100)
        );

        // Malformed shapes.
        for raw in [
            "",
            "2026-09-12",
            "15:15",
            "2026-09-12X15:15",
            "2026-09-12T15:15:38Z",
            "2026-09-12T15:15:38.5",
            "2026-09-12T15:15:38:00",
            "2026-09-12T15",
            "2026-09-1-2T15:15",
        ] {
            assert_eq!(parse_datetime_local(raw), None, "{raw:?}");
        }

        // Out-of-calendar values: month, day (leap rules included),
        // hour and the epoch-year floor.
        for raw in [
            "2026-13-01T00:00",
            "2026-00-10T00:00",
            "2026-02-30T00:00",
            "2023-02-29T00:00",
            "2100-02-29T00:00",
            "2024-01-32T00:00",
            "2026-09-12T24:00",
            "2026-09-12T23:60",
            "2026-09-12T23:59:60",
            "1969-12-31T23:59",
            "10000-01-01T00:00",
        ] {
            assert_eq!(parse_datetime_local(raw), None, "{raw:?}");
        }

        // The leap years the calendar accepts.
        assert!(parse_datetime_local("2024-02-29T00:00").is_some());
        assert!(parse_datetime_local("2000-02-29T00:00").is_some());
        // 2100 is a Gregorian common year: February has 28 days.
        assert!(parse_datetime_local("2100-02-28T00:00").is_some());
    }

    #[test]
    fn civil_round_trip_covers_the_algorithm_period() {
        // One date per month across a 400-year Gregorian period
        // (146_097 days): the pair inverts exactly everywhere.
        let start = days_from_civil(2000, 3, 1); // Mar 1st anchors the era
        for offset in (0..146_097).step_by(1_211) {
            let days = start + offset;
            let (year, month, day) = civil_from_days(days);
            assert_eq!(days_from_civil(year, month, day), days);
        }
    }

    #[test]
    fn schedules_normalize_to_live_flag_or_pending_future() {
        let now = 1_789_226_138;
        // A pending schedule survives untouched.
        assert_eq!(
            normalize_schedule(false, Some(now + 60), now),
            Some(now + 60)
        );
        // A published page never carries a schedule.
        assert_eq!(normalize_schedule(true, Some(now + 60), now), None);
        assert_eq!(normalize_schedule(true, None, now), None);
        // Elapsed (<= now) and absent schedules are spent/gone.
        assert_eq!(normalize_schedule(false, Some(now), now), None);
        assert_eq!(normalize_schedule(false, Some(now - 1), now), None);
        assert_eq!(normalize_schedule(false, None, now), None);
    }
}
