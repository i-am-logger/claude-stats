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
    /// A `teammate_terminated` frame was seen — the teammate shut down.
    pub terminated: bool,
}

/// Parse the top-level `.timestamp` field of a session entry.
pub(crate) fn entry_timestamp(val: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let ts = val.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Whether a `"type":"user"` entry marks the start of a genuinely new turn,
/// as opposed to a `tool_result` delivery — those are ALSO stamped
/// `"type":"user"` (the Anthropic API's tool-result role), and a single turn
/// can contain many of them as the assistant works through several tool
/// calls. Claude Code's own `turnStartTime` only resets on a real new
/// prompt, not on each `tool_result` — treating every `"user"` line as a turn
/// boundary makes "time in current turn" reset every time a tool finishes,
/// undercounting badly on tool-heavy turns.
pub(crate) fn is_real_user_turn(val: &serde_json::Value) -> bool {
    if val.get("type").and_then(|t| t.as_str()) != Some("user") {
        return false;
    }
    match val.pointer("/message/content") {
        Some(serde_json::Value::Array(blocks)) => !blocks
            .iter()
            .all(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result")),
        Some(serde_json::Value::String(_)) => true,
        _ => false,
    }
}

/// Extract the teammate name from a `<teammate-message teammate_id="name@team">`
/// tag inside a message content string.
fn teammate_name_from_message(content: &str) -> Option<&str> {
    let attr = content.split_once("teammate_id=\"")?.1;
    let id = attr.split_once('"')?.0;
    Some(id.split('@').next().unwrap_or(id))
}

/// Extract the `"timestamp":"..."` embedded in a teammate notification body.
fn embedded_timestamp(content: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let after = content.split_once("\"timestamp\":\"")?.1;
    let ts = after.split_once('"')?.0;
    chrono::DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&chrono::Utc))
}

/// Extract the teammate name from a `teammate_terminated` frame — its
/// message reads `"<name> has shut down. ..."`.
fn terminated_teammate_name(content: &str) -> Option<String> {
    let msg = content.split_once("\"message\":\"")?.1;
    let msg = msg.split_once('"')?.0;
    let name = msg.split_whitespace().next()?;
    msg.contains("has shut down").then(|| name.to_string())
}

