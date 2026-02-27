#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

pub mod activity;
mod process;
pub mod subagents;
pub mod tail;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use tail::CONTEXT_WINDOW;

/// Sessions modified more than 60s ago are considered inactive and excluded
/// from the dashboard.
const MAX_AGE_SECS: u64 = 60;

/// How many bytes from the end of a session file to read when extracting
/// recent lines (64 KB). Used by `read_last_lines` and subagent scanning.
pub(crate) const RECENT_TAIL_BYTES: u64 = 64_000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    #[default]
    Idle,
    Thinking,
    Working,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ModelShort {
    Opus,
    Sonnet,
    Haiku,
    #[default]
    Unknown,
}

impl std::fmt::Display for ModelShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Opus => write!(f, "opus"),
            Self::Sonnet => write!(f, "sonnet"),
            Self::Haiku => write!(f, "haiku"),
            Self::Unknown => write!(f, "?"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubagentData {
    pub task: String,
    pub model: ModelShort,
    pub context_tokens: u64,
    /// Used for display only; active/completed filtering uses parent JSONL tracking.
    pub state: SessionState,
}

#[derive(Debug, Clone)]
pub struct SessionData {
    pub title: String,
    pub git_branch: String,
    pub context_tokens: u64,
    pub context_percent: u16,
    pub agents: Vec<SubagentData>,
    pub compactions: u32,
    pub last_activity_label: String,
    pub state: SessionState,
    pub activity: String,
}

/// Cached computed state for a session file. Stores only the derived values,
/// not the file content.
#[derive(Debug)]
struct CachedEntry {
    cwd: String,
    git_branch: String,
    last_tokens: u64,
    compactions: u32,
    last_lines: VecDeque<String>,
    bytes_read: u64,
    /// `agentId` → `parentToolUseID` mapping from `agent_progress` entries.
    agent_tool_map: HashMap<String, String>,
    /// Set of `tool_use_id` values that have received a `tool_result`.
    completed_tool_ids: HashSet<String>,
}

impl CachedEntry {
    fn from_scan(data: tail::SessionFileData, file_len: u64) -> Self {
        Self {
            cwd: data.cwd,
            git_branch: data.git_branch,
            last_tokens: data.last_tokens,
            compactions: data.compactions,
            last_lines: data.last_lines,
            bytes_read: file_len,
            agent_tool_map: data.agent_tool_map,
            completed_tool_ids: data.completed_tool_ids,
        }
    }
}

/// Data returned from `read_session_file` to `parse_session`. Deliberately
/// excludes agent tracking fields — those are merged directly into the
/// `CachedEntry` and queried via `update_agent_tracking`.
struct SessionFileResult {
    cwd: String,
    git_branch: String,
    last_tokens: u64,
    compactions: u32,
    last_lines: VecDeque<String>,
}

/// Compute the set of agent IDs that are still active (have `agent_progress`
/// entries but no corresponding `tool_result`).
fn active_agent_ids(map: &HashMap<String, String>, completed: &HashSet<String>) -> HashSet<String> {
    map.iter()
        .filter(|(_, tool_id)| !completed.contains(*tool_id))
        .map(|(agent_id, _)| agent_id.clone())
        .collect()
}

/// Caches session file state across polls. First encounter does a full read;
/// subsequent polls read only the new bytes appended since last scan.
#[derive(Debug, Default)]
pub struct SessionCache {
    entries: HashMap<PathBuf, CachedEntry>,
}

impl SessionCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Scan all active session files and return session data. Uses cached state
    /// for files seen before, full reads for new files.
    pub fn scan(&mut self) -> Vec<SessionData> {
        let claude_dir = match dirs::home_dir() {
            Some(h) => h.join(".claude").join("projects"),
            None => return Vec::new(),
        };

        if !claude_dir.is_dir() {
            return Vec::new();
        }

        let active_dirs = process::active_project_dirs();
        let now = SystemTime::now();
        let mut candidates: Vec<(u64, PathBuf)> = Vec::new();

        let Ok(project_dirs) = fs::read_dir(&claude_dir) else {
            return Vec::new();
        };

        for project_entry in project_dirs.flatten() {
            let project_path = project_entry.path();
            if !project_path.is_dir() {
                continue;
            }

            let has_process =
                process::AVAILABLE && active_dirs.contains(&project_entry.file_name());

            // When process detection is available, skip projects without a
            // running claude process.
            if process::AVAILABLE && !has_process {
                continue;
            }

            let Ok(entries) = fs::read_dir(&project_path) else {
                continue;
            };

            // Track the most recently modified session file for this project.
            // When process detection is active, only the newest file is the
            // live session — older files are past conversations.
            let mut newest: Option<(u64, PathBuf)> = None;

            for entry in entries.flatten() {
                let path = entry.path();

                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }

                let Ok(metadata) = fs::metadata(&path) else {
                    continue;
                };
                let Ok(modified) = metadata.modified() else {
                    continue;
                };
                let age = now
                    .duration_since(modified)
                    .unwrap_or(Duration::MAX)
                    .as_secs();

                if has_process {
                    // Process alive: keep only the most recently modified file.
                    if newest.as_ref().is_none_or(|(prev_age, _)| age < *prev_age) {
                        newest = Some((age, path));
                    }
                } else if age < MAX_AGE_SECS {
                    // No process / non-Linux fallback: mtime filter.
                    candidates.push((age, path));
                }
            }

            if let Some(entry) = newest {
                candidates.push(entry);
            }
        }

        let mut sessions: Vec<SessionData> = candidates
            .iter()
            .filter_map(|(age, path)| self.parse_session(path, *age))
            .collect();

        // Prune cache entries for sessions no longer active
        let active: HashSet<&PathBuf> = candidates.iter().map(|(_, p)| p).collect();
        self.entries.retain(|k, _| active.contains(k));

        sessions.sort_by(|a, b| a.title.cmp(&b.title));
        sessions
    }

    /// Full-scan the file, cache the result, and return the session data.
    fn full_scan_and_cache(
        &mut self,
        path: &Path,
        file: &fs::File,
        file_len: u64,
    ) -> Option<SessionFileResult> {
        let data = tail::scan_session_file(file)?;
        self.entries
            .insert(path.to_path_buf(), CachedEntry::from_scan(data, file_len));
        Some(Self::result_from_cache(self.entries.get(path)?))
    }

    /// Build a `SessionFileResult` from the cached entry (no I/O).
    fn result_from_cache(cached: &CachedEntry) -> SessionFileResult {
        SessionFileResult {
            cwd: cached.cwd.clone(),
            git_branch: cached.git_branch.clone(),
            last_tokens: cached.last_tokens,
            compactions: cached.compactions,
            last_lines: cached.last_lines.clone(),
        }
    }

    /// Read session file data using cache. Returns parsed data or `None` if
    /// the file can't be parsed.
    fn read_session_file(
        &mut self,
        path: &Path,
        file: &fs::File,
        file_len: u64,
    ) -> Option<SessionFileResult> {
        if let Some(cached) = self.entries.get_mut(path) {
            match file_len.cmp(&cached.bytes_read) {
                std::cmp::Ordering::Less => {
                    self.entries.remove(path);
                    self.full_scan_and_cache(path, file, file_len)
                }
                std::cmp::Ordering::Greater => {
                    let result = tail::scan_from_offset(file, cached.bytes_read);
                    cached.compactions += result.compactions;
                    if result.last_tokens > 0 || result.compactions > 0 {
                        cached.last_tokens = result.last_tokens;
                    }
                    cached.last_lines.extend(result.last_lines);
                    while cached.last_lines.len() > tail::RECENT_LINES {
                        cached.last_lines.pop_front();
                    }
                    cached.bytes_read = file_len;
                    for (agent_id, tool_id) in result.agent_tool_map {
                        cached.agent_tool_map.entry(agent_id).or_insert(tool_id);
                    }
                    cached.completed_tool_ids.extend(result.completed_tool_ids);
                    Some(Self::result_from_cache(cached))
                }
                std::cmp::Ordering::Equal => Some(Self::result_from_cache(cached)),
            }
        } else {
            self.full_scan_and_cache(path, file, file_len)
        }
    }

    /// Parse a single session file, using cached state when available.
    fn parse_session(&mut self, path: &Path, age_secs: u64) -> Option<SessionData> {
        let file = fs::File::open(path).ok()?;
        let file_len = file.metadata().ok()?.len();

        let sfr = self.read_session_file(path, &file, file_len)?;

        let title = Path::new(&sfr.cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&sfr.cwd)
            .to_string();

        let (session_state, act) = activity::detect_state_and_activity(&sfr.last_lines);

        // Only scan subagents when the parent session is actively working.
        // Determine which agents are truly active by checking the parent
        // JSONL for tool_result completion entries — purely deterministic.
        let session_id = path.file_stem()?.to_str()?;
        let subagents_dir = path.parent()?.join(session_id).join("subagents");
        let agents = if session_state == SessionState::Working {
            let active_ids = self.update_agent_tracking(path);
            subagents::scan_subagents(&subagents_dir, &active_ids)
        } else {
            Vec::new()
        };
        let context_percent = ((sfr.last_tokens.min(CONTEXT_WINDOW) * 100) / CONTEXT_WINDOW) as u16;

        let last_activity_label = format!(
            "{} ago",
            crate::fmt::format_duration(age_secs.cast_signed())
        );

        Some(SessionData {
            title,
            git_branch: sfr.git_branch,
            context_tokens: sfr.last_tokens,
            context_percent,
            agents,
            compactions: sfr.compactions,
            last_activity_label,
            state: session_state,
            activity: act,
        })
    }

    /// Returns the set of agent IDs that are still active based on cached data.
    /// Agent tracking data is now collected during `read_session_file` scans,
    /// so this method performs no file I/O.
    fn update_agent_tracking(&self, path: &Path) -> HashSet<String> {
        let Some(cached) = self.entries.get(path) else {
            return HashSet::new();
        };
        active_agent_ids(&cached.agent_tool_map, &cached.completed_tool_ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    /// Write lines to a temp file and return the `File` handle (rewound to start).
    fn jsonl_file(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f.as_file().sync_all().unwrap();
        f
    }

    // ── scan_session_file (full read) ─────────────────────────────

    fn assistant_usage_line(input_tokens: u64) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{input_tokens}}},"content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn"}}}}"#
        )
    }

    #[test]
    fn full_scan_extracts_metadata_and_tokens() {
        let user_line = r#"{"type":"user","cwd":"/home/user/project","gitBranch":"main","message":{"content":"hi"}}"#;
        let usage = assistant_usage_line(5000);
        let f = jsonl_file(&[user_line, &usage]);
        let data = tail::scan_session_file(f.as_file()).unwrap();
        assert_eq!(data.cwd, "/home/user/project");
        assert_eq!(data.git_branch, "main");
        assert_eq!(data.last_tokens, 5000);
        assert_eq!(data.compactions, 0);
    }

    #[test]
    fn full_scan_missing_branch_defaults_empty() {
        let f = jsonl_file(&[r#"{"type":"user","cwd":"/tmp/test","message":{"content":"go"}}"#]);
        let data = tail::scan_session_file(f.as_file()).unwrap();
        assert_eq!(data.cwd, "/tmp/test");
        assert_eq!(data.git_branch, "");
    }

    #[test]
    fn full_scan_returns_none_without_user_line() {
        let f = jsonl_file(&[
            r#"{"type":"system","subtype":"init"}"#,
            &assistant_usage_line(1000),
        ]);
        assert!(tail::scan_session_file(f.as_file()).is_none());
    }

    #[test]
    fn full_scan_returns_none_for_empty_file() {
        let f = jsonl_file(&[]);
        assert!(tail::scan_session_file(f.as_file()).is_none());
    }

    #[test]
    fn full_scan_counts_compactions() {
        let user_line = r#"{"type":"user","cwd":"/tmp","message":{"content":"go"}}"#;
        let compact = r#"{"type":"system","subtype":"compact_boundary"}"#;
        let usage = assistant_usage_line(500);
        let f = jsonl_file(&[user_line, &usage, compact, &usage, compact, &usage]);
        let data = tail::scan_session_file(f.as_file()).unwrap();
        assert_eq!(data.compactions, 2);
        assert_eq!(data.last_tokens, 500);
    }

    #[test]
    fn full_scan_resets_tokens_on_compaction() {
        let user_line = r#"{"type":"user","cwd":"/tmp","message":{"content":"go"}}"#;
        let compact = r#"{"type":"system","subtype":"compact_boundary"}"#;
        let usage1 = assistant_usage_line(10_000);
        let usage2 = assistant_usage_line(3000);
        let f = jsonl_file(&[user_line, &usage1, compact, &usage2]);
        let data = tail::scan_session_file(f.as_file()).unwrap();
        assert_eq!(data.last_tokens, 3000);
        assert_eq!(data.compactions, 1);
    }

    #[test]
    fn full_scan_collects_last_lines() {
        let user_line = r#"{"type":"user","cwd":"/tmp","message":{"content":"go"}}"#;
        let lines: Vec<String> = (1..=8)
            .map(|i| format!(r#"{{"type":"system","n":{i}}}"#))
            .collect();
        let mut all: Vec<&str> = vec![user_line];
        all.extend(lines.iter().map(String::as_str));
        let f = jsonl_file(&all);
        let data = tail::scan_session_file(f.as_file()).unwrap();
        assert_eq!(data.last_lines.len(), 5);
        assert!(data.last_lines[0].contains("\"n\":4"));
        assert!(data.last_lines[4].contains("\"n\":8"));
    }

    // ── scan_from_offset (incremental) ────────────────────────────

    #[test]
    fn incremental_scan_reads_new_bytes() {
        let usage = assistant_usage_line(7000);
        let f = jsonl_file(&[&usage]);
        let result = tail::scan_from_offset(f.as_file(), 0);
        assert_eq!(result.last_tokens, 7000);
    }

    #[test]
    fn incremental_scan_from_midfile() {
        let line1 = assistant_usage_line(1000);
        let line2 = assistant_usage_line(9000);
        let f = jsonl_file(&[&line1, &line2]);
        // Offset past the first line
        let offset = (line1.len() + 1) as u64; // +1 for newline
        let result = tail::scan_from_offset(f.as_file(), offset);
        assert_eq!(result.last_tokens, 9000);
        assert_eq!(result.compactions, 0);
    }

    #[test]
    fn incremental_scan_detects_compaction() {
        let compact = r#"{"type":"system","subtype":"compact_boundary"}"#;
        let usage = assistant_usage_line(2000);
        let f = jsonl_file(&[compact, &usage]);
        let result = tail::scan_from_offset(f.as_file(), 0);
        assert_eq!(result.compactions, 1);
        assert_eq!(result.last_tokens, 2000);
    }

    // ── SessionCache integration ──────────────────────────────────

    #[test]
    fn cache_full_read_then_incremental() {
        let user_line = r#"{"type":"user","cwd":"/home/user/proj","gitBranch":"main","message":{"content":"go"}}"#;
        let usage1 = assistant_usage_line(50_000);
        let idle = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#;

        let mut f = jsonl_file(&[user_line, &usage1, idle]);
        let mut cache = SessionCache::new();

        // First parse: full read
        let session = cache.parse_session(f.path(), 5).unwrap();
        assert_eq!(session.context_tokens, 50_000);
        assert_eq!(session.compactions, 0);
        assert!(cache.entries.contains_key(f.path()));

        // Append new data
        let usage2 = assistant_usage_line(80_000);
        writeln!(f, "{usage2}").unwrap();
        f.as_file().sync_all().unwrap();

        // Second parse: incremental
        let session = cache.parse_session(f.path(), 2).unwrap();
        assert_eq!(session.context_tokens, 80_000);
        assert_eq!(session.compactions, 0);
    }

    #[test]
    fn cache_incremental_with_compaction() {
        let user_line =
            r#"{"type":"user","cwd":"/tmp/proj","gitBranch":"","message":{"content":"go"}}"#;
        let usage1 = assistant_usage_line(100_000);

        let mut f = jsonl_file(&[user_line, &usage1]);
        let mut cache = SessionCache::new();

        // First parse
        let session = cache.parse_session(f.path(), 1).unwrap();
        assert_eq!(session.context_tokens, 100_000);

        // Append compaction + new usage
        let compact = r#"{"type":"system","subtype":"compact_boundary"}"#;
        let usage2 = assistant_usage_line(15_000);
        writeln!(f, "{compact}").unwrap();
        writeln!(f, "{usage2}").unwrap();
        f.as_file().sync_all().unwrap();

        // Incremental: should see compaction and new tokens
        let session = cache.parse_session(f.path(), 1).unwrap();
        assert_eq!(session.compactions, 1);
        assert_eq!(session.context_tokens, 15_000);
    }

    #[test]
    fn cache_unchanged_file_reuses_stats() {
        let user_line =
            r#"{"type":"user","cwd":"/tmp/proj","gitBranch":"dev","message":{"content":"go"}}"#;
        let usage = assistant_usage_line(42_000);
        let idle = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#;

        let f = jsonl_file(&[user_line, &usage, idle]);
        let mut cache = SessionCache::new();

        // First parse
        let s1 = cache.parse_session(f.path(), 3).unwrap();
        // Second parse — file unchanged
        let s2 = cache.parse_session(f.path(), 4).unwrap();

        assert_eq!(s1.context_tokens, s2.context_tokens);
        assert_eq!(s1.compactions, s2.compactions);
        assert_eq!(s2.state, SessionState::Idle);
    }

    // ── parse_session via cache (integration) ─────────────────────

    #[test]
    fn parse_session_full_pipeline() {
        let user_line = r#"{"type":"user","cwd":"/home/user/my-project","gitBranch":"feat","message":{"content":"hello"}}"#;
        let usage = assistant_usage_line(50_000);
        let idle_line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"#;

        let f = jsonl_file(&[user_line, &usage, idle_line]);
        let mut cache = SessionCache::new();
        let session = cache.parse_session(f.path(), 5).unwrap();

        assert_eq!(session.title, "my-project");
        assert_eq!(session.git_branch, "feat");
        assert_eq!(session.context_tokens, 50_000);
        assert_eq!(session.context_percent, 30); // 50000/166000 ≈ 30%
        assert_eq!(session.compactions, 0);
        assert_eq!(session.state, SessionState::Idle);
        assert!(session.last_activity_label.contains("ago"));
    }

    #[test]
    fn parse_session_working_state() {
        let user_line =
            r#"{"type":"user","cwd":"/home/user/proj","gitBranch":"","message":{"content":"go"}}"#;
        let usage = assistant_usage_line(10_000);
        let tool_use = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],"stop_reason":null}}"#;
        let progress = r#"{"type":"progress","data":{"type":"bash_progress","output":"file.rs"}}"#;

        let f = jsonl_file(&[user_line, &usage, tool_use, progress]);
        let mut cache = SessionCache::new();
        let session = cache.parse_session(f.path(), 2).unwrap();

        assert_eq!(session.state, SessionState::Working);
        assert_eq!(session.activity, "Bash(ls)");
    }

    #[test]
    fn parse_session_thinking_state() {
        let user_line = r#"{"type":"user","cwd":"/tmp/x","message":{"content":"think"}}"#;
        let thinking = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hmm..."}],"stop_reason":null}}"#;

        let f = jsonl_file(&[user_line, thinking]);
        let mut cache = SessionCache::new();
        let session = cache.parse_session(f.path(), 1).unwrap();

        assert_eq!(session.state, SessionState::Thinking);
        assert_eq!(session.activity, "thinking...");
    }

    #[test]
    fn parse_session_returns_none_without_user_line() {
        let f = jsonl_file(&[
            r#"{"type":"system","subtype":"init"}"#,
            &assistant_usage_line(1000),
        ]);
        let mut cache = SessionCache::new();
        assert!(cache.parse_session(f.path(), 0).is_none());
    }

    #[test]
    fn parse_session_with_compaction() {
        let user_line = r#"{"type":"user","cwd":"/home/user/proj","gitBranch":"main","message":{"content":"go"}}"#;
        let usage1 = assistant_usage_line(100_000);
        let compact = r#"{"type":"system","subtype":"compact_boundary"}"#;
        let usage2 = assistant_usage_line(20_000);
        let idle = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#;

        let f = jsonl_file(&[user_line, &usage1, compact, &usage2, idle]);
        let mut cache = SessionCache::new();
        let session = cache.parse_session(f.path(), 3).unwrap();

        assert_eq!(session.compactions, 1);
        assert_eq!(session.context_tokens, 20_000);
        assert_eq!(session.state, SessionState::Idle);
    }

    #[test]
    fn cache_handles_file_shrink() {
        let user_line =
            r#"{"type":"user","cwd":"/tmp/proj","gitBranch":"main","message":{"content":"go"}}"#;
        let usage1 = assistant_usage_line(80_000);

        let f = jsonl_file(&[user_line, &usage1]);
        let mut cache = SessionCache::new();

        // First parse: full read
        let session = cache.parse_session(f.path(), 1).unwrap();
        assert_eq!(session.context_tokens, 80_000);

        // Truncate and rewrite with different content
        f.as_file().set_len(0).unwrap();
        f.as_file().seek(SeekFrom::Start(0)).unwrap();
        let usage2 = assistant_usage_line(5_000);
        writeln!(&f, "{user_line}").unwrap();
        writeln!(&f, "{usage2}").unwrap();
        f.as_file().sync_all().unwrap();

        // Should detect shrink and do full re-read
        let session = cache.parse_session(f.path(), 1).unwrap();
        assert_eq!(session.context_tokens, 5_000);
        assert_eq!(session.compactions, 0);
    }

    // ── active agent tracking ─────────────────────────────────

    fn progress_line(agent_id: &str, tool_use_id: &str) -> String {
        format!(
            r#"{{"type":"progress","parentToolUseID":"{tool_use_id}","data":{{"type":"agent_progress","agentId":"{agent_id}"}}}}"#
        )
    }

    fn tool_result_line(tool_use_id: &str) -> String {
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"{tool_use_id}","content":"done"}}]}}}}"#
        )
    }

    /// Helper: scan a temp file from offset 0 and return the set of active
    /// agent IDs using `scan_from_offset` + `active_agent_ids`.
    fn active_ids_from(f: &tempfile::NamedTempFile) -> HashSet<String> {
        let result = tail::scan_from_offset(f.as_file(), 0);
        active_agent_ids(&result.agent_tool_map, &result.completed_tool_ids)
    }

    #[test]
    fn active_agents_no_entries() {
        let f = jsonl_file(&[r#"{"type":"user","cwd":"/tmp","message":{"content":"hi"}}"#]);
        assert!(active_ids_from(&f).is_empty());
    }

    #[test]
    fn active_agents_one_running() {
        let progress = progress_line("abc123", "toolu_001");
        let f = jsonl_file(&[&progress]);
        let ids = active_ids_from(&f);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("abc123"));
    }

    #[test]
    fn active_agents_one_completed() {
        let progress = progress_line("abc123", "toolu_001");
        let result = tool_result_line("toolu_001");
        let f = jsonl_file(&[&progress, &result]);
        assert!(active_ids_from(&f).is_empty());
    }

    #[test]
    fn active_agents_mixed() {
        let p1 = progress_line("agent_a", "toolu_001");
        let p2 = progress_line("agent_b", "toolu_002");
        let r1 = tool_result_line("toolu_001");
        let f = jsonl_file(&[&p1, &p2, &r1]);
        let ids = active_ids_from(&f);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("agent_b"));
        assert!(!ids.contains("agent_a"));
    }

    #[test]
    fn active_agents_multiple_progress_same_agent() {
        let p1 = progress_line("agent_a", "toolu_001");
        let p2 = progress_line("agent_a", "toolu_001");
        let f = jsonl_file(&[&p1, &p2]);
        let ids = active_ids_from(&f);
        assert_eq!(ids.len(), 1);
        assert!(ids.contains("agent_a"));
    }

    #[test]
    fn active_agents_all_completed() {
        let p1 = progress_line("agent_a", "toolu_001");
        let p2 = progress_line("agent_b", "toolu_002");
        let r1 = tool_result_line("toolu_001");
        let r2 = tool_result_line("toolu_002");
        let f = jsonl_file(&[&p1, &p2, &r1, &r2]);
        assert!(active_ids_from(&f).is_empty());
    }

    #[test]
    fn active_agents_incremental_scan() {
        let user_line =
            r#"{"type":"user","cwd":"/tmp/proj","gitBranch":"main","message":{"content":"go"}}"#;
        let usage = assistant_usage_line(10_000);
        let p1 = progress_line("agent_a", "toolu_001");
        let p2 = progress_line("agent_b", "toolu_002");
        let tool_use = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],"stop_reason":null}}"#;

        let f = jsonl_file(&[user_line, &usage, &p1, &p2, tool_use]);
        let mut cache = SessionCache::new();

        // First parse (full scan): both agents tracked in cache
        let _session = cache.parse_session(f.path(), 1).unwrap();
        let cached = cache.entries.get(f.path()).unwrap();
        assert_eq!(cached.agent_tool_map.len(), 2);
        assert!(cached.completed_tool_ids.is_empty());

        // Append a tool_result for agent_a
        let r1 = tool_result_line("toolu_001");
        writeln!(&f, "{r1}").unwrap();
        f.as_file().sync_all().unwrap();

        // Second parse (incremental): cache merges the new tool_result
        let _session = cache.parse_session(f.path(), 1).unwrap();
        let cached = cache.entries.get(f.path()).unwrap();
        assert_eq!(cached.agent_tool_map.len(), 2);
        assert!(cached.completed_tool_ids.contains("toolu_001"));

        let active = active_agent_ids(&cached.agent_tool_map, &cached.completed_tool_ids);
        assert_eq!(active.len(), 1);
        assert!(active.contains("agent_b"));
    }

    // ── edge case: malformed agent_progress ──────────────────────

    #[test]
    fn agent_tracking_malformed_progress() {
        // Missing agentId field
        let missing_agent_id =
            r#"{"type":"progress","parentToolUseID":"toolu_001","data":{"type":"agent_progress"}}"#;
        // Missing parentToolUseID field
        let missing_tool_id =
            r#"{"type":"progress","data":{"type":"agent_progress","agentId":"abc123"}}"#;
        let f = jsonl_file(&[missing_agent_id, missing_tool_id]);
        let ids = active_ids_from(&f);
        assert!(ids.is_empty());
    }

    #[test]
    fn agent_tracking_tool_result_before_progress() {
        // tool_result appears before any agent_progress — should be harmless
        let result = tool_result_line("toolu_001");
        let progress = progress_line("agent_a", "toolu_001");
        let f = jsonl_file(&[&result, &progress]);
        let ids = active_ids_from(&f);
        // The tool_result was recorded before the progress, so the agent
        // should still be marked as completed.
        assert!(ids.is_empty());
    }

    #[test]
    fn cache_file_shrink_resets_agent_data() {
        let user_line =
            r#"{"type":"user","cwd":"/tmp/proj","gitBranch":"main","message":{"content":"go"}}"#;
        let progress = progress_line("agent_x", "toolu_099");
        let usage = assistant_usage_line(10_000);
        let tool_use = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}],"stop_reason":null}}"#;

        let f = jsonl_file(&[user_line, &progress, &usage, tool_use]);
        let mut cache = SessionCache::new();

        // First parse — agent_x should be in the cache
        let _session = cache.parse_session(f.path(), 1).unwrap();
        let cached = cache.entries.get(f.path()).unwrap();
        assert!(cached.agent_tool_map.contains_key("agent_x"));

        // Truncate and rewrite without agent data
        f.as_file().set_len(0).unwrap();
        f.as_file().seek(SeekFrom::Start(0)).unwrap();
        let idle = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn"}}"#;
        writeln!(&f, "{user_line}").unwrap();
        writeln!(&f, "{}", &assistant_usage_line(5_000)).unwrap();
        writeln!(&f, "{idle}").unwrap();
        f.as_file().sync_all().unwrap();

        // After shrink, agent maps should be rebuilt (empty)
        let _session = cache.parse_session(f.path(), 1).unwrap();
        let cached = cache.entries.get(f.path()).unwrap();
        assert!(cached.agent_tool_map.is_empty());
        assert!(cached.completed_tool_ids.is_empty());
    }

    // ── integration: parse_session with subagents ────────────────

    #[test]
    fn parse_session_with_subagents() {
        let dir = tempfile::tempdir().unwrap();

        // Create parent session file
        let session_id = "test-session-abc";
        let parent_path = dir.path().join(format!("{session_id}.jsonl"));

        let user_line = r#"{"type":"user","cwd":"/home/user/proj","gitBranch":"main","message":{"content":"go"}}"#;
        let usage = assistant_usage_line(10_000);
        let tool_use_a = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_001","name":"Task","input":{}}],"stop_reason":null}}"#;
        let progress_a = progress_line("agent_active", "toolu_001");
        let tool_use_b = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_002","name":"Task","input":{}}],"stop_reason":null}}"#;
        let progress_b = progress_line("agent_done", "toolu_002");
        let result_b = tool_result_line("toolu_002");
        // End with working state (tool_use + progress)
        let final_progress =
            r#"{"type":"progress","data":{"type":"agent_progress","agentId":"agent_active"}}"#;

        {
            let mut f = fs::File::create(&parent_path).unwrap();
            for line in &[
                user_line,
                &usage,
                tool_use_a,
                &progress_a,
                tool_use_b,
                &progress_b,
                &result_b,
                final_progress,
            ] {
                writeln!(f, "{line}").unwrap();
            }
            f.sync_all().unwrap();
        }

        // Create subagent files
        let subagents_dir = dir.path().join(session_id).join("subagents");
        fs::create_dir_all(&subagents_dir).unwrap();

        write_agent_file(&subagents_dir, "agent_active", "Search for tests", 5_000);
        write_agent_file(&subagents_dir, "agent_done", "Run linter", 3_000);

        // Parse session
        let mut cache = SessionCache::new();
        let session = cache.parse_session(&parent_path, 1).unwrap();

        // Working state → agents should be scanned
        assert_eq!(session.state, SessionState::Working);
        // Only the active agent (not the completed one) should appear
        assert_eq!(session.agents.len(), 1);
        assert_eq!(session.agents[0].task, "Search for tests");
    }

    /// Write a minimal subagent JSONL file.
    fn write_agent_file(subagents_dir: &Path, agent_id: &str, task: &str, tokens: u64) {
        let path = subagents_dir.join(format!("agent-{agent_id}.jsonl"));
        let user_line = format!(r#"{{"type":"user","message":{{"content":"{task}"}}}}"#);
        let usage_line = format!(
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":{tokens}}},"content":[{{"type":"text","text":"ok"}}],"stop_reason":"end_turn"}}}}"#
        );
        let mut f = fs::File::create(path).unwrap();
        writeln!(f, "{user_line}").unwrap();
        writeln!(f, "{usage_line}").unwrap();
        f.sync_all().unwrap();
    }
}
