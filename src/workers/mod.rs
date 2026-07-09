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
    rate_limit_until: Option<tokio::time::Instant>,
}

impl Backoff {
    pub(crate) fn new(normal: Duration, elevated: Duration, threshold: u32) -> Self {
        Self {
            consecutive_errors: 0,
            normal,
            elevated,
            threshold,
            rate_limit_until: None,
        }
    }

    pub(crate) fn record_rate_limit(&mut self, retry_at: chrono::DateTime<chrono::Utc>) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        let delay_secs = retry_at
            .signed_duration_since(chrono::Utc::now())
            .num_seconds()
            .max(0) as u64;
        self.rate_limit_until = Some(tokio::time::Instant::now() + Duration::from_secs(delay_secs));
    }

    pub(crate) fn record(&mut self, is_err: bool) {
        if is_err {
            self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        } else {
            self.consecutive_errors = 0;
            self.rate_limit_until = None;
        }
    }

    pub(crate) fn interval(&self) -> Duration {
        if let Some(until) = self.rate_limit_until {
            let remaining = until.saturating_duration_since(tokio::time::Instant::now());
            if remaining > Duration::ZERO {
                return remaining;
            }
        }
        if self.consecutive_errors >= self.threshold {
            self.elevated
        } else {
            self.normal
        }
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
    async fn backoff_rate_limit_overrides_interval() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);
        let retry_at = chrono::Utc::now() + chrono::TimeDelta::seconds(120);
        b.record_rate_limit(retry_at);
        let interval = b.interval();
        // Should be close to 120s, not the normal 30s
        assert!(interval > Duration::from_secs(118), "got {interval:?}");
        assert!(interval <= Duration::from_secs(120), "got {interval:?}");
    }

    #[tokio::test]
    async fn backoff_rate_limit_clears_on_success() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);
        let retry_at = chrono::Utc::now() + chrono::TimeDelta::seconds(300);
        b.record_rate_limit(retry_at);
        assert!(b.interval() > Duration::from_secs(60));
        b.record(false);
        assert_eq!(b.interval(), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn backoff_expired_rate_limit_falls_through() {
        let mut b = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);
        let retry_at = chrono::Utc::now() - chrono::TimeDelta::seconds(10);
        b.record_rate_limit(retry_at);
        // Expired rate limit, but 1 consecutive error (below threshold of 3)
        assert_eq!(b.interval(), Duration::from_secs(30));
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
