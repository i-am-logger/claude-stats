pub(crate) mod claude_version;
pub(crate) mod self_version;
pub(crate) mod sessions;
pub(crate) mod status;
pub(crate) mod usage;

use crate::event::{AppEvent, EventTx, ResourceKind};
use std::time::Duration;

/// RAII guard that sends `ResourceIdle` on drop, guaranteeing Busy/Idle pairing.
pub(crate) struct ResourceGuard {
    tx: EventTx,
    kind: ResourceKind,
}

impl ResourceGuard {
    pub(crate) fn acquire(tx: &EventTx, kind: ResourceKind) -> Self {
        drop(tx.try_send(AppEvent::ResourceBusy(kind)));
        Self {
            tx: tx.clone(),
            kind,
        }
    }
}

impl Drop for ResourceGuard {
    fn drop(&mut self) {
        drop(self.tx.try_send(AppEvent::ResourceIdle(self.kind)));
    }
}

pub(crate) fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("claude-stats/{}", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Tracks consecutive errors and provides backoff intervals.
pub(crate) struct Backoff {
    consecutive_errors: u32,
    normal: Duration,
    elevated: Duration,
    threshold: u32,
}

impl Backoff {
    pub(crate) const fn new(normal: Duration, elevated: Duration, threshold: u32) -> Self {
        Self {
            consecutive_errors: 0,
            normal,
            elevated,
            threshold,
        }
    }

    pub(crate) fn record(&mut self, is_err: bool) {
        if is_err {
            self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        } else {
            self.consecutive_errors = 0;
        }
    }

    pub(crate) const fn interval(&self) -> Duration {
        if self.consecutive_errors >= self.threshold {
            self.elevated
        } else {
            self.normal
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_with_normal_interval() {
        let b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);
        assert_eq!(b.interval(), Duration::from_secs(30));
    }

    #[test]
    fn backoff_stays_normal_below_threshold() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);
        b.record(true);
        b.record(true);
        assert_eq!(b.interval(), Duration::from_secs(30));
    }

    #[test]
    fn backoff_activates_at_threshold() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);
        b.record(true);
        b.record(true);
        b.record(true);
        assert_eq!(b.interval(), Duration::from_secs(60));
    }

    #[test]
    fn backoff_resets_on_success() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);
        b.record(true);
        b.record(true);
        b.record(true);
        assert_eq!(b.interval(), Duration::from_secs(60));

        b.record(false);
        assert_eq!(b.interval(), Duration::from_secs(30));
    }

    #[test]
    fn backoff_stays_elevated_above_threshold() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);
        for _ in 0..100 {
            b.record(true);
        }
        assert_eq!(b.interval(), Duration::from_secs(60));
    }

    #[tokio::test]
    async fn resource_guard_sends_busy_and_idle() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        {
            let _guard = ResourceGuard::acquire(&tx, ResourceKind::Network);
        }
        let busy = rx.try_recv().unwrap();
        let idle = rx.try_recv().unwrap();
        assert!(matches!(
            busy,
            AppEvent::ResourceBusy(ResourceKind::Network)
        ));
        assert!(matches!(
            idle,
            AppEvent::ResourceIdle(ResourceKind::Network)
        ));
    }

    #[tokio::test]
    async fn resource_guard_sends_idle_on_early_exit() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let result: Option<i32> = {
            let _guard = ResourceGuard::acquire(&tx, ResourceKind::Disk);
            None // simulate early return
        };
        assert!(result.is_none());
        let busy = rx.try_recv().unwrap();
        let idle = rx.try_recv().unwrap();
        assert!(matches!(busy, AppEvent::ResourceBusy(ResourceKind::Disk)));
        assert!(matches!(idle, AppEvent::ResourceIdle(ResourceKind::Disk)));
    }
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
