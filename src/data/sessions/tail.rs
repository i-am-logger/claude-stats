#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

/// Claude's context window size in tokens. Used to compute context utilisation
/// percentage. Must be non-zero to avoid division-by-zero in `parse_session`.
pub const CONTEXT_WINDOW: u64 = 166_000;
const _: () = assert!(CONTEXT_WINDOW > 0);

/// Number of recent lines to retain for state detection.
const RECENT_LINES: usize = 5;

/// Result of scanning an entire session file in a single pass.
#[derive(Debug)]
pub struct SessionFileData {
    pub cwd: String,
    pub git_branch: String,
    pub last_tokens: u64,
    pub compactions: u32,
    pub last_lines: Vec<String>,
}

/// Seek to `seek_pos` in the file, discard the partial first line if not at
/// the start, and return a `BufReader` positioned at the first complete line.
pub fn seek_tail(file: &fs::File, seek_pos: u64) -> Option<BufReader<&fs::File>> {
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(seek_pos)).ok()?;
    if seek_pos > 0 {
        let mut discard = String::new();
        reader.read_line(&mut discard).ok()?;
    }
    Some(reader)
}

/// Try to parse a line as JSON. Returns `None` on failure, logging non-empty
/// lines that fail to parse.
pub fn parse_json_line(line: &str) -> Option<serde_json::Value> {
    match serde_json::from_str(line) {
        Ok(val) => Some(val),
        Err(e) => {
            if !line.is_empty() {
                tracing::debug!(len = line.len(), error = %e, "failed to parse JSONL line");
            }
            None
        }
    }
}

/// Read the entire session file in one pass.
///
/// Extracts metadata (cwd, branch) from the first user line, and scans every
/// line for token usage, compaction markers, and the last few lines for state
/// detection.
pub fn scan_session_file(file: &fs::File) -> Option<SessionFileData> {
    let mut file = file;
    file.seek(SeekFrom::Start(0)).ok()?;
    let reader = BufReader::new(&mut file);

    let mut cwd: Option<String> = None;
    let mut git_branch = String::new();
    let mut last_tokens: u64 = 0;
    let mut compactions: u32 = 0;
    let mut recent: VecDeque<String> = VecDeque::new();

    for line in reader.lines().map_while(Result::ok) {
        if !line.is_empty() {
            recent.push_back(line.clone());
            if recent.len() > RECENT_LINES {
                recent.pop_front();
            }
        }

        let Some(val) = parse_json_line(&line) else {
            continue;
        };

        // Extract metadata from the first user line
        if cwd.is_none() && val.get("type").and_then(|t| t.as_str()) == Some("user") {
            cwd = val.get("cwd").and_then(|v| v.as_str()).map(String::from);
            git_branch = val
                .get("gitBranch")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }

        if is_compact_boundary(&val) {
            compactions += 1;
            last_tokens = 0;
            continue;
        }

        if is_assistant_usage(&val) {
            if let Some(usage) = val.pointer("/message/usage") {
                let total = extract_tokens(usage);
                if total > 0 {
                    last_tokens = total;
                }
            }
        }
    }

    Some(SessionFileData {
        cwd: cwd?,
        git_branch,
        last_tokens,
        compactions,
        last_lines: recent.into(),
    })
}

/// Result of an incremental scan (from a byte offset to EOF).
#[derive(Debug)]
pub struct ScanResult {
    pub last_tokens: u64,
    pub compactions: u32,
    pub last_lines: Vec<String>,
}

/// Scan a session file from `offset` to EOF. Tracks token usage, compaction
/// markers, and the last few non-empty lines. Used for incremental reads after
/// the initial full scan.
pub fn scan_from_offset(file: &fs::File, offset: u64) -> ScanResult {
    let mut file = file;
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return ScanResult {
            last_tokens: 0,
            compactions: 0,
            last_lines: Vec::new(),
        };
    }
    let reader = BufReader::new(&mut file);

    let mut last_tokens: u64 = 0;
    let mut compactions: u32 = 0;
    let mut recent: VecDeque<String> = VecDeque::new();

    for line in reader.lines().map_while(Result::ok) {
        if !line.is_empty() {
            recent.push_back(line.clone());
            if recent.len() > RECENT_LINES {
                recent.pop_front();
            }
        }

        let Some(val) = parse_json_line(&line) else {
            continue;
        };

        if is_compact_boundary(&val) {
            compactions += 1;
            last_tokens = 0;
            continue;
        }

        if is_assistant_usage(&val) {
            if let Some(usage) = val.pointer("/message/usage") {
                let total = extract_tokens(usage);
                if total > 0 {
                    last_tokens = total;
                }
            }
        }
    }

    ScanResult {
        last_tokens,
        compactions,
        last_lines: recent.into(),
    }
}

