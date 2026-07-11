use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
}

pub fn validate_token_response(tokens: &TokenResponse) -> anyhow::Result<()> {
    if tokens.access_token.trim().is_empty() {
        anyhow::bail!("token response missing access token");
    }
    if matches!(tokens.expires_in, Some(0)) {
        anyhow::bail!("token response has invalid expiration");
    }
    Ok(())
}

/// Prefer rotated refresh token when present.
pub fn refresh_token_from(tokens: &TokenResponse, current: &str) -> String {
    tokens
        .refresh_token
        .clone()
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| current.to_string())
}

/// Absolute expiry in unix ms. Prefer `expires_in`, else JWT `exp`, else now+1h.
pub fn expires_at_ms(tokens: &TokenResponse, now_ms: u64) -> u64 {
    if let Some(secs) = tokens.expires_in.filter(|s| *s > 0) {
        return now_ms.saturating_add(secs.saturating_mul(1000));
    }
    if let Some(exp_ms) = jwt_exp_ms(&tokens.access_token) {
        return exp_ms;
    }
    now_ms.saturating_add(3600 * 1000)
}

pub fn jwt_exp_ms(token: &str) -> Option<u64> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| {
            let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
            base64::engine::general_purpose::URL_SAFE.decode(padded)
        })
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    claims.get("exp")?.as_u64().map(|exp| exp * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn test_jwt_with_exp(exp: u64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn validate_accepts_valid() {
        let t = TokenResponse {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
            id_token: None,
            scope: None,
            token_type: None,
        };
        assert!(validate_token_response(&t).is_ok());
    }

    #[test]
    fn validate_rejects_empty_access() {
        let t = TokenResponse {
            access_token: "".into(),
            refresh_token: Some("r".into()),
            expires_in: Some(3600),
            id_token: None,
            scope: None,
            token_type: None,
        };
        assert!(validate_token_response(&t).is_err());
    }

    #[test]
    fn expires_prefers_expires_in() {
        let t = TokenResponse {
            access_token: test_jwt_with_exp(9_999_999),
            refresh_token: Some("r".into()),
            expires_in: Some(120),
            id_token: None,
            scope: None,
            token_type: None,
        };
        assert_eq!(expires_at_ms(&t, 1_000_000), 1_000_000 + 120_000);
    }

    #[test]
    fn expires_falls_back_to_jwt_exp() {
        let t = TokenResponse {
            access_token: test_jwt_with_exp(5000),
            refresh_token: Some("r".into()),
            expires_in: None,
            id_token: None,
            scope: None,
            token_type: None,
        };
        assert_eq!(expires_at_ms(&t, 0), 5_000_000);
    }
}
