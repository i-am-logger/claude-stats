#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use chrono::{DateTime, Utc};
use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum FetchError {
    #[error("API error ({status}): {body}")]
    Api { status: u16, body: String },

    #[error("rate limited (retry at {retry_at})")]
    RateLimited { retry_at: DateTime<Utc> },

    #[error("request timed out")]
    Timeout,

    #[error("network error: {0}")]
    Network(String),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("{0}")]
    Credential(#[from] CredentialError),
}

impl From<reqwest::Error> for FetchError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            Self::Timeout
        } else if e.is_decode() {
            Self::InvalidResponse(e.to_string())
        } else {
            Self::Network(e.to_string())
        }
    }
}

/// Default retry delay (seconds) when a 429 response lacks a valid `Retry-After` header.
const DEFAULT_RETRY_SECS: i64 = 300;

impl FetchError {
    /// Seconds remaining until rate limit expires, or `None` for non-rate-limit errors.
    pub fn rate_limit_remaining_secs(&self) -> Option<i64> {
        if let Self::RateLimited { retry_at } = self {
            Some(
                retry_at
                    .signed_duration_since(Utc::now())
                    .num_seconds()
                    .max(0),
            )
        } else {
            None
        }
    }

    /// Human-readable countdown label for rate limit errors.
    pub fn rate_limit_label(&self) -> Option<String> {
        let secs = self.rate_limit_remaining_secs()?;
        if secs == 0 {
            Some("now".to_string())
        } else {
            Some(crate::fmt::format_duration(secs))
        }
    }
}

/// Check an HTTP response status and return the response on success, or a
/// `FetchError::Api` with the status code and body text on failure.
pub async fn check_response(resp: reqwest::Response) -> Result<reqwest::Response, FetchError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let delay_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&secs| secs > 0)
            .unwrap_or(DEFAULT_RETRY_SECS);
        let retry_at = Utc::now() + chrono::TimeDelta::seconds(delay_secs);
        return Err(FetchError::RateLimited { retry_at });
    }
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(failed to read body: {e})"));
    Err(FetchError::Api { status, body })
}

#[derive(Debug, Clone, Error)]
pub enum CredentialError {
    #[error("home directory not found")]
    NoHomeDir,

    #[error("credentials file not found: ~/.claude/.credentials.json")]
    FileNotFound,

    #[error("invalid credentials file: {0}")]
    InvalidJson(String),

    #[error("missing OAuth access token in credentials")]
    MissingToken,
}
