#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

pub mod activity;
mod cache;
mod process;
pub mod subagents;
pub mod tail;
#[cfg(test)]
mod testutil;

pub use cache::SessionCache;

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
    pub context_window: u64,
    pub context_percent: u16,
    pub agents: Vec<SubagentData>,
    pub compactions: u32,
    pub last_activity_label: String,
    pub state: SessionState,
    pub activity: String,
}
