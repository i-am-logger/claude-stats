pub mod sessions;
pub mod status;
pub mod usage;

use std::time::Duration;

pub fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("claude-stats/{}", env!("CARGO_PKG_VERSION")))
        .build()
}
