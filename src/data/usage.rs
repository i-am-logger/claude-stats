use serde::Deserialize;

const USAGE_API_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const ANTHROPIC_BETA: &str = "oauth-2025-04-20";

/// Response from GET <https://api.anthropic.com/api/oauth/usage>
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
        self.utilization.map_or(0, |u| u.round() as u16)
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
        let Some(secs) = self.remaining_secs() else {
            return String::new();
        };

        if secs <= 0 {
            "now".to_string()
        } else {
            crate::fmt::format_duration(secs)
        }
    }
}

pub async fn fetch_usage(
    client: &reqwest::Client,
    token: &str,
) -> Result<UsageData, crate::error::FetchError> {
    let resp = client
        .get(USAGE_API_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .header("anthropic-beta", ANTHROPIC_BETA)
        .send()
        .await?;

    let resp = crate::error::check_response(resp).await?;
    let data: UsageData = resp.json().await?;
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(utilization: Option<f64>, resets_at: Option<&str>) -> UsageLimit {
        UsageLimit {
            utilization,
            resets_at: resets_at.map(String::from),
        }
    }

    #[test]
    fn percent_rounds() {
        assert_eq!(limit(Some(42.4), None).percent(), 42);
        assert_eq!(limit(Some(42.5), None).percent(), 43);
        assert_eq!(limit(Some(99.9), None).percent(), 100);
    }

    #[test]
    fn percent_none_utilization() {
        assert_eq!(limit(None, None).percent(), 0);
    }

    #[test]
    fn remaining_secs_none_without_reset() {
        assert_eq!(limit(None, None).remaining_secs(), None);
    }

    #[test]
    fn remaining_secs_past_date_is_zero() {
        let secs = limit(None, Some("2020-01-01T00:00:00Z")).remaining_secs();
        assert_eq!(secs, Some(0));
    }

    #[test]
    fn remaining_label_empty_without_reset() {
        assert_eq!(limit(None, None).remaining_label(), "");
    }

    #[test]
    fn remaining_label_past_date_is_now() {
        assert_eq!(
            limit(None, Some("2020-01-01T00:00:00Z")).remaining_label(),
            "now"
        );
    }
}
