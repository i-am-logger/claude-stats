use crate::error::FetchError;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClaudeVersion {
    pub(crate) installed: Option<String>,
    pub(crate) latest: Option<String>,
}

impl ClaudeVersion {
    pub(crate) fn is_outdated(&self) -> bool {
        match (&self.installed, &self.latest) {
            (Some(installed), Some(latest)) if installed != latest => {
                compare_versions(installed, latest) == std::cmp::Ordering::Less
            }
            _ => false,
        }
    }
}

/// Compare two dotted version strings (e.g. "1.2.3" vs "1.3.0").
/// Returns `Ordering::Less` if `a < b`, etc.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let va = parse(a);
    let vb = parse(b);
    va.cmp(&vb)
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
    stdout
        .split_whitespace()
        .find(|s| s.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

pub(crate) async fn fetch_latest_version(client: &reqwest::Client) -> Result<String, FetchError> {
    let resp = client
        .get("https://registry.npmjs.org/@anthropic-ai/claude-code/latest")
        .send()
        .await?;
    let resp = crate::error::check_response(resp).await?;
    let json: serde_json::Value = resp.json().await?;
    json["version"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| FetchError::InvalidResponse("missing version field".into()))
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
    fn compare_major_minor_patch() {
        assert_eq!(
            compare_versions("1.2.3", "1.2.3"),
            std::cmp::Ordering::Equal
        );
        assert_eq!(compare_versions("1.2.3", "1.2.4"), std::cmp::Ordering::Less);
        assert_eq!(
            compare_versions("1.3.0", "1.2.9"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_versions("2.0.0", "1.99.99"),
            std::cmp::Ordering::Greater
        );
    }
}
