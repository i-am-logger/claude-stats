use crate::error::FetchError;

pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SelfVersion {
    pub(crate) latest: Option<String>,
}

impl SelfVersion {
    pub(crate) fn is_outdated(&self) -> bool {
        match &self.latest {
            Some(latest) if latest != CURRENT_VERSION => {
                crate::fmt::compare_versions(CURRENT_VERSION, latest) == std::cmp::Ordering::Less
            }
            _ => false,
        }
    }
}

/// Strip an optional `v` prefix from a release tag (e.g. "v1.2.0" → "1.2.0").
fn parse_tag(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

pub(crate) async fn fetch_latest_version(client: &reqwest::Client) -> Result<String, FetchError> {
    let resp = client
        .get("https://api.github.com/repos/i-am-logger/claude-stats/releases/latest")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        let is_rate_limit = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            == Some("0");
        if is_rate_limit {
            tracing::warn!("GitHub API rate limit exceeded, will retry later");
        }
    }
    let resp = crate::error::check_response(resp).await?;
    let json: serde_json::Value = resp.json().await?;
    let tag = json["tag_name"]
        .as_str()
        .ok_or_else(|| FetchError::InvalidResponse("missing tag_name field".into()))?;
    Ok(parse_tag(tag).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tag_bare_version() {
        assert_eq!(parse_tag("1.2.0"), "1.2.0");
    }

    #[test]
    fn parse_tag_v_prefix() {
        assert_eq!(parse_tag("v1.2.0"), "1.2.0");
    }

    #[test]
    fn parse_tag_empty() {
        assert_eq!(parse_tag(""), "");
    }

    #[test]
    fn parse_tag_v_only() {
        assert_eq!(parse_tag("v"), "");
    }

    #[test]
    fn parse_tag_uppercase_v_not_stripped() {
        assert_eq!(parse_tag("V1.2.0"), "V1.2.0");
    }

    #[test]
    fn outdated_when_latest_is_newer() {
        let v = SelfVersion {
            latest: Some("99.99.99".into()),
        };
        assert!(v.is_outdated());
    }

    #[test]
    fn not_outdated_when_same() {
        let v = SelfVersion {
            latest: Some(CURRENT_VERSION.into()),
        };
        assert!(!v.is_outdated());
    }

    #[test]
    fn not_outdated_when_older() {
        let v = SelfVersion {
            latest: Some("0.0.1".into()),
        };
        assert!(!v.is_outdated());
    }

    #[test]
    fn not_outdated_when_none() {
        let v = SelfVersion::default();
        assert!(!v.is_outdated());
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn prop_is_outdated_never_panics(latest in proptest::option::of("\\PC{0,30}")) {
                let v = SelfVersion { latest };
                let _ = v.is_outdated();
            }

            #[test]
            fn prop_parse_tag_never_panics(tag in "\\PC{0,50}") {
                let _ = parse_tag(&tag);
            }

            #[test]
            fn prop_parse_tag_strips_v_prefix(version in "[0-9]{1,3}\\.[0-9]{1,3}\\.[0-9]{1,3}") {
                let tagged = format!("v{version}");
                assert_eq!(parse_tag(&tagged), version.as_str());
                assert_eq!(parse_tag(&version), version.as_str());
            }
        }
    }
}
