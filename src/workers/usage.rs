use super::{Backoff, ResourceGuard};
use crate::credentials::{self, Plan};
use crate::data::{profile, usage};
use crate::error::{CredentialError, FetchError};
use crate::event::{AppEvent, EventTx, ResourceKind};
use std::time::{Duration, Instant};

/// Tracked account state for change detection.
#[derive(Default, PartialEq, Eq)]
struct AccountState {
    email: Option<String>,
    plan: Option<Plan>,
}

#[allow(
    clippy::too_many_lines,
    reason = "sequential async steps in a single worker loop"
)]
pub(crate) fn spawn(tx: EventTx, client: reqwest::Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut backoff = Backoff::new(Duration::from_secs(600), Duration::from_secs(900), 3);
        let mut cached_token: Option<String> = None;
        let mut prev_account = AccountState::default();

        loop {
            if tx.send(AppEvent::UsageFetching).await.is_err() {
                break;
            }

            let creds_result = {
                let _guard = ResourceGuard::acquire(&tx, ResourceKind::Disk);
                super::blocking(
                    credentials::get_credentials,
                    Err(CredentialError::FileNotFound),
                )
                .await
            };

            match creds_result {
                Ok(creds) => {
                    tracing::debug!("credentials loaded");
                    let (result, elapsed) = {
                        let _guard = ResourceGuard::acquire(&tx, ResourceKind::Network);

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
                        (result, start.elapsed())
                    };

                    if let Err(ref e) = result {
                        tracing::warn!("usage fetch error: {e}");
                    }
                    if let Err(FetchError::RateLimited { retry_at }) = &result {
                        backoff.record_rate_limit(*retry_at);
                    } else {
                        backoff.record(result.is_err());
                    }

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
                    tracing::warn!("credential error: {e}");
                    backoff.record(true);
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

            tokio::time::sleep(backoff.interval()).await;
        }
    })
}
