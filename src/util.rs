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
    fn timestamps_format_as_utc_datetime() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00");
        assert_eq!(format_timestamp(1_788_739_200), "2026-09-07 00:00");
        assert_eq!(format_timestamp(951_782_400), "2000-02-29 00:00");
    }
}
