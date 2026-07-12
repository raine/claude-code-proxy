//! Device-code login for headless hosts, using the same public client as browser login.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::login::{CANONICAL_ISSUER, CLIENT_ID, SCOPES};
use super::token_store::{GrokTokenStore, StoredAuth};
use crate::auth::AuthStorage;

const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEVICE_POLL_SAFETY_MARGIN_MS: u64 = 500;
const SLOW_DOWN_BACKOFF_MS: u64 = 2000;
const MAX_DEVICE_POLL_WAIT: Duration = Duration::from_secs(600);

#[derive(Deserialize)]
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

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    #[serde(default)]
    token_type: Option<String>,
}

enum DevicePoll {
    Tokens(TokenResponse),
    Pending,
    SlowDown,
}

pub fn device_login<S: AuthStorage<StoredAuth>>(store: &GrokTokenStore<S>) -> anyhow::Result<()> {
    let client = client()?;
    let tokens = run_device_flow(&client, CANONICAL_ISSUER)?;
    let refresh = tokens
        .refresh_token
        .as_ref()
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Grok device login did not grant an offline session"))?;
    store.save_auth(StoredAuth {
        access: tokens.access_token,
        refresh,
        expires_at_ms: now_ms().saturating_add(tokens.expires_in.saturating_mul(1000)),
        issuer: CANONICAL_ISSUER.into(),
        client_id: CLIENT_ID.into(),
    })?;
    Ok(())
}

fn client() -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?)
}

fn run_device_flow(
    client: &reqwest::blocking::Client,
    issuer: &str,
) -> anyhow::Result<TokenResponse> {
    run_device_flow_inner(client, issuer, &|dur| std::thread::sleep(dur))
}

fn run_device_flow_inner(
    client: &reqwest::blocking::Client,
    issuer: &str,
    sleep: &dyn Fn(Duration),
) -> anyhow::Result<TokenResponse> {
    let auth = request_device_code(client, issuer)?;
    let visit = auth
        .verification_uri_complete
        .clone()
        .or_else(|| auth.verification_uri.clone())
        .unwrap_or_else(|| format!("{issuer}/device"));
    println!(
        "\nOpen this URL on any device to authorize:\n\n  {visit}\n\nand enter the code:  {}\n",
        auth.user_code
    );

    let interval = Duration::from_millis(
        auth.interval.unwrap_or(5).max(1) * 1000 + DEVICE_POLL_SAFETY_MARGIN_MS,
    );
    let max_wait = auth
        .expires_in
        .map(|secs| Duration::from_secs(secs.max(30)))
        .unwrap_or(MAX_DEVICE_POLL_WAIT);
    let deadline = Instant::now() + max_wait;

    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("Grok device login timed out after {}s", max_wait.as_secs());
        }
        match poll_token(client, issuer, &auth.device_code)? {
            DevicePoll::Tokens(tokens) => {
                validate_tokens(&tokens)?;
                return Ok(tokens);
            }
            DevicePoll::Pending => sleep(interval),
            DevicePoll::SlowDown => sleep(interval + Duration::from_millis(SLOW_DOWN_BACKOFF_MS)),
        }
    }
}

fn request_device_code(
    client: &reqwest::blocking::Client,
    issuer: &str,
) -> anyhow::Result<DeviceAuthResponse> {
    let response = client
        .post(format!("{issuer}/oauth2/device/code"))
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPES)])
        .send()?;
    if !response.status().is_success() {
        anyhow::bail!(
            "Grok device authorization failed with status {}",
            response.status()
        );
    }
    Ok(response.json()?)
}

fn poll_token(
    client: &reqwest::blocking::Client,
    issuer: &str,
    device_code: &str,
) -> anyhow::Result<DevicePoll> {
    let response = client
        .post(format!("{issuer}/oauth2/token"))
        .form(&[
            ("grant_type", GRANT_DEVICE_CODE),
            ("device_code", device_code),
            ("client_id", CLIENT_ID),
        ])
        .send()?;
    if response.status().is_success() {
        return Ok(DevicePoll::Tokens(response.json()?));
    }
    let status = response.status();
    let body: serde_json::Value = response.json().unwrap_or_else(|_| serde_json::json!({}));
    match body.get("error").and_then(|value| value.as_str()) {
        Some("authorization_pending") => Ok(DevicePoll::Pending),
        Some("slow_down") => Ok(DevicePoll::SlowDown),
        Some(error) => anyhow::bail!("Grok device login failed: {error}"),
        None => anyhow::bail!("Grok device login failed with status {status}"),
    }
}

