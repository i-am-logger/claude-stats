pub(crate) mod claude_version;
pub(crate) mod sessions;
pub(crate) mod status;
pub(crate) mod usage;

use std::time::Duration;

pub(crate) fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("claude-stats/{}", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Run a blocking closure on the tokio blocking thread pool.
///
/// If the task panics, the panic is propagated. If the task is cancelled
/// (runtime shutting down), `fallback` is returned instead.
pub(crate) async fn blocking<F, R>(f: F, fallback: R) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| {
        if e.is_panic() {
            std::panic::resume_unwind(e.into_panic());
        }
        fallback
    })
}
