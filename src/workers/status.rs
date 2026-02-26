use crate::data::incidents;
use crate::event::{AppEvent, EventTx};
use std::time::Duration;

const NORMAL_INTERVAL: Duration = Duration::from_secs(5);
const BACKOFF_INTERVAL: Duration = Duration::from_secs(30);
const BACKOFF_THRESHOLD: u32 = 3;

pub(crate) fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut consecutive_errors: u32 = 0;

        loop {
            let result = incidents::fetch_status(&client).await;
            let is_err = result.is_err();
            if tx.send(AppEvent::StatusUpdated(result)).await.is_err() {
                break;
            }

            if is_err {
                consecutive_errors = consecutive_errors.saturating_add(1);
            } else {
                consecutive_errors = 0;
            }

            let sleep_dur = if consecutive_errors >= BACKOFF_THRESHOLD {
                BACKOFF_INTERVAL
            } else {
                NORMAL_INTERVAL
            };
            tokio::time::sleep(sleep_dur).await;
        }
    })
}
