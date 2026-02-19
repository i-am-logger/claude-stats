pub mod usage;

#[derive(Clone, Copy)]
pub enum HealthStatus {
    Ok,
    Slow,
    Error,
}
