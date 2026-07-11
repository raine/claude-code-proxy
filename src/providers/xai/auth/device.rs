use std::time::{Duration, Instant};

use super::constants::{CLIENT_ID, DEVICE_POLL_SAFETY_MARGIN_MS, GRANT_DEVICE_CODE, SCOPE, issuer};
use super::jwt::TokenResponse;

const MAX_DEVICE_POLL_WAIT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, serde::Deserialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    #[serde(default)]
    verification_uri: Option<String>,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    interval: Option<u64>,
}

pub fn run_device_login() -> Result<TokenResponse, anyhow::Error> {
    run_device_login_with_issuer(&issuer())
}

pub fn run_device_login_with_issuer(issuer_url: &str) -> Result<TokenResponse, anyhow::Error> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    let init_resp = client
        .post(format!("{issuer_url}/oauth2/device/code"))
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .map_err(|e| anyhow::anyhow!("Device authorization network error: {e}"))?;

    if !init_resp.status().is_success() {
        let status = init_resp.status().as_u16();
        let text = init_resp.text().unwrap_or_default();
        anyhow::bail!("Device authorization failed: {status} {text}");
    }

    let auth: DeviceAuthResponse = init_resp
        .json()
        .map_err(|e| anyhow::anyhow!("failed to parse device authorization response: {e}"))?;

    let visit = auth
        .verification_uri_complete
        .or(auth.verification_uri)
        .unwrap_or_else(|| format!("{issuer_url}/device"));

    eprintln!();
    eprintln!("Visit: {visit}");
    eprintln!("Code:  {}", auth.user_code);
    eprintln!();

    let interval_ms = (auth.interval.unwrap_or(5).max(1)) * 1000 + DEVICE_POLL_SAFETY_MARGIN_MS;
    let max_wait = auth
        .expires_in
        .map(|s| Duration::from_secs(s.max(30)))
        .unwrap_or(MAX_DEVICE_POLL_WAIT);
    let deadline = Instant::now() + max_wait;

    loop {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "Device auth timed out after {} seconds",
                max_wait.as_secs()
            );
        }

        let resp = client
            .post(format!("{issuer_url}/oauth2/token"))
            .form(&[
                ("grant_type", GRANT_DEVICE_CODE),
                ("device_code", auth.device_code.as_str()),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .map_err(|e| anyhow::anyhow!("Device token poll network error: {e}"))?;

        let status = resp.status().as_u16();
        if status == 200 {
            let tokens: TokenResponse = resp
                .json()
                .map_err(|e| anyhow::anyhow!("failed to parse token response: {e}"))?;
            if tokens.access_token.trim().is_empty() {
                anyhow::bail!("xAI device-code response missing access_token");
            }
            if tokens
                .refresh_token
                .as_ref()
                .map(|r| r.trim().is_empty())
                .unwrap_or(true)
            {
                anyhow::bail!("xAI device-code response missing refresh_token");
            }
            return Ok(tokens);
        }

        let body: serde_json::Value = resp.json().unwrap_or_else(|_| serde_json::json!({}));
        let error = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        match error {
            "authorization_pending" => {
                std::thread::sleep(Duration::from_millis(interval_ms));
            }
            "slow_down" => {
                std::thread::sleep(Duration::from_millis(interval_ms + 2000));
            }
            "expired_token" | "access_denied" => {
                anyhow::bail!("Device authorization failed: {error}");
            }
            _ if status == 400 || status == 401 || status == 403 => {
                anyhow::bail!("Device authorization failed: {status} {error}");
            }
            _ => {
                // Treat other non-success as pending-ish unless clearly fatal
                std::thread::sleep(Duration::from_millis(interval_ms));
            }
        }
    }
}
