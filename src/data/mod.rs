#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

pub mod claude_version;
pub mod incidents;
pub mod profile;
pub mod sessions;
pub mod usage;

pub(super) const ANTHROPIC_BETA: &str = "oauth-2025-04-20";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum HealthStatus {
    #[default]
    Ok,
    Slow,
    Error,
}
