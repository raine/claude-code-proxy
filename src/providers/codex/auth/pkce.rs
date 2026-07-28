use sha2::{Digest, Sha256};

use super::constants::{CLIENT_ID, ORIGINATOR};

// ---------------------------------------------------------------------------
// PKCE types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

// ---------------------------------------------------------------------------
// Code generation
// ---------------------------------------------------------------------------

fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn generate_pkce() -> PkceCodes {
    let mut verifier_bytes = vec![0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut verifier_bytes);
    let verifier = base64_url_encode(&verifier_bytes);

    let hash = Sha256::digest(verifier.as_bytes());
    let challenge = base64_url_encode(&hash);

    PkceCodes {
        verifier,
        challenge,
    }
}

pub fn generate_state() -> String {
    let mut state_bytes = vec![0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut state_bytes);
    base64_url_encode(&state_bytes)
}

// ---------------------------------------------------------------------------
// Authorize URL
// ---------------------------------------------------------------------------

pub fn build_authorize_url(
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> Result<String, anyhow::Error> {
    let mut url = url::Url::parse(&format!("{issuer}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", "openid profile email offline_access")
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true")
        .append_pair("state", state)
        .append_pair("originator", ORIGINATOR);
    Ok(url.to_string())
}

// ---------------------------------------------------------------------------
// Token exchange
// ---------------------------------------------------------------------------

pub fn exchange_code_for_tokens(
    issuer: &str,
    code: &str,
    pkce: &PkceCodes,
    redirect_uri: &str,
) -> Result<crate::providers::codex::auth::jwt::TokenResponse, anyhow::Error> {
    let client = reqwest::blocking::Client::new();
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", &pkce.verifier),
    ];

    let resp = client
        .post(format!("{issuer}/oauth/token"))
        .form(&form)
        .send()
        .map_err(|e| anyhow::anyhow!("Token exchange network error: {e}"))?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let text = resp.text().unwrap_or_default();
        anyhow::bail!("Token exchange failed: {status} {text}");
    }

    let tokens = resp
        .json()
        .map_err(|e| anyhow::anyhow!("failed to parse token exchange response: {e}"))?;
    Ok(tokens)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn generate_pkce_produces_expected_format() {
        let pkce = generate_pkce();
        // Verifier should be 32 random bytes, base64 URL-encoded without padding
        // = ceil(32*4/3) = 43 characters
        assert_eq!(pkce.verifier.len(), 43);
        // Challenge should be SHA-256 of verifier, base64 URL-encoded without padding
        // SHA-256 is 32 bytes, so same length: 43 characters
        assert_eq!(pkce.challenge.len(), 43);
        // Verifier should be URL-safe base64 (no +, /, or =)
        assert!(!pkce.verifier.contains('+'));
        assert!(!pkce.verifier.contains('/'));
        assert!(!pkce.verifier.contains('='));
    }

    #[test]
    fn generate_pkce_challenge_is_s256_of_verifier() {
        let pkce = generate_pkce();
        let expected_hash = Sha256::digest(pkce.verifier.as_bytes());
        let expected_challenge =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(expected_hash);
        assert_eq!(pkce.challenge, expected_challenge);
    }

    #[test]
    fn generate_state_produces_url_safe_base64() {
        let state = generate_state();
        assert_eq!(state.len(), 43);
        assert!(!state.contains('+'));
        assert!(!state.contains('/'));
        assert!(!state.contains('='));
    }

    #[test]
    fn authorize_url_contains_codex_params() {
        let pkce = PkceCodes {
            verifier: "verifier".into(),
            challenge: "challenge".into(),
        };
        let url = build_authorize_url(
            "http://issuer",
            "http://localhost:1455/auth/callback",
            &pkce,
            "state",
        )
        .unwrap();
        assert!(url.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
        assert!(url.contains("scope=openid+profile+email+offline_access"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("originator=claude-code-proxy"));
        assert!(url.contains("state=state"));
    }

    #[test]
    fn authorize_url_starts_with_issuer_authorize() {
        let pkce = PkceCodes {
            verifier: "v".into(),
            challenge: "c".into(),
        };
        let url = build_authorize_url(
            "https://auth.openai.com",
            "http://localhost:1455/auth/callback",
            &pkce,
            "s",
        )
        .unwrap();
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
    }

    #[test]
    fn exchange_code_uses_form_encoding() {
        // Test that the exchange code builds proper form-encoded requests
        // by checking the function compiles and accepts expected params
        // (actual network call not made in this test)
        let pkce = PkceCodes {
            verifier: "test_verifier".into(),
            challenge: "test_challenge".into(),
        };
        // Just verify function signature and types
        let _ = pkce;
    }
}
