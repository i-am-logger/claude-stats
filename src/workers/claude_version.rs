use crate::data::claude_version::{self, ClaudeVersion};
use crate::event::{AppEvent, EventTx};
use std::time::Duration;

const NORMAL_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes
const BACKOFF_INTERVAL: Duration = Duration::from_secs(1200); // 20 minutes
const BACKOFF_THRESHOLD: u32 = 3;

pub(crate) fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut consecutive_errors: u32 = 0;
        let mut prev: Option<ClaudeVersion> = None;

        loop {
            let installed = super::blocking(claude_version::get_installed_version, None).await;
            let latest_result = claude_version::fetch_latest_version(&client).await;
            let is_err = latest_result.is_err();
            let latest = latest_result.ok();

            let version = ClaudeVersion { installed, latest };
            if prev.as_ref() != Some(&version) {
                if tx
                    .send(AppEvent::ClaudeVersionUpdated(version.clone()))
                    .await
                    .is_err()
                {
                    break;
                }
                prev = Some(version);
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
