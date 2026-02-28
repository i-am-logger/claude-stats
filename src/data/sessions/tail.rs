#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

/// Claude's context window size in tokens. Used to compute context utilisation
/// percentage. Must be non-zero to avoid division-by-zero in `parse_session`.
pub const CONTEXT_WINDOW: u64 = 166_000;
const _: () = assert!(CONTEXT_WINDOW > 0);

/// Number of recent lines to retain for state detection.
pub(crate) const RECENT_LINES: usize = 5;

/// Result of scanning an entire session file in a single pass.
#[derive(Debug)]
pub struct SessionFileData {
    pub cwd: String,
    pub git_branch: String,
    pub last_tokens: u64,
    pub compactions: u32,
    pub last_lines: VecDeque<String>,
    /// `agentId` → `parentToolUseID` mapping from `agent_progress` entries.
    pub agent_tool_map: HashMap<String, String>,
    /// Set of `tool_use_id` values that have received a `tool_result`.
    pub completed_tool_ids: HashSet<String>,
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

/// Accumulator for the shared scan loop logic. Both `scan_session_file` and
/// `scan_from_offset` delegate per-line processing here.
struct ScanState {
    last_tokens: u64,
    compactions: u32,
    recent: VecDeque<String>,
    agent_tool_map: HashMap<String, String>,
    completed_tool_ids: HashSet<String>,
}

impl ScanState {
    fn new() -> Self {
        Self {
            last_tokens: 0,
            compactions: 0,
            recent: VecDeque::new(),
            agent_tool_map: HashMap::new(),
            completed_tool_ids: HashSet::new(),
        }
    }

    fn process_line(&mut self, line: &str) {
        if !line.is_empty() {
            self.recent.push_back(line.to_string());
            if self.recent.len() > RECENT_LINES {
                self.recent.pop_front();
            }
        }

        let Some(val) = parse_json_line(line) else {
            return;
        };

        self.process_parsed(&val);
    }

    fn process_parsed(&mut self, val: &serde_json::Value) {
        if is_compact_boundary(val) {
            self.compactions += 1;
            self.last_tokens = 0;
            return;
        }

        if is_assistant_usage(val) {
            if let Some(usage) = val.pointer("/message/usage") {
                let total = extract_tokens(usage);
                if total > 0 {
                    self.last_tokens = total;
                }
            }
        }

        extract_agent_data(val, &mut self.agent_tool_map, &mut self.completed_tool_ids);
    }

