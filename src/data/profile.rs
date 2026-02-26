use crate::error::FetchError;

const PROFILE_API_URL: &str = "https://api.anthropic.com/api/oauth/profile";

pub(crate) async fn fetch_profile_email(
    client: &reqwest::Client,
    token: &str,
) -> Result<String, FetchError> {
    let resp = client
        .get(PROFILE_API_URL)
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", super::ANTHROPIC_BETA)
        .send()
        .await?;

    let resp = crate::error::check_response(resp).await?;
    let json: serde_json::Value = resp.json().await?;
    json["account"]["email"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| FetchError::InvalidResponse("missing account.email field".into()))
}
