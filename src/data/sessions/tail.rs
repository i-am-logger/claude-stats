#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

/// Default context window size for models we don't recognise.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// Return the context window size for a model based on its API identifier.
///
/// The Claude 5 family (Fable/Mythos/Sonnet 5), Opus 4.6+, Sonnet 4.6, and
/// Sonnet 4.5 / Sonnet 4 expose a 1 M-token context window. Haiku, older
/// Opus models, and anything unrecognised use 200 k.
pub fn context_window_for_model(model: &str) -> u64 {
    // Haiku is the only current-generation family still at 200k
    if model.contains("haiku") {
        return DEFAULT_CONTEXT_WINDOW;
    }
    // Claude 5 family — 1M (also covers mythos-preview)
    if model.contains("fable") || model.contains("mythos") || model.contains("sonnet-5") {
        return 1_000_000;
    }
    // Opus 4.6 / 4.7 / 4.8 — 1M
    if model.contains("opus-4-6") || model.contains("opus-4-7") || model.contains("opus-4-8") {
        return 1_000_000;
    }
    // Sonnet 4.6, Sonnet 4.5, and Sonnet 4 ("sonnet-4-2..." matches the
    // date-suffixed claude-sonnet-4-20250514) — 1M
    if model.contains("sonnet-4-6") || model.contains("sonnet-4-5") || model.contains("sonnet-4-2")
    {
        return 1_000_000;
    }
    // Older Opus and anything unknown → 200k
    DEFAULT_CONTEXT_WINDOW
}

/// Number of recent lines to retain for state detection.
pub(crate) const RECENT_LINES: usize = 5;

/// How a subagent was launched — determines completion detection.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentKind {
    /// Streams progress to parent. Completed when `tool_result` arrives
    /// for `parent_tool_use_id`.
    Foreground { parent_tool_use_id: String },
    /// Runs independently. Completed when a `queue-operation` enqueue
    /// with `<task-id>` and `<status>completed</status>` appears.
    Background,
}

/// Lifecycle status of a named teammate observed in the parent session JSONL.
///
/// Teammates (Agent tool with a `name`) never emit `agent_progress` entries;
/// their lifecycle shows up as a `toolUseResult.status == "teammate_spawned"`
/// entry on spawn and `<teammate-message>` idle notifications when they go
/// idle. Idle means "available", not terminated — a teammate can resume via
/// its mailbox with no new spawn marker.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TeammateStatus {
    /// Timestamp of the most recent `teammate_spawned` marker.
    pub spawned_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Timestamp of the most recent idle notification.
    pub last_idle_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl TeammateStatus {
    /// Whether the teammate has gone idle since its last spawn.
    pub fn is_idle(&self) -> bool {
        match (self.spawned_at, self.last_idle_at) {
            (Some(spawned), Some(idle)) => idle > spawned,
            (None, Some(_)) => true,
            _ => false,
        }
    }
}