    fn into_result(self) -> ScanResult {
        ScanResult {
            last_tokens: self.last_tokens,
            compactions: self.compactions,
            last_lines: self.recent,
            agent_tool_map: self.agent_tool_map,
            completed_tool_ids: self.completed_tool_ids,
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
    let mut scan = ScanState::new();

    for line in reader.lines().map_while(Result::ok) {
        if !line.is_empty() {
            scan.recent.push_back(line.clone());
            if scan.recent.len() > RECENT_LINES {
                scan.recent.pop_front();
            }
        }

        let Some(val) = parse_json_line(&line) else {
            continue;
        };

        // Extract metadata from the first user line (before it has been seen)
        if cwd.is_none() && val.get("type").and_then(|t| t.as_str()) == Some("user") {
            cwd = val.get("cwd").and_then(|v| v.as_str()).map(String::from);
            git_branch = val
                .get("gitBranch")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
        }

        scan.process_parsed(&val);
    }

    let result = scan.into_result();
    Some(SessionFileData {
        cwd: cwd?,
        git_branch,
        last_tokens: result.last_tokens,
        compactions: result.compactions,
        last_lines: result.last_lines,
        agent_tool_map: result.agent_tool_map,
        completed_tool_ids: result.completed_tool_ids,
    })
}

/// Result of an incremental scan (from a byte offset to EOF).
#[derive(Debug)]
pub struct ScanResult {
    pub last_tokens: u64,
    pub compactions: u32,
    pub last_lines: VecDeque<String>,
    /// `agentId` → `parentToolUseID` mapping from `agent_progress` entries.
    pub agent_tool_map: HashMap<String, String>,
    /// Set of `tool_use_id` values that have received a `tool_result`.
    pub completed_tool_ids: HashSet<String>,
}

/// Scan a session file from `offset` to EOF. Tracks token usage, compaction
/// markers, and the last few non-empty lines. Used for incremental reads after
/// the initial full scan.
pub fn scan_from_offset(file: &fs::File, offset: u64) -> ScanResult {
    let mut file = file;
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return ScanState::new().into_result();
    }
    let reader = BufReader::new(&mut file);

    let mut scan = ScanState::new();
    for line in reader.lines().map_while(Result::ok) {
        scan.process_line(&line);
    }

    scan.into_result()
}

/// Returns `true` when the JSON value is a `compact_boundary` system message.
fn is_compact_boundary(val: &serde_json::Value) -> bool {
    val.get("subtype").and_then(|s| s.as_str()) == Some("compact_boundary")
}

/// Returns `true` when the JSON value is an assistant message with `usage`.
///
/// Matches both `"type":"assistant"` (legacy) and `"type":"message"`
/// (current Claude Code format).
pub fn is_assistant_usage(val: &serde_json::Value) -> bool {
    matches!(
        val.get("type").and_then(|t| t.as_str()),
        Some("assistant" | "message")
    ) && val.pointer("/message/usage").is_some()
}

/// Extract agent tracking data from a single JSON line.
///
/// Looks for `agent_progress` entries (maps agentId → parentToolUseID) and
/// user messages containing `tool_result` items (marks `tool_use_ids` as
/// completed).
fn extract_agent_data(
    val: &serde_json::Value,
    agent_tool_map: &mut HashMap<String, String>,
    completed_tool_ids: &mut HashSet<String>,
) {
    // agent_progress: maps agentId → parentToolUseID
    if val.pointer("/data/type").and_then(|t| t.as_str()) == Some("agent_progress") {
        if let (Some(agent_id), Some(tool_id)) = (
            val.pointer("/data/agentId")
                .and_then(serde_json::Value::as_str),
            val.get("parentToolUseID")
                .and_then(serde_json::Value::as_str),
        ) {
            agent_tool_map
                .entry(agent_id.to_string())
                .or_insert_with(|| tool_id.to_string());
        }
    }

    // tool_result in user messages: marks tool_use_ids as completed.
    // Only user messages carry tool_result items, so skip other line types.
    if val.get("type").and_then(|t| t.as_str()) == Some("user") {
        if let Some(content) = val.pointer("/message/content").and_then(|v| v.as_array()) {
            for item in content {
                if item.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    if let Some(id) = item.get("tool_use_id").and_then(serde_json::Value::as_str) {
                        completed_tool_ids.insert(id.to_string());
                    }
                }
            }
        }
    }
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

pub fn read_last_lines(file: &fs::File, file_len: u64, count: usize) -> VecDeque<String> {
    if file_len == 0 {
        return VecDeque::new();
    }
    let seek_pos = file_len.saturating_sub(super::RECENT_TAIL_BYTES);
    let Some(reader) = seek_tail(file, seek_pos) else {
        return VecDeque::new();
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
    lines
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
    fn is_assistant_usage_message_type() {
        let val = serde_json::json!({
            "type": "message",
            "message": {"usage": {"input_tokens": 100}}
        });
        assert!(is_assistant_usage(&val));
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

    // ── extract_agent_data ─────────────────────────────────────────

    #[test]
    fn extract_agent_data_progress_entry() {
        let val = serde_json::json!({
            "type": "progress",
            "parentToolUseID": "toolu_001",
            "data": {"type": "agent_progress", "agentId": "abc123"}
        });
        let mut map = HashMap::new();
        let mut completed = HashSet::new();
        extract_agent_data(&val, &mut map, &mut completed);
        assert_eq!(map.get("abc123").unwrap(), "toolu_001");
        assert!(completed.is_empty());
    }

    #[test]
    fn extract_agent_data_tool_result_in_user_message() {
        let val = serde_json::json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_001", "content": "done"}
            ]}
        });
        let mut map = HashMap::new();
        let mut completed = HashSet::new();
        extract_agent_data(&val, &mut map, &mut completed);
        assert!(map.is_empty());
        assert!(completed.contains("toolu_001"));
    }