/// Split `content` into each individual `<teammate-message>...</teammate-message>`
/// block it contains. The lead's `InboxPoller` only delivers mail while idle,
/// so several different teammates' deliveries (each with their own
/// `teammate_id` and embedded JSON payload) can end up batched into one
/// relayed entry — every extraction helper above only ever looks at the
/// *first* occurrence within whatever string it's given, so each block must
/// be parsed in isolation rather than the whole message at once.
fn teammate_message_blocks(content: &str) -> Vec<&str> {
    const TAG_END: &str = "</teammate-message>";
    let mut blocks = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("<teammate-message") {
        let tail = &rest[start..];
        let Some(end) = tail.find(TAG_END) else {
            break;
        };
        let end = end + TAG_END.len();
        blocks.push(&tail[..end]);
        rest = &tail[end..];
    }
    blocks
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
    /// Workflow run human names keyed by `runId`, captured from the launch
    /// event — available before the run's script/snapshot are.
    workflow_names: HashMap<String, String>,
    /// Workflow run `taskId` keyed by `runId`, captured from the same launch
    /// event — locates the run's live progress file under
    /// `/tmp/claude-<uid>/<proj>/<sessionId>/tasks/<taskId>.output`.
    workflow_task_ids: HashMap<String, String>,
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

                    self.note_workflow_launch(tur);
                }

                // A <teammate-message> idle notification marks a teammate idle.
                // (A teammate-message WITHOUT idle_notification is its report —
                // informational, not a lifecycle signal.) The content can be a
                // plain string or an array of content blocks.
                match val.pointer("/message/content") {
                    Some(serde_json::Value::String(content)) => {
                        self.note_teammate_idle(content, val);
                    }
                    Some(serde_json::Value::Array(blocks)) => {
                        for block in blocks {
                            if let Some(text) =
                                block.get("text").and_then(serde_json::Value::as_str)
                            {
                                self.note_teammate_idle(text, val);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("queue-operation") => {
                if val.get("operation").and_then(|o| o.as_str()) != Some("enqueue") {
                    return;
                }
                if let Some(content) = val.get("content").and_then(|c| c.as_str()) {
                    let mut task_id = None;
                    let mut terminal = false;
                    for line in content.lines() {
                        let line = line.trim();
                        if let Some(id) = line
                            .strip_prefix("<task-id>")
                            .and_then(|s| s.strip_suffix("</task-id>"))
                        {
                            task_id = Some(id);
                        }
                        // Task-notification statuses mirror the registry's
                        // terminal states — a failed or user-stopped agent is
                        // just as finished as a completed one.
                        if let Some(status) = line
                            .strip_prefix("<status>")
                            .and_then(|s| s.strip_suffix("</status>"))
                        {
                            if matches!(status, "completed" | "failed" | "killed") {
                                terminal = true;
                            }
                        }
                    }
                    if let (true, Some(id)) = (terminal, task_id) {
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
            entry.terminated = entry.terminated || status.terminated;
        }
        for (run_id, name) in &other.workflow_names {
            self.workflow_names
                .entry(run_id.clone())
                .or_insert_with(|| name.clone());
        }
        for (run_id, task_id) in &other.workflow_task_ids {
            self.workflow_task_ids
                .entry(run_id.clone())
                .or_insert_with(|| task_id.clone());
        }
    }

    /// Named teammates observed so far, keyed by name.
    pub fn teammates(&self) -> &HashMap<String, TeammateStatus> {
        &self.teammates
    }

    /// Workflow run names observed via launch events, keyed by `runId`.
    pub fn workflow_names(&self) -> &HashMap<String, String> {
        &self.workflow_names
    }

    /// Workflow run `taskId`s observed via launch events, keyed by `runId`.
    pub fn workflow_task_ids(&self) -> &HashMap<String, String> {
        &self.workflow_task_ids
    }

    /// Tool-use ids that already received a `tool_result` — used to detect
    /// finished synchronous subagents via their meta.json `toolUseId`.
    pub fn completed_tool_ids(&self) -> &HashSet<String> {
        &self.completed_tool_ids
    }

    /// Record a workflow run's human name from its launch event
    /// (`toolUseResult.taskType == "local_workflow"`) — available immediately,
    /// before the run's persisted script or completion snapshot exist.
    fn note_workflow_launch(&mut self, tool_use_result: &serde_json::Value) {
        if tool_use_result
            .get("taskType")
            .and_then(serde_json::Value::as_str)
            != Some("local_workflow")
        {
            return;
        }
        let Some(run_id) = tool_use_result
            .get("runId")
            .and_then(serde_json::Value::as_str)
        else {
            return;
        };
        let Some(name) = tool_use_result
            .get("workflowName")
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                tool_use_result
                    .get("summary")
                    .and_then(serde_json::Value::as_str)
            })
        else {
            return;
        };
        self.workflow_names
            .entry(run_id.to_string())
            .or_insert_with(|| name.to_string());
        if let Some(task_id) = tool_use_result
            .get("taskId")
            .and_then(serde_json::Value::as_str)
        {
            self.workflow_task_ids
                .entry(run_id.to_string())
                .or_insert_with(|| task_id.to_string());
        }
    }

    /// Record teammate lifecycle frames carried in a teammate message. The
    /// exact JSON markers avoid false positives from reports that merely
    /// mention them in prose.
    fn note_teammate_idle(&mut self, content: &str, val: &serde_json::Value) {
        for block in teammate_message_blocks(content) {
            if block.contains("\"type\":\"idle_notification\"") {
                if let Some(name) = teammate_name_from_message(block) {
                    // Prefer the notification's embedded timestamp — delivery
                    // to the parent JSONL can lag minutes behind the actual
                    // idle.
                    let ts = embedded_timestamp(block).or_else(|| entry_timestamp(val));
                    let status = self.teammates.entry(name.to_string()).or_default();
                    status.last_idle_at = status.last_idle_at.max(ts);
                }
            }
            // "X has shut down" — the only explicit termination frame.
            if block.contains("\"type\":\"teammate_terminated\"") {
                if let Some(name) = terminated_teammate_name(block) {
                    self.teammates.entry(name).or_default().terminated = true;
                }
            }
        }
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
    /// Timestamp of the latest `user`-type line — "time in the current
    /// turn", the same semantics as a subagent row's runtime.
    pub last_user_ts: Option<chrono::DateTime<chrono::Utc>>,
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
    git_branch: String,
    /// `cwd` from the first `user`-type line seen in this scan range; empty
    /// if none. A session's cwd doesn't change mid-session (unlike
    /// `git_branch`), so the first one found is kept.
    cwd: String,
    /// Timestamp of the latest `user`-type line in this scan range — "time
    /// in the current turn" for the main session, mirroring how a
    /// teammate's own turn-start is derived in `subagents::read_subagent_usage`.
    last_user_ts: Option<chrono::DateTime<chrono::Utc>>,
}

impl ScanState {
    fn new() -> Self {
        Self {
            last_tokens: 0,
            compactions: 0,
            recent: VecDeque::new(),
            tracker: AgentTracker::new(),
            model: String::new(),
            git_branch: String::new(),
            cwd: String::new(),
            last_user_ts: None,
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

        // Track the LATEST branch — the user can switch branches mid-session.
        if let Some(branch) = val.get("gitBranch").and_then(serde_json::Value::as_str) {
            if !branch.is_empty() {
                self.git_branch = branch.to_string();
            }
        }

        // First user line's cwd — mirrors the extraction in scan_session_file
        // so an incremental scan can also recover it if the initial full scan
        // ran before any user line existed yet (a freshly-created session).
        let is_user_line = val.get("type").and_then(|t| t.as_str()) == Some("user");
        if self.cwd.is_empty() && is_user_line {
            if let Some(cwd) = val.get("cwd").and_then(serde_json::Value::as_str) {
                if !cwd.is_empty() {
                    self.cwd = cwd.to_string();
                }
            }
        }

        // Latest genuine-new-turn timestamp — "time in the current turn".
        // Tool_result deliveries are excluded (see is_real_user_turn) so a
        // tool-heavy turn doesn't look like it keeps restarting.
        if is_real_user_turn(val) {
            self.last_user_ts = entry_timestamp(val).or(self.last_user_ts);
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
            git_branch: self.git_branch,
            cwd: self.cwd,
            last_user_ts: self.last_user_ts,
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

        // Extract the working directory from the first user line; the branch
        // is tracked continuously by the scan state (it can change).
        if cwd.is_none() && val.get("type").and_then(|t| t.as_str()) == Some("user") {
            cwd = val.get("cwd").and_then(|v| v.as_str()).map(String::from);
        }

        scan.process_parsed(&val);
    }

    let result = scan.into_result();
    Some(SessionFileData {
        cwd: cwd?,
        git_branch: result.git_branch,
        last_tokens: result.last_tokens,
        compactions: result.compactions,
        last_lines: result.last_lines,
        model: result.model,
        tracker: result.tracker,
        last_user_ts: result.last_user_ts,
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
    /// Latest `gitBranch` seen in the scanned range; empty if none.
    pub git_branch: String,
    /// `cwd` from the first `user`-type line in the scanned range; empty if
    /// none was seen (e.g. the session's first user line was already scanned
    /// in an earlier pass).
    pub cwd: String,
    /// Timestamp of the latest `user`-type line in the scanned range; `None`
    /// if none was seen.
    pub last_user_ts: Option<chrono::DateTime<chrono::Utc>>,
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
        assert!(status.last_idle_at.is_none());
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
        assert!(tracker.teammates()["fix-docrefs"].last_idle_at.is_some());
    }

    #[test]
    fn tracker_batched_idle_notifications_update_every_teammate() {
        // The lead's InboxPoller only delivers mail while idle, so several
        // teammates going idle while the lead is busy can land as one
        // relayed entry with multiple concatenated <teammate-message> blocks.
        let mut tracker = AgentTracker::new();
        tracker.process(&teammate_spawn_entry("angle1", "2026-07-13T11:19:00.000Z"));
        tracker.process(&teammate_spawn_entry("angle2", "2026-07-13T11:19:00.000Z"));
        tracker.process(&teammate_spawn_entry("angle3", "2026-07-13T11:19:00.000Z"));

        let batched_content = format!(
            "{}{}{}",
            "<teammate-message teammate_id=\"angle1@t\">{\"type\":\"idle_notification\",\"timestamp\":\"2026-07-13T11:20:00.000Z\"}</teammate-message>",
            "<teammate-message teammate_id=\"angle2@t\">{\"type\":\"idle_notification\",\"timestamp\":\"2026-07-13T11:20:01.000Z\"}</teammate-message>",
            "<teammate-message teammate_id=\"angle3@t\">{\"type\":\"idle_notification\",\"timestamp\":\"2026-07-13T11:20:02.000Z\"}</teammate-message>",
        );
        tracker.process(&serde_json::json!({
            "type": "user",
            "timestamp": "2026-07-13T11:28:00.000Z",
            "message": {"content": batched_content}
        }));

        assert!(tracker.teammates()["angle1"].last_idle_at.is_some());
        assert!(tracker.teammates()["angle2"].last_idle_at.is_some());
        assert!(tracker.teammates()["angle3"].last_idle_at.is_some());
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
        let status = &tracker.teammates()["fix-docrefs"];
        assert!(status.spawned_at > status.last_idle_at);
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
        assert!(tracker.teammates()["fix-docrefs"].last_idle_at.is_none());
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
        assert!(a.teammates()["fix-docrefs"].last_idle_at.is_some());
    }

    #[test]
    fn tracker_teammate_terminated_frame() {
        let val = serde_json::json!({
            "type": "user",
            "timestamp": "2026-07-09T02:10:00.000Z",
            "message": {
                "content": "<teammate-message teammate_id=\"system@team\">{\"type\":\"teammate_terminated\",\"message\":\"fix-docrefs has shut down. 0 task(s) were unassigned\"}</teammate-message>"
            }
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:48.994Z",
        ));
        tracker.process(&val);
        assert!(tracker.teammates()["fix-docrefs"].terminated);
    }

    // ── workflow launch events ──────────────────────────────────────

    #[test]
    fn tracker_workflow_launch_captures_name() {
        let val = serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "status": "async_launched",
                "taskType": "local_workflow",
                "taskId": "w1a2b3c4",
                "runId": "wf_8001925fe44a",
                "workflowName": "review-changes",
                "scriptPath": "/tmp/script.js",
                "transcriptDir": "/tmp",
                "summary": "Review changed files"
            },
            "message": {"content": [
                {"type": "tool_result", "tool_use_id": "toolu_wf", "content": "launched"}
            ]}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert_eq!(
            tracker.workflow_names().get("wf_8001925fe44a"),
            Some(&"review-changes".to_string())
        );
        assert_eq!(
            tracker.workflow_task_ids().get("wf_8001925fe44a"),
            Some(&"w1a2b3c4".to_string())
        );
    }

    #[test]
    fn tracker_workflow_launch_falls_back_to_summary() {
        let val = serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "status": "async_launched",
                "taskType": "local_workflow",
                "runId": "wf_noname",
                "summary": "Ad-hoc workflow run"
            },
            "message": {"content": []}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert_eq!(
            tracker.workflow_names().get("wf_noname"),
            Some(&"Ad-hoc workflow run".to_string())
        );
    }

    #[test]
    fn tracker_workflow_launch_ignores_other_task_types() {
        let val = serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "status": "async_launched",
                "taskType": "local_agent",
                "runId": "wf_not_a_workflow",
                "workflowName": "should-not-appear"
            },
            "message": {"content": []}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.workflow_names().is_empty());
    }

    #[test]
    fn tracker_workflow_launch_merge_keeps_first_seen_name() {
        let mut a = AgentTracker::new();
        a.process(&serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "status": "async_launched",
                "taskType": "local_workflow",
                "runId": "wf_shared",
                "workflowName": "first-seen"
            },
            "message": {"content": []}
        }));
        let mut b = AgentTracker::new();
        b.process(&serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "status": "async_launched",
                "taskType": "local_workflow",
                "runId": "wf_shared",
                "workflowName": "second-seen"
            },
            "message": {"content": []}
        }));
        a.merge(&b);
        assert_eq!(
            a.workflow_names().get("wf_shared"),
            Some(&"first-seen".to_string())
        );
    }

    #[test]
    fn tracker_workflow_launch_merge_keeps_first_seen_task_id() {
        let mut a = AgentTracker::new();
        a.process(&serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "status": "async_launched",
                "taskType": "local_workflow",
                "taskId": "first-task",
                "runId": "wf_shared",
                "workflowName": "shared"
            },
            "message": {"content": []}
        }));
        let mut b = AgentTracker::new();
        b.process(&serde_json::json!({
            "type": "user",
            "toolUseResult": {
                "status": "async_launched",
                "taskType": "local_workflow",
                "taskId": "second-task",
                "runId": "wf_shared",
                "workflowName": "shared"
            },
            "message": {"content": []}
        }));
        a.merge(&b);
        assert_eq!(
            a.workflow_task_ids().get("wf_shared"),
            Some(&"first-task".to_string())
        );
    }

    #[test]
    fn tracker_background_failed_status_is_terminal() {
        let val = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "<task-notification>\n<task-id>bg_abc</task-id>\n<status>failed</status>\n</task-notification>"
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.completed_background_ids.contains("bg_abc"));
    }

    #[test]
    fn tracker_background_killed_status_is_terminal() {
        let val = serde_json::json!({
            "type": "queue-operation",
            "operation": "enqueue",
            "content": "<task-notification>\n<task-id>bg_abc</task-id>\n<status>killed</status>\n</task-notification>"
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.completed_background_ids.contains("bg_abc"));
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
        // An id without a team suffix is used verbatim
        assert_eq!(
            teammate_name_from_message("<teammate-message teammate_id=\"solo\">"),
            Some("solo")
        );
    }

    #[test]
    fn tracker_teammate_idle_in_array_content() {
        let val = serde_json::json!({
            "type": "user",
            "timestamp": "2026-07-09T02:07:22.456Z",
            "message": {
                "content": [
                    {"type": "text", "text": "<teammate-message teammate_id=\"fix-docrefs@t\">{\"type\":\"idle_notification\",\"from\":\"fix-docrefs\"}</teammate-message>"}
                ]
            }
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:48.994Z",
        ));
        tracker.process(&val);
        assert!(tracker.teammates()["fix-docrefs"].last_idle_at.is_some());
    }

    #[test]
    fn tracker_idle_prefers_embedded_timestamp() {
        // Delivery entry is stamped minutes after the embedded idle time —
        // a respawn in between must win the ordering.
        let idle = serde_json::json!({
            "type": "user",
            "timestamp": "2026-07-09T02:10:00.000Z",
            "message": {
                "content": "<teammate-message teammate_id=\"fix-docrefs@t\">{\"type\":\"idle_notification\",\"timestamp\":\"2026-07-09T02:05:00.000Z\"}</teammate-message>"
            }
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:00.000Z",
        ));
        tracker.process(&idle);
        // Respawned after the embedded idle time but before delivery
        tracker.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:07:00.000Z",
        ));
        let status = &tracker.teammates()["fix-docrefs"];
        assert!(status.spawned_at > status.last_idle_at);
    }

    #[test]
    fn tracker_merge_propagates_terminated() {
        let mut a = AgentTracker::new();
        a.process(&teammate_spawn_entry(
            "fix-docrefs",
            "2026-07-09T02:00:48.994Z",
        ));
        let mut b = AgentTracker::new();
        b.process(&serde_json::json!({
            "type": "user",
            "timestamp": "2026-07-09T02:10:00.000Z",
            "message": {
                "content": "<teammate-message teammate_id=\"system@t\">{\"type\":\"teammate_terminated\",\"message\":\"fix-docrefs has shut down.\"}</teammate-message>"
            }
        }));
        a.merge(&b);
        assert!(a.teammates()["fix-docrefs"].terminated);
    }

    #[test]
    fn terminated_name_requires_shutdown_phrase() {
        assert_eq!(
            terminated_teammate_name("{\"message\":\"fix-docrefs has shut down. 0 tasks\"}"),
            Some("fix-docrefs".to_string())
        );
        assert_eq!(
            terminated_teammate_name("{\"message\":\"fix-docrefs said hello\"}"),
            None
        );
        assert_eq!(terminated_teammate_name("no message field"), None);
    }

    #[test]
    fn embedded_timestamp_rejects_garbage() {
        assert!(embedded_timestamp("{\"timestamp\":\"not-a-date\"}").is_none());
        assert!(embedded_timestamp("no timestamp").is_none());
        assert_eq!(
            embedded_timestamp("{\"timestamp\":\"2026-07-09T02:05:00.000Z\"}")
                .unwrap()
                .timestamp(),
            1_783_562_700
        );
    }

    #[test]
    fn is_real_user_turn_rejects_tool_result_only() {
        let val = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "done"}
            ]}
        });
        assert!(!is_real_user_turn(&val));
    }

    #[test]
    fn is_real_user_turn_rejects_multiple_tool_results() {
        let val = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "done"},
                {"type": "tool_result", "tool_use_id": "toolu_2", "content": "done"}
            ]}
        });
        assert!(!is_real_user_turn(&val));
    }

    #[test]
    fn is_real_user_turn_accepts_string_content() {
        let val = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "do the next thing"}
        });
        assert!(is_real_user_turn(&val));
    }

    #[test]
    fn is_real_user_turn_accepts_text_block_content() {
        let val = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": "do the next thing"}]}
        });
        assert!(is_real_user_turn(&val));
    }

    #[test]
    fn is_real_user_turn_rejects_non_user_type() {
        let val = serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "ok"}]}
        });
        assert!(!is_real_user_turn(&val));
    }

    #[test]
    fn is_real_user_turn_rejects_empty_content() {
        let val = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": []}
        });
        assert!(!is_real_user_turn(&val));
    }

    #[test]
    fn completed_tool_ids_exposed() {
        let val = serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_9"}]}
        });
        let mut tracker = AgentTracker::new();
        tracker.process(&val);
        assert!(tracker.completed_tool_ids().contains("toolu_9"));
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

    #[test]
    fn scan_from_offset_extracts_cwd_from_first_user_line() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"type":"system","subtype":"init"}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"user","cwd":"/home/user/proj","message":{{"content":"hi"}}}}"#
        )
        .unwrap();
        f.as_file().sync_all().unwrap();

        let result = scan_from_offset(f.as_file(), 0);
        assert_eq!(result.cwd, "/home/user/proj");
    }

    #[test]
    fn scan_from_offset_empty_cwd_when_no_user_line() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"type":"system","subtype":"init"}}"#).unwrap();
        f.as_file().sync_all().unwrap();

        let result = scan_from_offset(f.as_file(), 0);
        assert_eq!(result.cwd, "");
    }

    #[test]
    fn scan_from_offset_takes_latest_user_ts() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-07-13T20:00:00.000Z","message":{{"content":"first"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-07-13T20:00:05.000Z","message":{{"content":[{{"type":"text","text":"ok"}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-07-13T20:05:00.000Z","message":{{"content":"second"}}}}"#
        )
        .unwrap();
        f.as_file().sync_all().unwrap();

        let result = scan_from_offset(f.as_file(), 0);
        assert_eq!(
            result.last_user_ts.unwrap().timestamp(),
            chrono::DateTime::parse_from_rfc3339("2026-07-13T20:05:00.000Z")
                .unwrap()
                .timestamp()
        );
    }

    #[test]
    fn scan_from_offset_tool_result_does_not_reset_turn_start() {
        // A tool-heavy turn writes many tool_result deliveries (also
        // "type":"user") after the real prompt — none of them should look
        // like a newer turn start.
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-07-13T20:00:00.000Z","message":{{"content":"do the thing"}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","timestamp":"2026-07-13T20:00:05.000Z","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"ls"}}}}]}}}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"type":"user","timestamp":"2026-07-13T20:24:00.000Z","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}}]}}}}"#
        )
        .unwrap();
        f.as_file().sync_all().unwrap();

        let result = scan_from_offset(f.as_file(), 0);
        assert_eq!(
            result.last_user_ts.unwrap().timestamp(),
            chrono::DateTime::parse_from_rfc3339("2026-07-13T20:00:00.000Z")
                .unwrap()
                .timestamp(),
            "the tool_result 24 minutes later must not look like a new turn"
        );
    }

    #[test]
    fn scan_from_offset_no_user_ts_when_no_user_line() {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, r#"{{"type":"system","subtype":"init"}}"#).unwrap();
        f.as_file().sync_all().unwrap();

        let result = scan_from_offset(f.as_file(), 0);
        assert!(result.last_user_ts.is_none());
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
