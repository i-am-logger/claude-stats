use std::time::Duration;

/// Format a duration in seconds as a human-readable string.
///
/// Thin wrapper around [`humantime::format_duration`] that accepts
/// signed seconds (negative values are clamped to zero).
pub(crate) fn format_duration(secs: i64) -> String {
    let dur = Duration::from_secs(secs.max(0) as u64);
    humantime::format_duration(dur).to_string()
}

/// Truncate a string to at most `max` characters, appending `…` if truncated.
pub(crate) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max < 2 {
        s.chars().take(max).collect()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}\u{2026}")
    }
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

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_exact() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn truncate_str_long() {
        assert_eq!(truncate_str("hello world", 8), "hello w\u{2026}");
    }

    #[test]
    fn truncate_str_max_zero() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn truncate_str_max_one() {
        assert_eq!(truncate_str("hello", 1), "h");
    }

    #[test]
    fn truncate_str_max_two() {
        assert_eq!(truncate_str("hello", 2), "h\u{2026}");
    }

    #[test]
    fn truncate_str_multibyte() {
        assert_eq!(
            truncate_str("\u{1f389}\u{1f38a}\u{1f388}\u{1f386}\u{1f387}", 4),
            "\u{1f389}\u{1f38a}\u{1f388}\u{2026}"
        );
    }
}
