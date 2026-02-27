use super::{Backoff, ResourceGuard};
use crate::data::incidents;
use crate::event::{AppEvent, EventTx, ResourceKind};
use std::time::Duration;

pub(crate) fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Backoff::new(Duration::from_secs(30), Duration::from_secs(60), 3);

        loop {
            let result = {
                let _guard = ResourceGuard::acquire(&tx, ResourceKind::Network);
                incidents::fetch_status(&client).await
            };
            if let Err(ref e) = result {
                tracing::warn!("status fetch error: {e}");
            }
            backoff.record(result.is_err());
            if tx.send(AppEvent::StatusUpdated(result)).await.is_err() {
                break;
            }

            tokio::time::sleep(backoff.interval()).await;
        }
    })
}
