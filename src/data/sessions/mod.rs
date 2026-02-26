mod activity;
mod subagents;
mod tail;

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

pub(crate) fn scan_active_sessions() -> Vec<SessionData> {
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
        .into_iter()
        .filter_map(|(age, path)| parse_session(&path, age))
        .collect();

    sessions.sort_by(|a, b| a.title.cmp(&b.title));
    sessions
}

/// Parse a single session file into [`SessionData`].
///
/// The same `File` handle is shared (via `&File`) between `parse_session_metadata`,
/// `parse_tail_stats`, and `detect_state_and_activity`. Each callee seeks to its
/// required position before reading, so call order does not matter.
fn parse_session(path: &Path, age_secs: u64) -> Option<SessionData> {
    let file = fs::File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();

    let (cwd, git_branch) = tail::parse_session_metadata(&file)?;
    let title = Path::new(&cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&cwd)
        .to_string();

    let stats = tail::parse_tail_stats(&file, file_len);
    let context_tokens = stats.last_tokens;

    let session_id = path.file_stem()?.to_str()?;
    let subagents_dir = path.parent()?.join(session_id).join("subagents");
    let agents = subagents::scan_subagents(&subagents_dir);

    let (session_state, act) = activity::detect_state_and_activity(&file, file_len);
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
        compactions: stats.compactions,
        last_activity_label,
        state: session_state,
        activity: act,
    })
}