/// Parse the top-level `.timestamp` field of a session entry.
pub(crate) fn entry_timestamp(val: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let ts = val.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Extract the teammate name from a `<teammate-message teammate_id="name@team">`
/// tag inside a message content string.
fn teammate_name_from_message(content: &str) -> Option<&str> {
    let attr = content.split_once("teammate_id=\"")?.1;
    let id = attr.split_once('"')?.0;
    Some(id.split('@').next().unwrap_or(id))
}

/// Tracks all subagents observed in a session JSONL.
#[derive(Debug, Clone, Default)]
pub(crate) struct AgentTracker {
    agents: HashMap<String, AgentKind>,
    /// `tool_use_id`s that received a `tool_result` (foreground completion).
    completed_tool_ids: HashSet<String>,
    /// Agent IDs that received a queue-operation completion (background).
    completed_background_ids: HashSet<String>,
    /// Named teammates keyed by name.
    teammates: HashMap<String, TeammateStatus>,
}

impl AgentTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extract agent data from a single parsed JSONL value.
    pub fn process(&mut self, val: &serde_json::Value) {
        match val.get("type").and_then(|t| t.as_str()) {
            Some("progress") => {
                if val.pointer("/data/type").and_then(|t| t.as_str()) != Some("agent_progress") {
                    return;
                }
                if let (Some(agent_id), Some(tool_id)) = (
                    val.pointer("/data/agentId")
                        .and_then(serde_json::Value::as_str),
                    val.get("parentToolUseID")
                        .and_then(serde_json::Value::as_str),
                ) {
                    self.agents.entry(agent_id.to_string()).or_insert_with(|| {
                        AgentKind::Foreground {
                            parent_tool_use_id: tool_id.to_string(),
                        }
                    });
                }
            }
            Some("user") => {
                // tool_result items mark foreground completion
                if let Some(content) = val.pointer("/message/content").and_then(|v| v.as_array()) {
                    for item in content {
                        if item.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                            if let Some(id) =
                                item.get("tool_use_id").and_then(serde_json::Value::as_str)
                            {
                                self.completed_tool_ids.insert(id.to_string());
                            }
                        }
                    }
                }

                // toolUseResult with isAsync: true → Background agent
                if let Some(tur) = val.pointer("/toolUseResult") {
                    if tur.get("isAsync").and_then(serde_json::Value::as_bool) == Some(true) {
                        if let Some(agent_id) =
                            tur.get("agentId").and_then(serde_json::Value::as_str)
                        {
                            self.agents
                                .entry(agent_id.to_string())
                                .or_insert(AgentKind::Background);
                        }
                    }

                    // toolUseResult with status "teammate_spawned" → named teammate
                    if tur.get("status").and_then(serde_json::Value::as_str)
                        == Some("teammate_spawned")
                    {
                        if let Some(name) = tur.get("name").and_then(serde_json::Value::as_str) {
                            let status = self.teammates.entry(name.to_string()).or_default();
                            status.spawned_at = status.spawned_at.max(entry_timestamp(val));
                        }
                    }
                }

                // A <teammate-message> idle notification marks a teammate idle.
                // (A teammate-message WITHOUT idle_notification is its report —
                // informational, not a lifecycle signal.)
                if let Some(serde_json::Value::String(content)) = val.pointer("/message/content") {
                    if content.contains("<teammate-message")
                        && content.contains("idle_notification")
                    {
                        if let Some(name) = teammate_name_from_message(content) {
                            let status = self.teammates.entry(name.to_string()).or_default();
                            status.last_idle_at = status.last_idle_at.max(entry_timestamp(val));
                        }
                    }
                }
            }
            Some("queue-operation") => {
                if val.get("operation").and_then(|o| o.as_str()) != Some("enqueue") {
                    return;
                }
                if let Some(content) = val.get("content").and_then(|c| c.as_str()) {
                    let mut task_id = None;
                    let mut completed = false;
                    for line in content.lines() {
                        let line = line.trim();
                        if let Some(id) = line
                            .strip_prefix("<task-id>")
                            .and_then(|s| s.strip_suffix("</task-id>"))
                        {
                            task_id = Some(id);
                        }
                        if line == "<status>completed</status>" {
                            completed = true;
                        }
                    }
                    if let (true, Some(id)) = (completed, task_id) {
                        self.completed_background_ids.insert(id.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    /// Merge incremental scan results into this tracker.
    pub fn merge(&mut self, other: &Self) {
        for (id, kind) in &other.agents {
            self.agents
                .entry(id.clone())
                .or_insert_with(|| kind.clone());
        }
        self.completed_tool_ids
            .extend(other.completed_tool_ids.iter().cloned());
        self.completed_background_ids
            .extend(other.completed_background_ids.iter().cloned());
        for (name, status) in &other.teammates {
            let entry = self.teammates.entry(name.clone()).or_default();
            entry.spawned_at = entry.spawned_at.max(status.spawned_at);
            entry.last_idle_at = entry.last_idle_at.max(status.last_idle_at);
        }
    }

    /// Named teammates observed so far, keyed by name.
    pub fn teammates(&self) -> &HashMap<String, TeammateStatus> {
        &self.teammates
    }

    /// Return the set of agent IDs that are still active.
    pub fn active_ids(&self) -> HashSet<String> {
        self.agents
            .iter()
            .filter(|(id, kind)| match kind {
                AgentKind::Foreground { parent_tool_use_id } => {
                    !self.completed_tool_ids.contains(parent_tool_use_id)
                }
                AgentKind::Background => !self.completed_background_ids.contains(id.as_str()),
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// Result of scanning an entire session file in a single pass.
#[derive(Debug)]
pub struct SessionFileData {
    pub cwd: String,
    pub git_branch: String,
    pub last_tokens: u64,
    pub compactions: u32,
    pub last_lines: VecDeque<String>,
    /// Raw model identifier from the most recent assistant message (e.g.
    /// `"claude-opus-4-6"`). Empty if no assistant message was seen.
    pub model: String,
    pub(crate) tracker: AgentTracker,
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
    tracker: AgentTracker,
    model: String,
}

impl ScanState {
    fn new() -> Self {
        Self {
            last_tokens: 0,
            compactions: 0,
            recent: VecDeque::new(),
            tracker: AgentTracker::new(),
            model: String::new(),
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
            if let Some(m) = val
                .pointer("/message/model")
                .and_then(serde_json::Value::as_str)
            {
                if !m.is_empty() {
                    self.model = m.to_string();
                }
            }
        }

        self.tracker.process(val);
    }

    fn into_result(self) -> ScanResult {
        ScanResult {
            last_tokens: self.last_tokens,
            compactions: self.compactions,
            last_lines: self.recent,
            tracker: self.tracker,
            model: self.model,
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
        model: result.model,
        tracker: result.tracker,
    })
}

/// Result of an incremental scan (from a byte offset to EOF).
#[derive(Debug)]
pub struct ScanResult {
    pub last_tokens: u64,
    pub compactions: u32,
    pub last_lines: VecDeque<String>,
    pub(crate) tracker: AgentTracker,
    pub model: String,
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

    // ── context_window_for_model ──────────────────────────────────

    #[test]
    fn context_window_fable_5() {
        assert_eq!(context_window_for_model("claude-fable-5"), 1_000_000);
    }

    #[test]
    fn context_window_mythos_5() {
        assert_eq!(context_window_for_model("claude-mythos-5"), 1_000_000);
    }

    #[test]
    fn context_window_sonnet_5() {
        assert_eq!(context_window_for_model("claude-sonnet-5"), 1_000_000);
    }

    #[test]
    fn context_window_opus_4_8() {
        assert_eq!(context_window_for_model("claude-opus-4-8"), 1_000_000);
    }

    #[test]
    fn context_window_opus_4_7() {
        assert_eq!(context_window_for_model("claude-opus-4-7"), 1_000_000);
    }

    #[test]
    fn context_window_opus_4_6() {
        assert_eq!(context_window_for_model("claude-opus-4-6"), 1_000_000);
    }

    #[test]
    fn context_window_sonnet_4_6() {
        assert_eq!(context_window_for_model("claude-sonnet-4-6"), 1_000_000);
    }

    #[test]
    fn context_window_sonnet_4_5() {
        assert_eq!(
            context_window_for_model("claude-sonnet-4-5-20250929"),
            1_000_000
        );
    }

    #[test]
    fn context_window_sonnet_4() {
        assert_eq!(
            context_window_for_model("claude-sonnet-4-20250514"),
            1_000_000
        );
    }

    #[test]
    fn context_window_haiku() {
        assert_eq!(
            context_window_for_model("claude-haiku-4-5-20251001"),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn context_window_opus_4() {
        assert_eq!(
            context_window_for_model("claude-opus-4-20250514"),
            DEFAULT_CONTEXT_WINDOW
        );
    }

    #[test]
    fn context_window_unknown() {
        assert_eq!(context_window_for_model("gpt-4o"), DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn context_window_empty() {
        assert_eq!(context_window_for_model(""), DEFAULT_CONTEXT_WINDOW);
    }

    // ── extract_tokens ────────────────────────────────────────────

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

    // ── AgentTracker ───────────────────────────────────────────────

    #[test]
    fn tracker_foreground_agent_via_progress() {
        let val = serde_json::json!({
            "type": "progress",
            "parentToolUseID": "toolu_001",
            "data": {"type": "agent_progress", "agentId": "abc123"}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert_eq!(
            tracker.agents.get("abc123"),
            Some(&AgentKind::Foreground {
                parent_tool_use_id: "toolu_001".into()
            })
        );
        assert!(tracker.active_ids().contains("abc123"));
    }

    #[test]
    fn tracker_foreground_agent_completed() {
        let progress = serde_json::json!({
            "type": "progress",
            "parentToolUseID": "toolu_001",
            "data": {"type": "agent_progress", "agentId": "abc123"}
        });
        let result = serde_json::json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_001", "content": "done"}
            ]}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&progress);
        tracker.process(&result);
        assert!(tracker.active_ids().is_empty());
    }

    #[test]
    fn tracker_background_agent_via_tool_use_result() {
        let val = serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "isAsync": true,
                "status": "async_launched",
                "agentId": "bg_abc"
            },
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_099", "content": "launched"}
            ]}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert_eq!(tracker.agents.get("bg_abc"), Some(&AgentKind::Background));
        assert!(tracker.active_ids().contains("bg_abc"));
    }

    #[test]
    fn tracker_background_agent_completed_via_queue_op() {
        let launch = serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "isAsync": true,
                "status": "async_launched",
                "agentId": "bg_abc"
            },
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_099", "content": "launched"}
            ]}
        });
        let complete = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "<task-notification>\n<task-id>bg_abc</task-id>\n<status>completed</status>\n</task-notification>"
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&launch);
        tracker.process(&complete);
        assert!(tracker.active_ids().is_empty());
    }

    #[test]
    fn tracker_background_no_false_positive() {
        // Normal tool_result without isAsync should NOT add a background agent
        let val = serde_json::json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_001", "content": "done"}
            ]}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.agents.is_empty());
        assert!(tracker.active_ids().is_empty());
    }

    #[test]
    fn tracker_active_ids_mixed() {
        let mut tracker = AgentTracker::new();

        // Foreground agent — completed
        let fg_progress = serde_json::json!({
            "type": "progress",
            "parentToolUseID": "toolu_001",
            "data": {"type": "agent_progress", "agentId": "fg_done"}
        });
        let fg_result = serde_json::json!({
            "type": "user",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_001", "content": "done"}
            ]}
        });
        tracker.process(&fg_progress);
        tracker.process(&fg_result);

        // Background agent — still active
        let bg_launch = serde_json::json!({
            "type": "user",
            "toolUseResult": {"isAsync": true, "agentId": "bg_active"},
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_002", "content": "launched"}
            ]}
        });
        tracker.process(&bg_launch);

        let active = tracker.active_ids();
        assert_eq!(active.len(), 1);
        assert!(active.contains("bg_active"));
        assert!(!active.contains("fg_done"));
    }

    #[test]
    fn tracker_merge_combines_both() {
        let mut a = AgentTracker::new();
        a.agents.insert(
            "fg1".into(),
            AgentKind::Foreground {
                parent_tool_use_id: "t1".into(),
            },
        );
        a.completed_tool_ids.insert("t1".into());

        let mut b = AgentTracker::new();
        b.agents.insert("bg1".into(), AgentKind::Background);

        a.merge(&b);
        assert_eq!(a.agents.len(), 2);
        assert!(a.agents.contains_key("bg1"));
        // fg1 should still be completed
        assert!(a.active_ids().contains("bg1"));
        assert!(!a.active_ids().contains("fg1"));
    }

    #[test]
    fn tracker_foreground_first_tool_id_wins() {
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
        let mut tracker = AgentTracker::new();
        tracker.process(&p1);
        tracker.process(&p2);
        assert_eq!(
            tracker.agents.get("abc"),
            Some(&AgentKind::Foreground {
                parent_tool_use_id: "toolu_001".into()
            })
        );
    }

    #[test]
    fn tracker_missing_agent_id_ignored() {
        let val = serde_json::json!({
            "type": "progress",
            "parentToolUseID": "toolu_001",
            "data": {"type": "agent_progress"}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.agents.is_empty());
    }

    #[test]
    fn tracker_missing_parent_tool_use_id_ignored() {
        let val = serde_json::json!({
            "type": "progress",
            "data": {"type": "agent_progress", "agentId": "abc"}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.agents.is_empty());
    }

    #[test]
    fn tracker_ignores_non_user_tool_result() {
        // Assistant messages with tool_result should NOT mark completion
        let val = serde_json::json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_001", "content": "done"}
            ]}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.completed_tool_ids.is_empty());
    }

    #[test]
    fn tracker_irrelevant_line_ignored() {
        let val = serde_json::json!({"type": "system", "subtype": "init"});
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.agents.is_empty());
        assert!(tracker.completed_tool_ids.is_empty());
        assert!(tracker.completed_background_ids.is_empty());
    }

    #[test]
    fn tracker_queue_op_non_completed_ignored() {
        let val = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "<task-notification>\n<task-id>bg_abc</task-id>\n<status>running</status>\n</task-notification>"
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.completed_background_ids.is_empty());
    }

    #[test]
    fn tracker_queue_op_no_status_ignored() {
        let val = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "please remember to always use proper architecture"
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.completed_background_ids.is_empty());
    }

    // ── teammate tracking ──────────────────────────────────────────

    fn teammate_spawn_entry(name: &str, ts: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "user",
            "timestamp": ts,
            "toolUseResult": {
                "status": "teammate_spawned",
                "name": name,
                "agent_id": format!("{name}@session-4e9fac7d"),
                "model": "claude-opus-4-8",
                "team_name": "session-4e9fac7d"
            }
        })
    }

    fn teammate_idle_entry(name: &str, ts: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "user",
            "timestamp": ts,
            "message": {
                "content": format!(
                    "<teammate-message teammate_id=\"{name}@session-4e9fac7d\">{{\"type\":\"idle_notification\",\"from\":\"{name}\"}}</teammate-message>"
                )
            }
        })
    }

    #[test]
    fn tracker_teammate_spawn_registers_active() {
        let mut tracker = AgentTracker::new();
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:48.994Z",
        ));
        let status = &tracker.teammates()["fix-docrefs"];
        assert!(status.spawned_at.is_some());
        assert!(!status.is_idle());
    }

    #[test]
    fn tracker_teammate_idle_after_spawn() {
        let mut tracker = AgentTracker::new();
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:48.994Z",
        ));
        tracker.process(&teammate_idle_entry(
            "fix-docrefs",
            "2026-07-09T02:07:22.456Z",
        ));
        assert!(tracker.teammates()["fix-docrefs"].is_idle());
    }

    #[test]
    fn tracker_teammate_respawn_after_idle_is_active() {
        let mut tracker = AgentTracker::new();
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:48.994Z",
        ));
        tracker.process(&teammate_idle_entry(
            "fix-docrefs",
            "2026-07-09T02:07:22.456Z",
        ));
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:10:00.000Z",
        ));
        assert!(!tracker.teammates()["fix-docrefs"].is_idle());
    }

    #[test]
    fn tracker_teammate_report_message_is_not_idle() {
        let val = serde_json::json!({
            "type": "user",
            "timestamp": "2026-07-09T02:05:00.000Z",
            "message": {
                "content": "<teammate-message teammate_id=\"fix-docrefs@team\" summary=\"progress report\">done with step 1</teammate-message>"
            }
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:48.994Z",
        ));
        tracker.process(&val);
        assert!(!tracker.teammates()["fix-docrefs"].is_idle());
    }

    #[test]
    fn tracker_teammate_merge_keeps_latest_timestamps() {
        let mut a = AgentTracker::new();
        a.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:48.994Z",
        ));
        let mut b = AgentTracker::new();
        b.process(&teammate_idle_entry(
            "fix-docrefs",
            "2026-07-09T02:07:22.456Z",
        ));
        a.merge(&b);
        assert!(a.teammates()["fix-docrefs"].is_idle());
    }

    #[test]
    fn teammate_name_parsed_from_message_tag() {
        assert_eq!(
            teammate_name_from_message(
                "<teammate-message teammate_id=\"fix-docrefs@session-4e9fac7d\" ...>"
            ),
            Some("fix-docrefs")
        );
        assert_eq!(teammate_name_from_message("no tag here"), None);
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
        assert_eq!(reader.lines().map_while(Result::ok).count(), 0);
    }

    #[test]
    fn seek_tail_empty_file() {
        let f = raw_file(b"");
        let reader = seek_tail(f.as_file(), 0).unwrap();
        assert_eq!(reader.lines().map_while(Result::ok).count(), 0);
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
