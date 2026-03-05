use super::{Backoff, ResourceGuard};
use crate::data::incidents;
use crate::event::{AppEvent, EventTx, ResourceKind};
use std::time::Duration;

pub(crate) fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Backoff::new(Duration::from_secs(360), Duration::from_secs(300), 3);

        loop {
            let result = {
                let _guard = ResourceGuard::acquire(&tx, ResourceKind::Network);
                incidents::fetch_status(&client).await
            };
            if let Err(ref e) = result {
                tracing::warn!("status fetch error: {e}");
            }
            if let Err(crate::error::FetchError::RateLimited { retry_at }) = &result {
                backoff.record_rate_limit(*retry_at);
            } else {
                backoff.record(result.is_err());
            }

            if tx.send(AppEvent::StatusUpdated(result)).await.is_err() {
                break;
            }

            tokio::time::sleep(backoff.interval()).await;
        }
    })
}