fn validate_tokens(tokens: &TokenResponse) -> anyhow::Result<()> {
    if tokens.access_token.is_empty()
        || tokens.expires_in == 0
        || tokens
            .token_type
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
    {
        anyhow::bail!("Grok device token response is invalid");
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// Minimal mock issuer: one response for `/oauth2/device/code`, then a queued
    /// sequence of `(status, body)` responses for `/oauth2/token`.
    fn spawn_issuer(device_body: &str, token_responses: Vec<(u16, String)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let device_body = device_body.to_string();
        thread::spawn(move || {
            let mut token_index = 0usize;
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buffer = [0_u8; 2048];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("");
                let (status, body) = if path.contains("device/code") {
                    (200_u16, device_body.clone())
                } else {
                    let response = token_responses
                        .get(token_index)
                        .cloned()
                        .unwrap_or((200, "{}".into()));
                    token_index += 1;
                    response
                };
                let http = format!(
                    "HTTP/1.1 {status} STATUS\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(http.as_bytes());
                if path.contains("token") && token_index >= token_responses.len() {
                    break;
                }
            }
        });
        base
    }

    fn test_client() -> reqwest::blocking::Client {
        reqwest::blocking::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .unwrap()
    }

    fn no_sleep() -> impl Fn(Duration) {
        |_| {}
    }

    #[test]
    fn device_flow_returns_tokens_after_pending() {
        let issuer = spawn_issuer(
            r#"{"device_code":"dev-1","user_code":"WXYZ-1234","verification_uri":"https://auth.x.ai/device","interval":0}"#,
            vec![
                (400, r#"{"error":"authorization_pending"}"#.into()),
                (400, r#"{"error":"slow_down"}"#.into()),
                (
                    200,
                    r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#
                        .into(),
                ),
            ],
        );
        let tokens = run_device_flow_inner(&test_client(), &issuer, &no_sleep()).unwrap();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
    }

    #[test]
    fn device_flow_reports_denied() {
        let issuer = spawn_issuer(
            r#"{"device_code":"dev-2","user_code":"AAAA-0000","interval":0}"#,
            vec![(400, r#"{"error":"access_denied"}"#.into())],
        );
        let error = run_device_flow_inner(&test_client(), &issuer, &no_sleep()).unwrap_err();
        assert!(error.to_string().contains("access_denied"));
    }

    #[test]
    fn device_flow_reports_init_failure() {
        let issuer = spawn_issuer(r#"{"error":"invalid_client"}"#, vec![]);
        // device/code returns 200 with a body missing required fields -> parse error.
        let error = run_device_flow_inner(&test_client(), &issuer, &no_sleep()).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn device_login_persists_tokens() {
        let issuer = spawn_issuer(
            r#"{"device_code":"dev-3","user_code":"BBBB-1111","interval":0}"#,
            vec![(
                200,
                r#"{"access_token":"access-3","refresh_token":"refresh-3","expires_in":3600,"token_type":"Bearer"}"#
                    .into(),
            )],
        );
        let store = GrokTokenStore::new(InMemoryAuthStore::<StoredAuth>::default());
        let tokens = run_device_flow_inner(&test_client(), &issuer, &no_sleep()).unwrap();
        let refresh = tokens.refresh_token.clone().unwrap();
        store
            .save_auth(StoredAuth {
                access: tokens.access_token,
                refresh,
                expires_at_ms: now_ms() + tokens.expires_in * 1000,
                issuer: CANONICAL_ISSUER.into(),
                client_id: CLIENT_ID.into(),
            })
            .unwrap();
        let saved = store.load_auth().unwrap().unwrap();
        assert_eq!(saved.access, "access-3");
        assert_eq!(saved.refresh, "refresh-3");
        assert_eq!(saved.issuer, CANONICAL_ISSUER);
    }
}
