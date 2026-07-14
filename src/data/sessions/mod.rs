#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

pub mod activity;
mod cache;
mod process;
pub mod subagents;
pub mod tail;
mod tasks;
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
    Fable,
    Mythos,
    Opus,
    Sonnet,
    Haiku,
    #[default]
    Unknown,
}

impl std::fmt::Display for ModelShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fable => write!(f, "fable"),
            Self::Mythos => write!(f, "mythos"),
            Self::Opus => write!(f, "opus"),
            Self::Sonnet => write!(f, "sonnet"),
            Self::Haiku => write!(f, "haiku"),
            Self::Unknown => write!(f, "?"),
        }
    }
}

/// A workflow run's declared phase and the agents observed in it so far.
///
/// `done` agents with `state == "done"` out of `total` agents assigned to
/// this phase's title. `total == 0` means the workflow hasn't dispatched any
/// agent into this phase yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseProgress {
    pub title: String,
    pub done: u32,
    pub total: u32,
    /// What the phase's currently-running agent is doing right now (its
    /// `lastToolSummary`, falling back to `lastToolName`), from the first
    /// not-yet-`done` `workflow_agent` entry assigned to this phase. `None`
    /// if every agent in the phase is done, or none has reported a tool yet.
    pub current_tool: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SubagentData {
    pub task: String,
    /// Teammate name from the `.meta.json` sidecar; `None` for anonymous
    /// Task-tool and workflow agents.
    pub name: Option<String>,
    pub model: ModelShort,
    pub context_tokens: u64,
    /// Seconds since the agent transcript's first entry.
    pub runtime_secs: Option<u64>,
    /// Seconds since the transcript was last written. Used to downgrade the
    /// displayed state of agents that stopped writing without a completion
    /// marker.
    pub last_write_age_secs: u64,
    /// For rows that aggregate a whole workflow run: `(done, total)` agent
    /// counts. `None` for individual agents.
    pub progress: Option<(u32, u32)>,
    /// Declared `meta.phases` breakdown for workflow runs, in declaration
    /// order. Empty for plain agents and for workflow runs that don't
    /// declare phases.
    pub phases: Vec<PhaseProgress>,
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
    /// Seconds since the session's current turn started (the latest `user`
    /// entry) — "time in the current turn", the same semantics already used
    /// for subagent/teammate rows. `None` when idle or unknown.
    pub turn_runtime_secs: Option<u64>,
}
