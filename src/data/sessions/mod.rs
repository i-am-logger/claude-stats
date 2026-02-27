mod activity;
mod subagents;
mod tail;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) use tail::CONTEXT_WINDOW;

/// Sessions modified more than 60s ago are considered inactive and excluded
/// from the dashboard.
const MAX_AGE_SECS: u64 = 60;

/// How many bytes from the end of a session file to read when extracting
/// recent lines (64 KB). Shared by `read_last_lines` and subagent scanning.
const RECENT_TAIL_BYTES: u64 = 64_000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionState {
    #[default]
    Idle,
    Thinking,
    Working,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelShort {
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
pub(crate) struct SubagentData {
    pub(crate) task: String,
    pub(crate) model: ModelShort,
    pub(crate) context_tokens: u64,
    pub(crate) state: SessionState,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionData {
    pub(crate) title: String,
    pub(crate) git_branch: String,
    pub(crate) context_tokens: u64,
    pub(crate) context_percent: u16,
    pub(crate) agents: Vec<SubagentData>,
    pub(crate) compactions: u32,
    pub(crate) last_activity_label: String,
    pub(crate) state: SessionState,
    pub(crate) activity: String,
}

/// Cached computed state for a session file. Stores only the derived values,
/// not the file content.
struct CachedEntry {
    cwd: String,
    git_branch: String,
    last_tokens: u64,
    compactions: u32,
    bytes_read: u64,
}

/// Caches session file state across polls. First encounter does a full read;
/// subsequent polls read only the new bytes appended since last scan.
pub(crate) struct SessionCache {
    entries: HashMap<PathBuf, CachedEntry>,
}

impl SessionCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Scan all active session files and return session data. Uses cached state
    /// for files seen before, full reads for new files.
    pub(crate) fn scan(&mut self) -> Vec<SessionData> {
        let claude_dir = match dirs::home_dir() {
            Some(h) => h.join(".claude").join("projects"),
            None => return Vec::new(),
        };

        if !claude_dir.is_dir() {
            return Vec::new();
        }

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

            let Ok(entries) = fs::read_dir(&project_path) else {
                continue;
            };

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

                if age < MAX_AGE_SECS {
                    candidates.push((age, path));
                }
            }
        }

        let mut sessions: Vec<SessionData> = candidates
            .iter()
            .filter_map(|(age, path)| self.parse_session(path, *age))
            .collect();

        // Prune cache entries for sessions no longer active
        let active: std::collections::HashSet<&PathBuf> =
            candidates.iter().map(|(_, p)| p).collect();
        self.entries.retain(|k, _| active.contains(k));

        sessions.sort_by(|a, b| a.title.cmp(&b.title));
        sessions
    }

    /// Parse a single session file, using cached state when available.
    fn parse_session(&mut self, path: &Path, age_secs: u64) -> Option<SessionData> {
        let file = fs::File::open(path).ok()?;
        let file_len = file.metadata().ok()?.len();

        let (cwd, git_branch, context_tokens, compactions, last_lines) =
            if let Some(cached) = self.entries.get_mut(path) {
                match file_len.cmp(&cached.bytes_read) {
                    std::cmp::Ordering::Less => {
                        // File shrank (truncated/recreated): discard cache, full re-read
                        self.entries.remove(path);
                        let data = tail::scan_session_file(&file)?;
                        let result = (
                            data.cwd.clone(),
                            data.git_branch.clone(),
                            data.last_tokens,
                            data.compactions,
                            data.last_lines,
                        );
                        self.entries.insert(
                            path.to_path_buf(),
                            CachedEntry {
                                cwd: data.cwd,
                                git_branch: data.git_branch,
                                last_tokens: data.last_tokens,
                                compactions: data.compactions,
                                bytes_read: file_len,
                            },
                        );
                        result
                    }
                    std::cmp::Ordering::Greater => {
                        // File grew: scan only the new bytes
                        let result = tail::scan_from_offset(&file, cached.bytes_read);
                        cached.compactions += result.compactions;
                        if result.last_tokens > 0 || result.compactions > 0 {
                            cached.last_tokens = result.last_tokens;
                        }
                        cached.bytes_read = file_len;
                        (
                            cached.cwd.clone(),
                            cached.git_branch.clone(),
                            cached.last_tokens,
                            cached.compactions,
                            result.last_lines,
                        )
                    }
                    std::cmp::Ordering::Equal => {
                        // File unchanged: reuse cached stats, read last lines for state
                        let last_lines = tail::read_last_lines(&file, file_len, 5);
                        (
                            cached.cwd.clone(),
                            cached.git_branch.clone(),
                            cached.last_tokens,
                            cached.compactions,
                            last_lines,
                        )
                    }
                }
            } else {
                // First encounter: full read
                let data = tail::scan_session_file(&file)?;
                let path_buf = path.to_path_buf();
                let result = (
                    data.cwd.clone(),
                    data.git_branch.clone(),
                    data.last_tokens,
                    data.compactions,
                    data.last_lines,
                );
                self.entries.insert(
                    path_buf,
                    CachedEntry {
                        cwd: data.cwd,
                        git_branch: data.git_branch,
                        last_tokens: data.last_tokens,
                        compactions: data.compactions,
                        bytes_read: file_len,
                    },
                );
                result
            };

        let title = Path::new(&cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&cwd)
            .to_string();

        let session_id = path.file_stem()?.to_str()?;
        let subagents_dir = path.parent()?.join(session_id).join("subagents");
        let agents = subagents::scan_subagents(&subagents_dir);

        let (session_state, act) = activity::detect_state_and_activity(&last_lines);
        let context_percent = ((context_tokens.min(CONTEXT_WINDOW) * 100) / CONTEXT_WINDOW) as u16;

        let last_activity_label = format!(
            "{} ago",
            crate::fmt::format_duration(age_secs.cast_signed())
        );

        Some(SessionData {
            title,
            git_branch,
            context_tokens,
            context_percent,
            agents,
            compactions,
            last_activity_label,
            state: session_state,
            activity: act,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

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
        f.as_file().seek(std::io::SeekFrom::Start(0)).unwrap();
        let usage2 = assistant_usage_line(5_000);
        writeln!(&f, "{user_line}").unwrap();
        writeln!(&f, "{usage2}").unwrap();
        f.as_file().sync_all().unwrap();

        // Should detect shrink and do full re-read
        let session = cache.parse_session(f.path(), 1).unwrap();
        assert_eq!(session.context_tokens, 5_000);
        assert_eq!(session.compactions, 0);
    }
}
