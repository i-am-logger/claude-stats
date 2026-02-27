#![allow(
    unreachable_pub,
    reason = "pub items exposed via lib.rs for benchmarks"
)]

use crate::error::CredentialError;
use std::fmt;
use std::fs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Max,
    Pro,
    Team,
    Enterprise,
    Other(String),
}

impl Plan {
    pub fn from_api(s: &str) -> Self {
        match s {
            "max" => Self::Max,
            "pro" => Self::Pro,
            "team" => Self::Team,
            "enterprise" => Self::Enterprise,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Max => write!(f, "Claude Max"),
            Self::Pro => write!(f, "Claude Pro"),
            Self::Team => write!(f, "Claude Team"),
            Self::Enterprise => write!(f, "Claude Enterprise"),
            Self::Other(s) => write!(f, "{s}"),
        }
    }
}

pub(crate) struct Credentials {
    pub(crate) token: String,
    pub(crate) plan: Option<Plan>,
}

pub(crate) fn get_credentials() -> Result<Credentials, CredentialError> {
    let path = dirs::home_dir()
        .ok_or(CredentialError::NoHomeDir)?
        .join(".claude")
        .join(".credentials.json");
    let contents = fs::read_to_string(&path).map_err(|_| CredentialError::FileNotFound)?;
    let creds: serde_json::Value =
        serde_json::from_str(&contents).map_err(|e| CredentialError::InvalidJson(e.to_string()))?;
    let oauth = &creds["claudeAiOauth"];
    let token = oauth["accessToken"]
        .as_str()
        .ok_or(CredentialError::MissingToken)?
        .to_string();
    let plan = oauth["subscriptionType"].as_str().map(Plan::from_api);
    Ok(Credentials { token, plan })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_plans_display() {
        assert_eq!(Plan::Max.to_string(), "Claude Max");
        assert_eq!(Plan::Pro.to_string(), "Claude Pro");
        assert_eq!(Plan::Team.to_string(), "Claude Team");
        assert_eq!(Plan::Enterprise.to_string(), "Claude Enterprise");
    }

    #[test]
    fn unknown_plan_display() {
        assert_eq!(Plan::Other("free".to_string()).to_string(), "free");
        assert_eq!(
            Plan::Other("custom_tier".to_string()).to_string(),
            "custom_tier"
        );
    }

    #[test]
    fn from_api_known() {
        assert_eq!(Plan::from_api("max"), Plan::Max);
        assert_eq!(Plan::from_api("pro"), Plan::Pro);
        assert_eq!(Plan::from_api("team"), Plan::Team);
        assert_eq!(Plan::from_api("enterprise"), Plan::Enterprise);
    }

    #[test]
    fn from_api_unknown() {
        assert_eq!(Plan::from_api("free"), Plan::Other("free".to_string()));
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_from_api_never_panics(s in "\\PC{0,100}") {
                let _plan = Plan::from_api(&s);
            }
        }
    }
}
