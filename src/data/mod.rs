pub mod incidents;
pub mod sessions;
pub mod usage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Ok,
    Slow,
    Error,
}
