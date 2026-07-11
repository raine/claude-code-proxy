use sha2::{Digest, Sha256};

use super::constants::{CLIENT_ID, SCOPE};
use super::jwt::TokenResponse;

#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

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

pub fn build_authorize_url(
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> Result<String, anyhow::Error> {
    let mut url = url::Url::parse(&format!("{issuer}/oauth2/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("nonce", &generate_state());
    Ok(url.to_string())
}

pub fn exchange_code_for_tokens(
    issuer: &str,
    code: &str,
    pkce: &PkceCodes,
    redirect_uri: &str,
) -> Result<TokenResponse, anyhow::Error> {
    let client = reqwest::blocking::Client::new();
    // xAI expects code_challenge echoed on token exchange (Hermes/OpenCode).
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", pkce.verifier.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ];

    let resp = client
        .post(format!("{issuer}/oauth2/token"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn generate_pkce_produces_expected_format() {
        let pkce = generate_pkce();
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);
        assert!(!pkce.verifier.contains('+'));
        assert!(!pkce.verifier.contains('/'));
        assert!(!pkce.verifier.contains('='));
    }

    #[test]
    fn challenge_is_s256_of_verifier() {
        let pkce = generate_pkce();
        let expected_hash = Sha256::digest(pkce.verifier.as_bytes());
        let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(expected_hash);
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn authorize_url_contains_xai_params() {
        let pkce = PkceCodes {
            verifier: "v".into(),
            challenge: "c".into(),
        };
        let url = build_authorize_url(
            "https://auth.x.ai",
            "http://127.0.0.1:56121/callback",
            &pkce,
            "state",
        )
        .unwrap();
        assert!(url.starts_with("https://auth.x.ai/oauth2/authorize?"));
        assert!(url.contains("client_id=b1a00492-073a-47ea-816f-4c329264a828"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("grok-cli%3Aaccess") || url.contains("grok-cli:access"));
        assert!(url.contains("api%3Aaccess") || url.contains("api:access"));
        assert!(url.contains("state=state"));
    }
}
