use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct IdTokenClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
    #[serde(default)]
    organizations: Option<Vec<OrgClaim>>,
    #[allow(dead_code)]
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    #[serde(rename = "https://api.openai.com/auth")]
    openai_auth: Option<OpenAiAuthClaim>,
    #[serde(default)]
    #[serde(rename = "https://api.openai.com/auth.chatgpt_account_id")]
    openai_chatgpt_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrgClaim {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiAuthClaim {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub id_token: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: Option<u64>,
}

fn parse_jwt_claims(token: &str) -> Option<IdTokenClaims> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload_b64 = parts[1].replace('-', "+").replace('_', "/");
    let padded = match payload_b64.len() % 4 {
        2 => format!("{payload_b64}=="),
        3 => format!("{payload_b64}="),
        _ => payload_b64,
    };
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&padded)
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn extract_account_id_from_claims(claims: &IdTokenClaims) -> Option<String> {
    claims
        .chatgpt_account_id
        .clone()
        .or_else(|| claims.openai_auth.as_ref()?.chatgpt_account_id.clone())
        .or_else(|| claims.openai_chatgpt_account_id.clone())
        .or_else(|| claims.organizations.as_ref()?.first()?.id.clone().into())
}

pub fn validate_token_response(tokens: &TokenResponse) -> anyhow::Result<()> {
    if tokens.access_token.trim().is_empty() {
        anyhow::bail!("token response missing access token");
    }
    if tokens.refresh_token.trim().is_empty() {
        anyhow::bail!("token response missing refresh token");
    }
    if matches!(tokens.expires_in, Some(0)) {
        anyhow::bail!("token response has invalid expiration");
    }
    Ok(())
}

pub fn extract_account_id(tokens: &TokenResponse) -> Option<String> {
    if let Some(ref id_token) = tokens.id_token
        && let Some(claims) = parse_jwt_claims(id_token)
        && let Some(account_id) = extract_account_id_from_claims(&claims)
    {
        return Some(account_id);
    }
    let claims = parse_jwt_claims(&tokens.access_token)?;
    extract_account_id_from_claims(&claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use serde_json::json;

    fn token_with_claims(claims: serde_json::Value) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        format!("header.{payload}.signature")
    }

    #[test]
    fn extract_account_id_from_access_token() {
        let token = TokenResponse {
            id_token: None,
            access_token: "eyJhbGciOiJIUzI1NiJ9.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0XzEyMyJ9.sig"
                .into(),
            refresh_token: "r".into(),
            expires_in: Some(3600),
        };
        assert_eq!(extract_account_id(&token), Some("acct_123".into()));
    }

    #[test]
    fn extract_account_id_from_id_token_takes_precedence() {
        let token = TokenResponse {
            id_token: Some(
                "eyJhbGciOiJIUzI1NiJ9.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJpZF9hY2N0In0.sig".into(),
            ),
            access_token: "eyJhbGciOiJIUzI1NiJ9.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NfYWNjIn0.sig"
                .into(),
            refresh_token: "r".into(),
            expires_in: Some(3600),
        };
        assert_eq!(extract_account_id(&token), Some("id_acct".into()));
    }

    #[test]
    fn invalid_id_token_email_falls_back_to_access_token() {
        let token = TokenResponse {
            id_token: Some(token_with_claims(json!({
                "chatgpt_account_id": "id_acct",
                "email": 123,
            }))),
            access_token: token_with_claims(json!({
                "chatgpt_account_id": "access_acct",
            })),
            refresh_token: "r".into(),
            expires_in: Some(3600),
        };

        assert_eq!(extract_account_id(&token), Some("access_acct".into()));
    }

    #[test]
    fn extract_account_id_returns_none_for_invalid_token() {
        let token = TokenResponse {
            id_token: None,
            access_token: "invalid".into(),
            refresh_token: "r".into(),
            expires_in: None,
        };
        assert_eq!(extract_account_id(&token), None);
    }

    #[test]
    fn validate_token_response_rejects_empty_access_token() {
        let token = TokenResponse {
            access_token: "".into(),
            refresh_token: "r".into(),
            expires_in: Some(3600),
            id_token: None,
        };
        assert!(validate_token_response(&token).is_err());
        assert!(
            validate_token_response(&token)
                .unwrap_err()
                .to_string()
                .contains("missing access token")
        );
    }

    #[test]
    fn validate_token_response_rejects_empty_refresh_token() {
        let token = TokenResponse {
            access_token: "a".into(),
            refresh_token: "".into(),
            expires_in: Some(3600),
            id_token: None,
        };
        assert!(validate_token_response(&token).is_err());
        assert!(
            validate_token_response(&token)
                .unwrap_err()
                .to_string()
                .contains("missing refresh token")
        );
    }

    #[test]
    fn validate_token_response_rejects_zero_expires_in() {
        let token = TokenResponse {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: Some(0),
            id_token: None,
        };
        assert!(validate_token_response(&token).is_err());
        assert!(
            validate_token_response(&token)
                .unwrap_err()
                .to_string()
                .contains("invalid expiration")
        );
    }

    #[test]
    fn validate_token_response_accepts_valid() {
        let token = TokenResponse {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: Some(3600),
            id_token: None,
        };
        assert!(validate_token_response(&token).is_ok());
    }

    #[test]
    fn validate_token_response_accepts_no_expires_in() {
        let token = TokenResponse {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: None,
            id_token: None,
        };
        assert!(validate_token_response(&token).is_ok());
    }
}
