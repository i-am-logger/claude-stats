use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

/// Claude's context window size in tokens. Used to compute context utilisation
/// percentage. Must be non-zero to avoid division-by-zero in `parse_session`.
pub(crate) const CONTEXT_WINDOW: u64 = 166_000;
const _: () = assert!(CONTEXT_WINDOW > 0);

/// How far from the end of a session file to start scanning for token usage on
/// the first attempt (256 KB). Doubled on retry up to `MAX_TAIL_BYTES`.
const INITIAL_TAIL_BYTES: u64 = 256_000;

/// Hard ceiling for the tail scan window (2 MB). Prevents unbounded reads on
/// very large session files.
const MAX_TAIL_BYTES: u64 = 2_000_000;

pub(super) struct TailStats {
    pub last_tokens: u64,
    pub compactions: u32,
}

/// Seek to `seek_pos` in the file, discard the partial first line if not at
/// the start, and return a `BufReader` positioned at the first complete line.
pub(super) fn seek_tail(file: &fs::File, seek_pos: u64) -> Option<BufReader<&fs::File>> {
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(seek_pos)).ok()?;
    if seek_pos > 0 {
        let mut discard = String::new();
        reader.read_line(&mut discard).ok()?;
    }
    Some(reader)
}

/// Try to parse a line as JSON. Returns `None` on failure.
pub(super) fn parse_json_line(line: &str) -> Option<serde_json::Value> {
    serde_json::from_str(line).ok()
}

pub(super) fn parse_session_metadata(file: &fs::File) -> Option<(String, String)> {
    let mut file = file;
    if file.seek(SeekFrom::Start(0)).is_err() {
        return None;
    }
    let reader = BufReader::new(&mut file);

    for line in reader.lines().map_while(Result::ok).take(5) {
        let Some(val) = parse_json_line(&line) else {
            continue;
        };

        if val.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }

        let cwd = val.get("cwd").and_then(|v| v.as_str())?.to_string();
        let git_branch = val
            .get("gitBranch")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        return Some((cwd, git_branch));
    }

    None
}

pub(super) fn parse_tail_stats(file: &fs::File, file_len: u64) -> TailStats {
    let mut tail_bytes = INITIAL_TAIL_BYTES;
    loop {
        let stats = scan_tail_window(file, file_len, tail_bytes);
        if stats.last_tokens > 0 || tail_bytes >= file_len || tail_bytes >= MAX_TAIL_BYTES {
            return stats;
        }
        tail_bytes = tail_bytes.saturating_mul(2).min(MAX_TAIL_BYTES);
    }
}

fn scan_tail_window(file: &fs::File, file_len: u64, tail_bytes: u64) -> TailStats {
    let seek_pos = file_len.saturating_sub(tail_bytes);
    let Some(reader) = seek_tail(file, seek_pos) else {
        return TailStats {
            last_tokens: 0,
            compactions: 0,
        };
    };

    let mut last_tokens: u64 = 0;
    let mut compactions: u32 = 0;

    for line in reader.lines().map_while(Result::ok) {
        if line.contains("\"compact_boundary\"") {
            compactions += 1;
            last_tokens = 0;
            continue;
        }

        if !is_usage_line(&line) {
            continue;
        }

        let Some(val) = parse_json_line(&line) else {
            continue;
        };

        if let Some(usage) = val.pointer("/message/usage") {
            let total = extract_tokens(usage);
            if total > 0 {
                last_tokens = total;
            }
        }
    }

    TailStats {
        last_tokens,
        compactions,
    }
}

/// Fast pre-filter: returns `true` when a raw JSONL line looks like an
/// assistant message that carries a `usage` object. Used by both the main
/// tail scanner and the subagent reader to skip irrelevant lines cheaply
/// before attempting JSON parsing.
pub(super) fn is_usage_line(line: &str) -> bool {
    line.contains("\"type\":\"assistant\"")
        && !line.contains("\"type\":\"progress\"")
        && line.contains("\"usage\"")
}

