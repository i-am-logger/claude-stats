use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fmt;

const STATUS_API_URL: &str = "https://status.claude.com/api/v2/status.json";
const INCIDENTS_API_URL: &str = "https://status.claude.com/api/v2/incidents/unresolved.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncidentStatus {
    Investigating,
    Identified,
    Monitoring,
    Postmortem,
    Resolved,
    #[serde(other)]
    Unknown,
}

impl fmt::Display for IncidentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Investigating => write!(f, "INVESTIGATING"),
            Self::Identified => write!(f, "IDENTIFIED"),
            Self::Monitoring => write!(f, "MONITORING"),
            Self::Postmortem => write!(f, "POSTMORTEM"),
            Self::Resolved => write!(f, "RESOLVED"),
            Self::Unknown => write!(f, "UNKNOWN"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IncidentImpact {
    None,
    Minor,
    Major,
    Critical,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusIndicator {
    None,
    Minor,
    Major,
    Critical,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
struct IncidentsResponse {
    incidents: Vec<Incident>,
}

#[derive(Debug, Clone, Deserialize)]
struct IncidentUpdate {
    body: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Incident {
    pub name: String,
    pub status: IncidentStatus,
    pub impact: IncidentImpact,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, rename = "incident_updates")]
    updates: Vec<IncidentUpdate>,
}

impl Incident {
    /// Latest update body text, if any.
    pub fn latest_body(&self) -> Option<&str> {
        self.updates.first().map(|u| u.body.as_str())
    }

    /// Human-friendly relative time label, e.g. "2h 15m ago".
    pub fn started_ago(&self) -> String {
        relative_time(Utc::now() - self.started_at)
    }

    /// How long since the last update.
    pub fn updated_ago(&self) -> String {
        let latest = self
            .updates
            .first()
            .map_or(self.updated_at, |u| u.created_at);
        relative_time(Utc::now() - latest)
    }
}

fn relative_time(delta: chrono::TimeDelta) -> String {
    let secs = delta.num_seconds().max(0);
    if secs < 60 {
        "just now".to_string()
    } else {
        format!("{} ago", crate::fmt::format_duration(secs))
    }
}

/// Overall status from the statuspage API.
#[derive(Debug, Clone, Deserialize)]
struct StatusResponse {
    status: StatusSummary,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatusSummary {
    pub indicator: StatusIndicator,
    pub description: String,
}

/// Combined result: overall status + unresolved incidents.
#[derive(Debug, Clone)]
pub struct StatusData {
    pub summary: StatusSummary,
    pub incidents: Vec<Incident>,
}

pub async fn fetch_status(
    client: &reqwest::Client,
) -> Result<StatusData, crate::error::FetchError> {
    let (status_resp, incidents_resp) = tokio::join!(
        client.get(STATUS_API_URL).send(),
        client.get(INCIDENTS_API_URL).send(),
    );

    let summary = {
        let resp = crate::error::check_response(status_resp?).await?;
        let data: StatusResponse = resp.json().await?;
        data.status
    };

    let incidents = {
        let resp = incidents_resp?;
        if resp.status().is_success() {
            let data: IncidentsResponse = resp.json().await?;
            data.incidents
        } else {
            Vec::new()
        }
    };

    Ok(StatusData { summary, incidents })
}
