use super::{Backoff, ResourceGuard};
use crate::data::self_version::{self, SelfVersion};
use crate::event::{AppEvent, EventTx, ResourceKind};
use std::time::Duration;

pub(crate) fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Delay the first check to avoid hitting GitHub on every app restart
        // and to keep startup fast.
        tokio::time::sleep(Duration::from_secs(10)).await;

        let mut backoff = Backoff::new(
            Duration::from_secs(3600), // 1 hour
            Duration::from_secs(7200), // 2 hours
            3,
        );
        let mut prev: Option<SelfVersion> = None;

        loop {
            let latest_result = {
                let _guard = ResourceGuard::acquire(&tx, ResourceKind::Network);
                self_version::fetch_latest_version(&client).await
            };
            if let Err(ref e) = latest_result {
                tracing::warn!("self-version fetch error: {e}");
            }
            backoff.record(latest_result.is_err());
            let latest = latest_result.ok();

            let version = SelfVersion { latest };
            if prev.as_ref() != Some(&version) {
                if tx
                    .send(AppEvent::SelfVersionUpdated(version.clone()))
                    .await
                    .is_err()
                {
                    break;
                }
                prev = Some(version);
            }

            tokio::time::sleep(backoff.interval()).await;
        }
    })
}