/// Returns `true` when the JSON value is a `compact_boundary` system message.
fn is_compact_boundary(val: &serde_json::Value) -> bool {
    val.get("subtype").and_then(|s| s.as_str()) == Some("compact_boundary")
}

/// Returns `true` when the JSON value is an assistant message with a `usage`
/// object (i.e. not a progress event).
pub fn is_assistant_usage(val: &serde_json::Value) -> bool {
    val.get("type").and_then(|t| t.as_str()) == Some("assistant")
        && val.pointer("/message/usage").is_some()
}

pub fn extract_tokens(usage: &serde_json::Value) -> u64 {
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
    input
        .saturating_add(cache_create)
        .saturating_add(cache_read)
}

pub fn read_last_lines(file: &fs::File, file_len: u64, count: usize) -> Vec<String> {
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
    fn is_assistant_usage_matches() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": {"usage": {"input_tokens": 10}}
        });
        assert!(is_assistant_usage(&val));
    }

    #[test]
    fn is_assistant_usage_rejects_progress() {
        let val = serde_json::json!({"type": "progress", "usage": {}});
        assert!(!is_assistant_usage(&val));
    }

    #[test]
    fn is_assistant_usage_rejects_no_usage() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": {"content": "hi"}
        });
        assert!(!is_assistant_usage(&val));
    }

    #[test]
    fn is_assistant_usage_rejects_user() {
        let val = serde_json::json!({"type": "user", "usage": {}});
        assert!(!is_assistant_usage(&val));
    }

    #[test]
    fn is_assistant_usage_with_spaces_in_json() {
        // Ensures JSON-based check works regardless of serialization format
        let val: serde_json::Value = serde_json::from_str(
            r#"{ "type" : "assistant" , "message" : { "usage" : { "input_tokens" : 5 } } }"#,
        )
        .unwrap();
        assert!(is_assistant_usage(&val));
    }

    #[test]
    fn is_compact_boundary_matches() {
        let val = serde_json::json!({"type": "system", "subtype": "compact_boundary"});
        assert!(is_compact_boundary(&val));
    }

    #[test]
    fn is_compact_boundary_rejects_other_subtypes() {
        let val = serde_json::json!({"type": "system", "subtype": "turn_duration"});
        assert!(!is_compact_boundary(&val));
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
    fn is_assistant_usage_empty_object() {
        let val = serde_json::json!({});
        assert!(!is_assistant_usage(&val));
    }

    #[test]
    fn extract_tokens_saturates_on_overflow() {
        let usage = serde_json::json!({
            "input_tokens": u64::MAX,
            "cache_creation_input_tokens": 1_u64,
        });
        assert_eq!(extract_tokens(&usage), u64::MAX);
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_parse_json_line_never_panics(s in "\\PC*") {
                drop(parse_json_line(&s));
            }

            #[test]
            fn prop_extract_tokens_never_panics(
                input in proptest::option::of(0..=u64::MAX),
                cache_create in proptest::option::of(0..=u64::MAX),
                cache_read in proptest::option::of(0..=u64::MAX),
            ) {
                let mut map = serde_json::Map::new();
                if let Some(v) = input {
                    map.insert("input_tokens".into(), v.into());
                }
                if let Some(v) = cache_create {
                    map.insert("cache_creation_input_tokens".into(), v.into());
                }
                if let Some(v) = cache_read {
                    map.insert("cache_read_input_tokens".into(), v.into());
                }
                let usage = serde_json::Value::Object(map);
                let _total = extract_tokens(&usage);
            }

            #[test]
            fn prop_extract_tokens_sum_correct(
                input in 0..=1_000_000_u64,
                cache_create in 0..=1_000_000_u64,
                cache_read in 0..=1_000_000_u64,
            ) {
                let usage = serde_json::json!({
                    "input_tokens": input,
                    "cache_creation_input_tokens": cache_create,
                    "cache_read_input_tokens": cache_read,
                });
                let result = extract_tokens(&usage);
                assert_eq!(result, input + cache_create + cache_read);
            }
        }
    }
}
