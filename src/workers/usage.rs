use crate::credentials::{self, Plan};
use crate::data::{profile, usage};
use crate::error::{CredentialError, FetchError};
use crate::event::{AppEvent, EventTx};
use std::time::{Duration, Instant};

const NORMAL_INTERVAL: Duration = Duration::from_secs(30);
const BACKOFF_INTERVAL: Duration = Duration::from_secs(60);
const BACKOFF_THRESHOLD: u32 = 3;

/// Tracked account state for change detection.
#[derive(Default, PartialEq, Eq)]
struct AccountState {
    email: Option<String>,
    plan: Option<Plan>,
}

pub(crate) fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut consecutive_cred_errors: u32 = 0;
        let mut cached_token: Option<String> = None;
        let mut prev_account = AccountState::default();

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

                    // Re-fetch profile email when the token changes (login/logout)
                    let token_changed = cached_token.as_ref() != Some(&creds.token);
                    let email = if token_changed {
                        cached_token = Some(creds.token.clone());
                        profile::fetch_profile_email(&client, &creds.token)
                            .await
                            .ok()
                    } else {
                        prev_account.email.clone()
                    };

                    let current = AccountState {
                        email,
                        plan: creds.plan,
                    };
                    if current != prev_account {
                        if tx
                            .send(AppEvent::AccountUpdated {
                                email: current.email.clone(),
                                plan: current.plan.clone(),
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                        prev_account = current;
                    }

                    let start = Instant::now();
                    let result = usage::fetch_usage(&client, &creds.token).await;
                    let elapsed = start.elapsed();
                    if tx
                        .send(AppEvent::UsageUpdated {
                            data: result,
                            elapsed,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    consecutive_cred_errors = consecutive_cred_errors.saturating_add(1);
                    cached_token = None;

                    let logged_out = AccountState::default();
                    if logged_out != prev_account {
                        if tx
                            .send(AppEvent::AccountUpdated {
                                email: None,
                                plan: None,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                        prev_account = logged_out;
                    }

                    if tx
                        .send(AppEvent::UsageUpdated {
                            data: Err(FetchError::from(e)),
                            elapsed: Duration::ZERO,
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
