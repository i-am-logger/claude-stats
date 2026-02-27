use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::fmt;

const STATUS_API_URL: &str = "https://status.claude.com/api/v2/status.json";
const INCIDENTS_API_URL: &str = "https://status.claude.com/api/v2/incidents/unresolved.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum IncidentStatus {
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
pub(crate) enum IncidentImpact {
    None,
    Minor,
    Major,
    Critical,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StatusIndicator {
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
pub(crate) struct Incident {
    pub(crate) name: String,
    pub(crate) status: IncidentStatus,
    pub(crate) impact: IncidentImpact,
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
    #[serde(default, rename = "incident_updates")]
    updates: Vec<IncidentUpdate>,
}

impl Incident {
    /// Latest update body text, if any.
    pub(crate) fn latest_body(&self) -> Option<&str> {
        self.updates.first().map(|u| u.body.as_str())
    }

    /// Human-friendly relative time label, e.g. "2h 15m ago".
    pub(crate) fn started_ago(&self) -> String {
        relative_time(Utc::now() - self.started_at)
    }

    /// How long since the last update.
    pub(crate) fn updated_ago(&self) -> String {
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
pub(crate) struct StatusSummary {
    pub(crate) indicator: StatusIndicator,
    pub(crate) description: String,
}

/// Combined result: overall status + unresolved incidents.
#[derive(Debug, Clone)]
pub(crate) struct StatusData {
    pub(crate) summary: StatusSummary,
    pub(crate) incidents: Vec<Incident>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_just_now_under_60s() {
        assert_eq!(relative_time(chrono::TimeDelta::seconds(0)), "just now");
        assert_eq!(relative_time(chrono::TimeDelta::seconds(30)), "just now");
        assert_eq!(relative_time(chrono::TimeDelta::seconds(59)), "just now");
    }

    #[test]
    fn relative_time_boundary_at_60s() {
        let result = relative_time(chrono::TimeDelta::seconds(60));
        assert_eq!(result, "1m ago");
    }

    #[test]
    fn relative_time_negative_clamped() {
        assert_eq!(relative_time(chrono::TimeDelta::seconds(-10)), "just now");
    }

    #[test]
    fn relative_time_hours() {
        let result = relative_time(chrono::TimeDelta::seconds(7200));
        assert_eq!(result, "2h ago");
    }

    fn make_incident(updates: Vec<IncidentUpdate>) -> Incident {
        Incident {
            name: "Test incident".into(),
            status: IncidentStatus::Investigating,
            impact: IncidentImpact::Minor,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            updates,
        }
    }

    #[test]
    fn latest_body_with_updates() {
        let incident = make_incident(vec![IncidentUpdate {
            body: "We are investigating".into(),
            created_at: Utc::now(),
        }]);
        assert_eq!(incident.latest_body(), Some("We are investigating"));
    }

    #[test]
    fn latest_body_no_updates() {
        let incident = make_incident(vec![]);
        assert_eq!(incident.latest_body(), None);
    }

    #[test]
    fn started_ago_returns_string() {
        let incident = make_incident(vec![]);
        let result = incident.started_ago();
        assert_eq!(result, "just now");
    }

    #[test]
    fn updated_ago_uses_latest_update() {
        let incident = make_incident(vec![IncidentUpdate {
            body: "update".into(),
            created_at: Utc::now(),
        }]);
        let result = incident.updated_ago();
        assert_eq!(result, "just now");
    }

    #[test]
    fn updated_ago_falls_back_to_updated_at() {
        let incident = make_incident(vec![]);
        let result = incident.updated_ago();
        assert_eq!(result, "just now");
    }

    #[test]
    fn incident_status_display() {
        assert_eq!(IncidentStatus::Investigating.to_string(), "INVESTIGATING");
        assert_eq!(IncidentStatus::Identified.to_string(), "IDENTIFIED");
        assert_eq!(IncidentStatus::Monitoring.to_string(), "MONITORING");
        assert_eq!(IncidentStatus::Postmortem.to_string(), "POSTMORTEM");
        assert_eq!(IncidentStatus::Resolved.to_string(), "RESOLVED");
        assert_eq!(IncidentStatus::Unknown.to_string(), "UNKNOWN");
    }

    #[test]
    fn serde_incident_status() {
        let s: IncidentStatus = serde_json::from_str(r#""investigating""#).unwrap();
        assert_eq!(s, IncidentStatus::Investigating);
    }

    #[test]
    fn serde_unknown_status_fallback() {
        let s: IncidentStatus = serde_json::from_str(r#""some_new_status""#).unwrap();
        assert_eq!(s, IncidentStatus::Unknown);
    }

    #[test]
    fn serde_impact() {
        let i: IncidentImpact = serde_json::from_str(r#""minor""#).unwrap();
        assert_eq!(i, IncidentImpact::Minor);
        let i: IncidentImpact = serde_json::from_str(r#""critical""#).unwrap();
        assert_eq!(i, IncidentImpact::Critical);
    }

    #[test]
    fn serde_status_indicator() {
        let s: StatusIndicator = serde_json::from_str(r#""none""#).unwrap();
        assert_eq!(s, StatusIndicator::None);
        let s: StatusIndicator = serde_json::from_str(r#""major""#).unwrap();
        assert_eq!(s, StatusIndicator::Major);
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_unknown_status_deserializes(s in "[a-z_]{1,30}") {
                let json = format!(r#""{s}""#);
                let status: Result<IncidentStatus, _> = serde_json::from_str(&json);
                // Known variants match; everything else is Unknown
                prop_assert!(status.is_ok());
            }

            #[test]
            fn prop_unknown_impact_deserializes(s in "[a-z_]{1,30}") {
                let json = format!(r#""{s}""#);
                let impact: Result<IncidentImpact, _> = serde_json::from_str(&json);
                prop_assert!(impact.is_ok());
            }

            #[test]
            fn prop_unknown_indicator_deserializes(s in "[a-z_]{1,30}") {
                let json = format!(r#""{s}""#);
                let indicator: Result<StatusIndicator, _> = serde_json::from_str(&json);
                prop_assert!(indicator.is_ok());
            }
        }
    }
}

pub(crate) async fn fetch_status(
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
        let resp = crate::error::check_response(incidents_resp?).await?;
        let data: IncidentsResponse = resp.json().await?;
        data.incidents
    };

    Ok(StatusData { summary, incidents })
}
