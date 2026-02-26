pub(crate) mod incidents;
pub(crate) mod sessions;
pub(crate) mod usage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthStatus {
    Ok,
    Slow,
    Error,
}
