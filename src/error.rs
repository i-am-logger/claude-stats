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
const DEFAULT_RETRY_SECS: i64 = 900;

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

/// Parse a `Retry-After` header value as either seconds or an HTTP-date (RFC 7231).
/// Returns `None` if the value is unparseable or zero seconds.
fn parse_retry_after(value: &str) -> Option<DateTime<Utc>> {
    // Try as seconds first (most common)
    if let Ok(secs) = value.parse::<i64>() {
        if secs > 0 {
            return Some(Utc::now() + chrono::TimeDelta::seconds(secs));
        }
        return None;
    }
    // Try as HTTP-date (IMF-fixdate): "Thu, 05 Mar 2026 06:40:00 GMT"
    chrono::NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT")
        .ok()
        .map(|naive| naive.and_utc())
        .filter(|dt| *dt > Utc::now())
}

/// Check an HTTP response status and return the response on success, or a
/// `FetchError::Api` with the status code and body text on failure.
pub async fn check_response(resp: reqwest::Response) -> Result<reqwest::Response, FetchError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_at = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after)
            .unwrap_or_else(|| Utc::now() + chrono::TimeDelta::seconds(DEFAULT_RETRY_SECS));
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── rate_limit_remaining_secs ─────────────────────────────

    #[test]
    fn remaining_secs_future_retry_at() {
        let retry_at = Utc::now() + chrono::TimeDelta::seconds(120);
        let err = FetchError::RateLimited { retry_at };
        let secs = err.rate_limit_remaining_secs().unwrap();
        // Allow 1s tolerance for test execution time
        assert!((119..=120).contains(&secs), "got {secs}");
    }

    #[test]
    fn remaining_secs_past_retry_at_clamped_to_zero() {
        let retry_at = Utc::now() - chrono::TimeDelta::seconds(60);
        let err = FetchError::RateLimited { retry_at };
        assert_eq!(err.rate_limit_remaining_secs(), Some(0));
    }

    #[test]
    fn remaining_secs_none_for_non_rate_limit() {
        assert_eq!(FetchError::Timeout.rate_limit_remaining_secs(), None);
    }

    // ── rate_limit_label ──────────────────────────────────────

    #[test]
    fn label_future_retry_at() {
        let retry_at = Utc::now() + chrono::TimeDelta::seconds(300);
        let err = FetchError::RateLimited { retry_at };
        let label = err.rate_limit_label().unwrap();
        assert!(label.contains('m'), "expected minutes in '{label}'");
    }

    #[test]
    fn label_expired_retry_at_is_now() {
        let retry_at = Utc::now() - chrono::TimeDelta::seconds(10);
        let err = FetchError::RateLimited { retry_at };
        assert_eq!(err.rate_limit_label(), Some("now".to_string()));
    }

    #[test]
    fn label_none_for_non_rate_limit() {
        assert_eq!(FetchError::Timeout.rate_limit_label(), None);
    }

    // ── check_response (429) ──────────────────────────────────

    // ── parse_retry_after ───────────────────────────────────────

    #[test]
    fn parse_retry_after_seconds() {
        let dt = parse_retry_after("120").unwrap();
        let secs = dt.signed_duration_since(Utc::now()).num_seconds();
        assert!((119..=120).contains(&secs), "got {secs}");
    }

    #[test]
    fn parse_retry_after_zero_returns_none() {
        assert!(parse_retry_after("0").is_none());
    }

    #[test]
    fn parse_retry_after_negative_returns_none() {
        assert!(parse_retry_after("-10").is_none());
    }

    #[test]
    fn parse_retry_after_http_date() {
        // Use a date far in the future so it's always valid
        let dt = parse_retry_after("Thu, 01 Jan 2099 00:00:00 GMT").unwrap();
        assert!(dt > Utc::now());
    }

    #[test]
    fn parse_retry_after_http_date_in_past_returns_none() {
        assert!(parse_retry_after("Thu, 01 Jan 2020 00:00:00 GMT").is_none());
    }

    #[test]
    fn parse_retry_after_garbage_returns_none() {
        assert!(parse_retry_after("not-a-date").is_none());
    }

    // ── check_response (429) ──────────────────────────────────

    #[tokio::test]
    async fn check_response_429_with_retry_after() {
        let resp = http::Response::builder()
            .status(429)
            .header("retry-after", "120")
            .body("")
            .unwrap();
        let resp = reqwest::Response::from(resp);
        let err = check_response(resp).await.unwrap_err();
        match err {
            FetchError::RateLimited { retry_at } => {
                let secs = retry_at.signed_duration_since(Utc::now()).num_seconds();
                assert!((119..=120).contains(&secs), "got {secs}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_response_429_without_header_uses_default() {
        let resp = http::Response::builder().status(429).body("").unwrap();
        let resp = reqwest::Response::from(resp);
        let err = check_response(resp).await.unwrap_err();
        match err {
            FetchError::RateLimited { retry_at } => {
                let secs = retry_at.signed_duration_since(Utc::now()).num_seconds();
                assert!((899..=900).contains(&secs), "got {secs}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_response_429_with_zero_retry_after_uses_default() {
        let resp = http::Response::builder()
            .status(429)
            .header("retry-after", "0")
            .body("")
            .unwrap();
        let resp = reqwest::Response::from(resp);
        let err = check_response(resp).await.unwrap_err();
        match err {
            FetchError::RateLimited { retry_at } => {
                let secs = retry_at.signed_duration_since(Utc::now()).num_seconds();
                assert!((899..=900).contains(&secs), "got {secs}");
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_response_success_passes_through() {
        let resp = http::Response::builder().status(200).body("").unwrap();
        let resp = reqwest::Response::from(resp);
        assert!(check_response(resp).await.is_ok());
    }

    #[tokio::test]
    async fn check_response_500_returns_api_error() {
        let resp = http::Response::builder().status(500).body("oops").unwrap();
        let resp = reqwest::Response::from(resp);
        let err = check_response(resp).await.unwrap_err();
        match err {
            FetchError::Api { status, body } => {
                assert_eq!(status, 500);
                assert_eq!(body, "oops");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }
}
