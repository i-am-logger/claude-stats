pub(crate) mod claude_version;
pub(crate) mod incidents;
pub(crate) mod profile;
pub(crate) mod sessions;
pub(crate) mod usage;

pub(super) const ANTHROPIC_BETA: &str = "oauth-2025-04-20";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HealthStatus {
    #[default]
    Ok,
    Slow,
    Error,
}