pub(super) fn extract_tokens(usage: &serde_json::Value) -> u64 {
    let input = usage
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cache_create = usage
        .get("cache_creation_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cache_read = usage
        .get("cache_read_input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    input + cache_create + cache_read
}

pub(super) fn read_last_lines(file: &fs::File, file_len: u64, count: usize) -> Vec<String> {
    if file_len == 0 {
        return Vec::new();
    }
    let seek_pos = file_len.saturating_sub(super::RECENT_TAIL_BYTES);
    let Some(reader) = seek_tail(file, seek_pos) else {
        return Vec::new();
    };
    let mut lines: VecDeque<String> = VecDeque::new();
    for line in reader.lines().map_while(Result::ok) {
        if !line.is_empty() {
            lines.push_back(line);
            if lines.len() > count {
                lines.pop_front();
            }
        }
    }
    lines.into()
}

pub(super) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max < 4 {
        s.chars().take(max).collect()
    } else {
        let truncated: String = s.chars().take(max - 3).collect();
        format!("{truncated}...")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_tokens_all_fields() {
        let usage = serde_json::json!({
            "input_tokens": 100,
            "cache_creation_input_tokens": 50,
            "cache_read_input_tokens": 25
        });
        assert_eq!(extract_tokens(&usage), 175);
    }

    #[test]
    fn extract_tokens_missing_fields() {
        let usage = serde_json::json!({ "input_tokens": 42 });
        assert_eq!(extract_tokens(&usage), 42);
    }

    #[test]
    fn extract_tokens_empty_object() {
        let usage = serde_json::json!({});
        assert_eq!(extract_tokens(&usage), 0);
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
        assert_eq!(truncate_str("hello world", 8), "hello...");
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
    fn truncate_str_max_three() {
        assert_eq!(truncate_str("hello", 3), "hel");
    }

    #[test]
    fn truncate_str_max_four() {
        assert_eq!(truncate_str("hello world", 4), "h...");
    }

    #[test]
    fn parse_json_line_valid() {
        let val = parse_json_line(r#"{"key":"value"}"#);
        assert!(val.is_some());
        assert_eq!(val.unwrap()["key"], "value");
    }

    #[test]
    fn parse_json_line_invalid() {
        assert!(parse_json_line("not json").is_none());
    }

    #[test]
    fn parse_json_line_empty() {
        assert!(parse_json_line("").is_none());
    }

    #[test]
    fn is_usage_line_matches_assistant_with_usage() {
        let line = r#"{"type":"assistant","message":{"usage":{"input_tokens":10}}}"#;
        assert!(is_usage_line(line));
    }

    #[test]
    fn is_usage_line_rejects_progress() {
        let line = r#"{"type":"progress","type":"assistant","usage":{}}"#;
        assert!(!is_usage_line(line));
    }

    #[test]
    fn is_usage_line_rejects_no_usage() {
        let line = r#"{"type":"assistant","message":{"content":"hi"}}"#;
        assert!(!is_usage_line(line));
    }

    #[test]
    fn is_usage_line_rejects_user() {
        let line = r#"{"type":"user","usage":{}}"#;
        assert!(!is_usage_line(line));
    }

    // ── edge cases ─────────────────────────────────────────────────

    #[test]
    fn parse_json_line_truncated_json() {
        assert!(parse_json_line(r#"{"type":"assistant","message":"#).is_none());
    }

    #[test]
    fn parse_json_line_nested_braces() {
        let val = parse_json_line(r#"{"a":{"b":{"c":1}}}"#);
        assert!(val.is_some());
        assert_eq!(val.unwrap().pointer("/a/b/c").unwrap(), 1);
    }

    #[test]
    fn extract_tokens_negative_values_treated_as_zero() {
        // JSON numbers that are negative won't parse as u64
        let usage = serde_json::json!({"input_tokens": -5});
        assert_eq!(extract_tokens(&usage), 0);
    }

    #[test]
    fn extract_tokens_float_values_treated_as_zero() {
        let usage = serde_json::json!({"input_tokens": 1.5});
        assert_eq!(extract_tokens(&usage), 0);
    }

    #[test]
    fn is_usage_line_empty_string() {
        assert!(!is_usage_line(""));
    }

    #[test]
    fn truncate_str_multibyte_unicode() {
        // 4 emoji chars, truncate at 3 — should get 0 chars + "..."
        // but since max=3 < 4, it takes first 3 chars without ellipsis
        assert_eq!(truncate_str("🎉🎊🎈🎆", 3), "🎉🎊🎈");
    }

    #[test]
    fn truncate_str_multibyte_exact_fit() {
        // 4 chars at max=4 fits exactly — no truncation
        assert_eq!(truncate_str("🎉🎊🎈🎆", 4), "🎉🎊🎈🎆");
    }

    #[test]
    fn truncate_str_multibyte_with_ellipsis() {
        // 5 emoji chars at max=4 → 1 char + "..."
        assert_eq!(truncate_str("🎉🎊🎈🎆🎇", 4), "🎉...");
    }
}
