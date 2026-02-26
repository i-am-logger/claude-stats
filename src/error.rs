use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub(crate) enum FetchError {
    #[error("API error ({status}): {body}")]
    Api { status: u16, body: String },

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

/// Check an HTTP response status and return the response on success, or a
/// `FetchError::Api` with the status code and body text on failure.
pub(crate) async fn check_response(
    resp: reqwest::Response,
) -> Result<reqwest::Response, FetchError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .unwrap_or_else(|e| format!("(failed to read body: {e})"));
    Err(FetchError::Api { status, body })
}

#[derive(Debug, Clone, Error)]
pub(crate) enum CredentialError {
    #[error("home directory not found")]
    NoHomeDir,

    #[error("credentials file not found: ~/.claude/.credentials.json")]
    FileNotFound,

    #[error("invalid credentials file: {0}")]
    InvalidJson(String),

    #[error("missing OAuth access token in credentials")]
    MissingToken,
}
