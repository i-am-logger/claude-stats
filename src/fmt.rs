use std::time::Duration;

/// Format a duration in seconds as a human-readable string.
///
/// Thin wrapper around [`humantime::format_duration`] that accepts
/// signed seconds (negative values are clamped to zero).
pub fn format_duration(secs: i64) -> String {
    let dur = Duration::from_secs(secs.max(0) as u64);
    humantime::format_duration(dur).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_seconds() {
        assert_eq!(format_duration(0), "0s");
    }

    #[test]
    fn negative_clamped_to_zero() {
        assert_eq!(format_duration(-42), "0s");
    }

    #[test]
    fn seconds_only() {
        assert_eq!(format_duration(45), "45s");
    }

    #[test]
    fn minutes_and_seconds() {
        assert_eq!(format_duration(125), "2m 5s");
    }

    #[test]
    fn hours_minutes_seconds() {
        assert_eq!(format_duration(3661), "1h 1m 1s");
    }
}
