//! Small internal helpers shared across modules.

use std::time::{SystemTime, UNIX_EPOCH};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_to_three_decimals() {
        assert_eq!(round3(1.23456), 1.235);
        assert_eq!(round3(0.0), 0.0);
        assert_eq!(round3(2.9999), 3.0);
    }
}
