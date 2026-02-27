#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use std::time::Duration;

/// Format a duration in seconds as a human-readable string.
///
/// Thin wrapper around [`humantime::format_duration`] that accepts
/// signed seconds (negative values are clamped to zero).
pub fn format_duration(secs: i64) -> String {
    let dur = Duration::from_secs(secs.max(0) as u64);
    humantime::format_duration(dur).to_string()
}

/// Compare two dotted version strings (e.g. "1.2.3" vs "1.3.0").
/// Returns `Ordering::Less` if `a < b`, etc.
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let va = parse(a);
    let vb = parse(b);
    va.cmp(&vb)
}

/// Truncate a string to at most `max` characters, appending `…` if truncated.
pub fn truncate_str(s: &str, max: usize) -> String {
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
    fn compare_major_minor_patch() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("1.2.3", "1.2.3"), Ordering::Equal);
        assert_eq!(compare_versions("1.2.3", "1.2.4"), Ordering::Less);
        assert_eq!(compare_versions("1.3.0", "1.2.9"), Ordering::Greater);
        assert_eq!(compare_versions("2.0.0", "1.99.99"), Ordering::Greater);
    }

    #[test]
    fn compare_different_lengths() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0.0", "1.0.0"), Ordering::Greater);
    }

    #[test]
    fn compare_non_numeric_segments() {
        use std::cmp::Ordering;
        // Non-numeric segments parse as 0
        assert_eq!(compare_versions("1.beta.3", "1.0.3"), Ordering::Equal);
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

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_truncate_str_length_invariant(s in "\\PC{0,200}", max in 0..=200_usize) {
                let result = truncate_str(&s, max);
                let result_chars = result.chars().count();
                prop_assert!(
                    result_chars <= max,
                    "truncate_str({:?}, {}) produced {} chars",
                    s, max, result_chars
                );
            }

            #[test]
            fn prop_compare_versions_never_panics(a in "\\PC{0,30}", b in "\\PC{0,30}") {
                let _ = compare_versions(&a, &b);
            }

            #[test]
            fn prop_compare_versions_reflexive(a in "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}") {
                assert_eq!(compare_versions(&a, &a), std::cmp::Ordering::Equal);
            }

            #[test]
            fn prop_format_duration_never_panics(secs in i64::MIN..=i64::MAX) {
                drop(format_duration(secs));
            }

            #[test]
            fn prop_format_duration_negative_equals_zero(secs in i64::MIN..=-1_i64) {
                assert_eq!(format_duration(secs), format_duration(0));
            }
        }
    }
}
