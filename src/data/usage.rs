use serde::Deserialize;

/// Response from GET https://api.anthropic.com/api/oauth/usage
#[derive(Debug, Clone, Deserialize)]
pub struct UsageData {
    pub five_hour: Option<UsageLimit>,
    pub seven_day: Option<UsageLimit>,
    pub seven_day_opus: Option<UsageLimit>,
    pub seven_day_sonnet: Option<UsageLimit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageLimit {
    #[serde(default)]
    pub utilization: Option<f64>,

    #[serde(default)]
    pub resets_at: Option<String>,
}

impl UsageLimit {
    pub fn percent(&self) -> u16 {
        self.utilization.map(|u| u.round() as u16).unwrap_or(0)
    }

    /// Seconds remaining until reset, or None if no reset time.
    pub fn remaining_secs(&self) -> Option<i64> {
        let ts = self.resets_at.as_ref()?;
        let dt = chrono::DateTime::parse_from_rfc3339(ts)
            .ok()?
            .with_timezone(&chrono::Utc);
        let secs = dt.signed_duration_since(chrono::Utc::now()).num_seconds();
        Some(secs.max(0))
    }

    /// Human-readable remaining time label with seconds.
    pub fn remaining_label(&self) -> String {
        let secs = match self.remaining_secs() {
            Some(s) => s,
            None => return String::new(),
        };

        if secs <= 0 {
            "now".to_string()
        } else if secs < 60 {
            format!("{}s", secs)
        } else if secs < 3600 {
            format!("{}m {:02}s", secs / 60, secs % 60)
        } else if secs < 86400 {
            let hours = secs / 3600;
            let mins = (secs % 3600) / 60;
            let s = secs % 60;
            format!("{}h {:02}m {:02}s", hours, mins, s)
        } else {
            let days = secs / 86400;
            let hours = (secs % 86400) / 3600;
            let mins = (secs % 3600) / 60;
            format!("{}d {}h {:02}m", days, hours, mins)
        }
    }
}

pub async fn fetch_usage(token: &str) -> anyhow::Result<UsageData> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .header("anthropic-beta", "oauth-2025-04-20")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("API returned {}: {}", status, body);
    }

    let data: UsageData = resp.json().await?;
    Ok(data)
}
