use crate::error::FetchError;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClaudeVersion {
    pub(crate) installed: Option<String>,
    pub(crate) latest: Option<String>,
}

impl ClaudeVersion {
    pub(crate) fn is_outdated(&self) -> bool {
        match (&self.installed, &self.latest) {
            (Some(installed), Some(latest)) => {
                crate::fmt::compare_versions(installed, latest) == std::cmp::Ordering::Less
            }
            _ => false,
        }
    }
}

pub(crate) fn get_installed_version() -> Option<String> {
    let output = std::process::Command::new("claude")
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_version_output(&stdout).map(String::from)
}

/// Extract a version number from `claude --version` output.
/// Handles formats like "1.0.16", "claude 1.0.16", "claude v1.0.16".
fn parse_version_output(stdout: &str) -> Option<&str> {
    stdout.split_whitespace().find_map(|s| {
        if s.starts_with(|c: char| c.is_ascii_digit()) {
            Some(s)
        } else {
            s.strip_prefix('v')
                .filter(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        }
    })
}

/// Extract the `version` field from an npm registry JSON response.
fn extract_npm_version(json: &serde_json::Value) -> Result<String, FetchError> {
    json["version"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| FetchError::InvalidResponse("missing version field".into()))
}

pub(crate) async fn fetch_latest_version(client: &reqwest::Client) -> Result<String, FetchError> {
    let resp = client
        .get("https://registry.npmjs.org/@anthropic-ai/claude-code/latest")
        .send()
        .await?;
    let resp = crate::error::check_response(resp).await?;
    let json: serde_json::Value = resp.json().await?;
    extract_npm_version(&json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outdated_when_installed_is_older() {
        let v = ClaudeVersion {
            installed: Some("1.0.0".into()),
            latest: Some("1.0.1".into()),
        };
        assert!(v.is_outdated());
    }

    #[test]
    fn not_outdated_when_same() {
        let v = ClaudeVersion {
            installed: Some("1.0.1".into()),
            latest: Some("1.0.1".into()),
        };
        assert!(!v.is_outdated());
    }

    #[test]
    fn not_outdated_when_installed_is_newer() {
        let v = ClaudeVersion {
            installed: Some("2.0.0".into()),
            latest: Some("1.9.9".into()),
        };
        assert!(!v.is_outdated());
    }

    #[test]
    fn not_outdated_when_missing() {
        let v = ClaudeVersion::default();
        assert!(!v.is_outdated());
    }

    // ── extract_npm_version ─────────────────────────────────────

    #[test]
    fn extract_npm_version_valid() {
        let json = serde_json::json!({"version": "1.0.16"});
        assert_eq!(extract_npm_version(&json).unwrap(), "1.0.16");
    }

    #[test]
    fn extract_npm_version_missing_field() {
        let json = serde_json::json!({"name": "@anthropic-ai/claude-code"});
        assert!(extract_npm_version(&json).is_err());
    }

    #[test]
    fn extract_npm_version_null_field() {
        let json = serde_json::json!({"version": null});
        assert!(extract_npm_version(&json).is_err());
    }

    // ── parse_version_output ──────────────────────────────────────

    #[test]
    fn parse_version_bare_number() {
        assert_eq!(parse_version_output("1.0.16"), Some("1.0.16"));
    }

    #[test]
    fn parse_version_with_prefix() {
        assert_eq!(parse_version_output("claude 1.0.16"), Some("1.0.16"));
    }

    #[test]
    fn parse_version_with_newline() {
        assert_eq!(parse_version_output("1.0.16\n"), Some("1.0.16"));
    }

    #[test]
    fn parse_version_empty() {
        assert_eq!(parse_version_output(""), None);
    }

    #[test]
    fn parse_version_no_digits() {
        assert_eq!(parse_version_output("claude"), None);
    }

    #[test]
    fn parse_version_v_prefix() {
        assert_eq!(parse_version_output("v1.0.16"), Some("1.0.16"));
    }

    #[test]
    fn parse_version_v_prefix_with_label() {
        assert_eq!(parse_version_output("claude v1.0.16"), Some("1.0.16"));
    }
}