    #[test]
    fn extract_agent_data_ignores_non_user_content() {
        // Assistant messages with content array should NOT extract tool_result
        let val = serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_001", "content": "done"}
            ]}
        });
        let mut map = HashMap::new();
        let mut completed = HashSet::new();
        extract_agent_data(&val, &mut map, &mut completed);
        assert!(completed.is_empty());
    }

    #[test]
    fn extract_agent_data_irrelevant_line() {
        let val = serde_json::json!({"type": "system", "subtype": "init"});
        let mut map = HashMap::new();
        let mut completed = HashSet::new();
        extract_agent_data(&val, &mut map, &mut completed);
        assert!(map.is_empty());
        assert!(completed.is_empty());
    }

    #[test]
    fn extract_agent_data_first_tool_id_wins() {
        let p1 = serde_json::json!({
            "type": "progress",
            "parentToolUseID": "toolu_001",
            "data": {"type": "agent_progress", "agentId": "abc"}
        });
        let p2 = serde_json::json!({
            "type": "progress",
            "parentToolUseID": "toolu_999",
            "data": {"type": "agent_progress", "agentId": "abc"}
        });
        let mut map = HashMap::new();
        let mut completed = HashSet::new();
        extract_agent_data(&p1, &mut map, &mut completed);
        extract_agent_data(&p2, &mut map, &mut completed);
        // First insertion wins (or_insert_with)
        assert_eq!(map.get("abc").unwrap(), "toolu_001");
    }

    #[test]
    fn extract_agent_data_missing_agent_id() {
        let val = serde_json::json!({
            "type": "progress",
            "parentToolUseID": "toolu_001",
            "data": {"type": "agent_progress"}
        });
        let mut map = HashMap::new();
        let mut completed = HashSet::new();
        extract_agent_data(&val, &mut map, &mut completed);
        assert!(map.is_empty());
    }

    #[test]
    fn extract_agent_data_missing_parent_tool_use_id() {
        let val = serde_json::json!({
            "type": "progress",
            "data": {"type": "agent_progress", "agentId": "abc"}
        });
        let mut map = HashMap::new();
        let mut completed = HashSet::new();
        extract_agent_data(&val, &mut map, &mut completed);
        assert!(map.is_empty());
    }

    // ── seek_tail ──────────────────────────────────────────────────

    /// Write raw bytes to a temp file and return it.
    fn raw_file(content: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.as_file().sync_all().unwrap();
        f
    }

    #[test]
    fn seek_tail_from_start() {
        let f = raw_file(b"line1\nline2\nline3\n");
        let reader = seek_tail(f.as_file(), 0).unwrap();
        let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
    }

    #[test]
    fn seek_tail_discards_partial_first_line() {
        let f = raw_file(b"line1\nline2\nline3\n");
        // Seek into the middle of "line1" — should discard remainder and start at "line2"
        let reader = seek_tail(f.as_file(), 2).unwrap();
        let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
        assert_eq!(lines, vec!["line2", "line3"]);
    }

    #[test]
    fn seek_tail_at_newline_boundary() {
        let f = raw_file(b"line1\nline2\n");
        // Seek to exactly after "line1\n" (byte 6) — should discard "line2" partial? No,
        // seek_pos > 0 means it reads+discards one line. At pos 6, it reads "line2" as discard.
        let reader = seek_tail(f.as_file(), 6).unwrap();
        let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
        assert!(lines.is_empty());
    }

    #[test]
    fn seek_tail_empty_file() {
        let f = raw_file(b"");
        let reader = seek_tail(f.as_file(), 0).unwrap();
        let lines: Vec<String> = reader.lines().map_while(Result::ok).collect();
        assert!(lines.is_empty());
    }

    // ── read_last_lines ──────────────────────────────────────────

    #[test]
    fn read_last_lines_normal() {
        let f = raw_file(b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n{\"d\":4}\n");
        let file_len = f.as_file().metadata().unwrap().len();
        let lines = read_last_lines(f.as_file(), file_len, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"c\""));
        assert!(lines[1].contains("\"d\""));
    }

    #[test]
    fn read_last_lines_fewer_than_count() {
        let f = raw_file(b"{\"a\":1}\n{\"b\":2}\n");
        let file_len = f.as_file().metadata().unwrap().len();
        let lines = read_last_lines(f.as_file(), file_len, 5);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn read_last_lines_empty_file() {
        let f = raw_file(b"");
        let lines = read_last_lines(f.as_file(), 0, 5);
        assert!(lines.is_empty());
    }

    #[test]
    fn read_last_lines_skips_empty_lines() {
        let f = raw_file(b"{\"a\":1}\n\n\n{\"b\":2}\n");
        let file_len = f.as_file().metadata().unwrap().len();
        let lines = read_last_lines(f.as_file(), file_len, 5);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"a\""));
        assert!(lines[1].contains("\"b\""));
    }

    #[test]
    fn read_last_lines_single_line() {
        let f = raw_file(b"only\n");
        let file_len = f.as_file().metadata().unwrap().len();
        let lines = read_last_lines(f.as_file(), file_len, 3);
        assert_eq!(lines, VecDeque::from(["only".to_string()]));
    }

    // ── scan_from_offset last_lines capping ──────────────────────

    #[test]
    fn scan_from_offset_caps_last_lines_at_recent_lines() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        // Write more lines than RECENT_LINES (5) so the deque must trim
        for i in 0..8 {
            writeln!(f, r#"{{"type":"system","n":{i}}}"#).unwrap();
        }
        f.as_file().sync_all().unwrap();

        let result = scan_from_offset(f.as_file(), 0);
        assert_eq!(result.last_lines.len(), RECENT_LINES);
        // Newest line should be n:7
        assert!(result.last_lines.back().unwrap().contains("\"n\":7"));
        // Oldest retained should be n:3
        assert!(result.last_lines.front().unwrap().contains("\"n\":3"));
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
