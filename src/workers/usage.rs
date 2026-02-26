use crate::credentials;
use crate::data::usage;
use crate::error::{CredentialError, FetchError};
use crate::event::{AppEvent, EventTx};
use std::time::{Duration, Instant};

const NORMAL_INTERVAL: Duration = Duration::from_secs(5);
const BACKOFF_INTERVAL: Duration = Duration::from_secs(30);
const BACKOFF_THRESHOLD: u32 = 3;

pub(crate) fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut consecutive_cred_errors: u32 = 0;

        loop {
            if tx.send(AppEvent::UsageFetching).await.is_err() {
                break;
            }

            let creds_result = super::blocking(
                credentials::get_credentials,
                Err(CredentialError::FileNotFound),
            )
            .await;

            match creds_result {
                Ok(creds) => {
                    consecutive_cred_errors = 0;
                    let start = Instant::now();
                    let result = usage::fetch_usage(&client, &creds.token).await;
                    let elapsed = start.elapsed();
                    if tx
                        .send(AppEvent::UsageUpdated {
                            data: result,
                            elapsed,
                            plan: creds.plan,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    consecutive_cred_errors = consecutive_cred_errors.saturating_add(1);
                    if tx
                        .send(AppEvent::UsageUpdated {
                            data: Err(FetchError::from(e)),
                            elapsed: Duration::ZERO,
                            plan: None,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }

            let sleep_dur = if consecutive_cred_errors >= BACKOFF_THRESHOLD {
                BACKOFF_INTERVAL
            } else {
                NORMAL_INTERVAL
            };
            tokio::time::sleep(sleep_dur).await;
        }
    })
}
