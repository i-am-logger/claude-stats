use crate::error::CredentialError;
use std::fs;

pub struct Credentials {
    pub token: String,
    pub plan: Option<String>,
}

pub fn get_credentials() -> Result<Credentials, CredentialError> {
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
    let plan = oauth["subscriptionType"]
        .as_str()
        .map(|s| plan_display_name(s).to_string());
    Ok(Credentials { token, plan })
}

fn plan_display_name(s: &str) -> &str {
    match s {
        "max" => "Claude Max",
        "pro" => "Claude Pro",
        "team" => "Claude Team",
        "enterprise" => "Claude Enterprise",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_plans() {
        assert_eq!(plan_display_name("max"), "Claude Max");
        assert_eq!(plan_display_name("pro"), "Claude Pro");
        assert_eq!(plan_display_name("team"), "Claude Team");
        assert_eq!(plan_display_name("enterprise"), "Claude Enterprise");
    }

    #[test]
    fn unknown_plan_passes_through() {
        assert_eq!(plan_display_name("free"), "free");
        assert_eq!(plan_display_name("custom_tier"), "custom_tier");
    }
}
