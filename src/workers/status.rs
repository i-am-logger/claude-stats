use crate::data::incidents;
use crate::event::{AppEvent, EventTx};
use std::time::Duration;

pub fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let result = incidents::fetch_status(&client).await;
            if tx.send(AppEvent::StatusUpdated(result)).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    })
}
