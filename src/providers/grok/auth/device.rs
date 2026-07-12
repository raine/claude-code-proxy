//! Device-code login for headless hosts, using the same public client as browser login.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::login::{CANONICAL_ISSUER, CLIENT_ID, SCOPES};
use super::token_store::{GrokTokenStore, StoredAuth};
use crate::auth::AuthStorage;

const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);
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

trait DeviceRuntime {
    fn monotonic_now(&self) -> Instant;
    fn unix_time_ms(&self) -> u64;
    fn sleep(&self, duration: Duration);
}

struct SystemRuntime;

impl DeviceRuntime for SystemRuntime {
    fn monotonic_now(&self) -> Instant {
        Instant::now()
    }

    fn unix_time_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

pub fn device_login<S: AuthStorage<StoredAuth>>(store: &GrokTokenStore<S>) -> anyhow::Result<()> {
    let client = client()?;
    device_login_inner(store, &client, CANONICAL_ISSUER, &SystemRuntime)
}

fn device_login_inner<S: AuthStorage<StoredAuth>>(
    store: &GrokTokenStore<S>,
    client: &reqwest::blocking::Client,
    issuer: &str,
    runtime: &dyn DeviceRuntime,
) -> anyhow::Result<()> {
    let tokens = run_device_flow_inner(client, issuer, runtime)?;
    let refresh = tokens
        .refresh_token
        .as_ref()
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Grok device login did not grant an offline session"))?;
    store.save_auth(StoredAuth {
        access: tokens.access_token,
        refresh,
        expires_at_ms: runtime
            .unix_time_ms()
            .saturating_add(tokens.expires_in.saturating_mul(1000)),
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

fn run_device_flow_inner(
    client: &reqwest::blocking::Client,
    issuer: &str,
    runtime: &dyn DeviceRuntime,
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

    let mut interval = auth
        .interval
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_POLL_INTERVAL);
    let max_wait = auth
        .expires_in
        .map(Duration::from_secs)
        .unwrap_or(MAX_DEVICE_POLL_WAIT);
    let deadline = runtime
        .monotonic_now()
        .checked_add(max_wait)
        .ok_or_else(|| anyhow::anyhow!("Grok device code lifetime is too large"))?;

    loop {
        let remaining = deadline.saturating_duration_since(runtime.monotonic_now());
        if remaining.is_zero() {
            anyhow::bail!("Grok device login timed out after {}s", max_wait.as_secs());
        }
        runtime.sleep(interval.min(remaining));
        if runtime.monotonic_now() >= deadline {
            anyhow::bail!("Grok device login timed out after {}s", max_wait.as_secs());
        }

        match poll_token(client, issuer, &auth.device_code)? {
            DevicePoll::Tokens(tokens) => {
                validate_tokens(&tokens)?;
                return Ok(tokens);
            }
            DevicePoll::Pending => {}
            DevicePoll::SlowDown => {
                interval = interval.saturating_add(SLOW_DOWN_INCREMENT);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::thread;

    const TEST_UNIX_TIME_MS: u64 = 1_700_000_000_000;

    struct TestRuntime {
        start: Instant,
        elapsed: Mutex<Duration>,
        sleeps: Mutex<Vec<Duration>>,
    }

    impl Default for TestRuntime {
        fn default() -> Self {
            Self {
                start: Instant::now(),
                elapsed: Mutex::new(Duration::ZERO),
                sleeps: Mutex::new(Vec::new()),
            }
        }
    }

    impl DeviceRuntime for TestRuntime {
        fn monotonic_now(&self) -> Instant {
            self.start + *self.elapsed.lock().unwrap()
        }

        fn unix_time_ms(&self) -> u64 {
            TEST_UNIX_TIME_MS
        }

        fn sleep(&self, duration: Duration) {
            self.sleeps.lock().unwrap().push(duration);
            *self.elapsed.lock().unwrap() += duration;
        }
    }

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
        let tokens =
            run_device_flow_inner(&test_client(), &issuer, &TestRuntime::default()).unwrap();
        assert_eq!(tokens.access_token, "access-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("refresh-1"));
    }

    #[test]
    fn device_flow_reports_denied() {
        let issuer = spawn_issuer(
            r#"{"device_code":"dev-2","user_code":"AAAA-0000","interval":0}"#,
            vec![(400, r#"{"error":"access_denied"}"#.into())],
        );
        let error =
            run_device_flow_inner(&test_client(), &issuer, &TestRuntime::default()).unwrap_err();
        assert!(error.to_string().contains("access_denied"));
    }

    #[test]
    fn device_flow_reports_init_failure() {
        let issuer = spawn_issuer(r#"{"error":"invalid_client"}"#, vec![]);
        // device/code returns 200 with a body missing required fields -> parse error.
        let error =
            run_device_flow_inner(&test_client(), &issuer, &TestRuntime::default()).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn device_flow_waits_before_polling_and_persists_slow_down() {
        let issuer = spawn_issuer(
            r#"{"device_code":"dev-4","user_code":"CCCC-2222","interval":2}"#,
            vec![
                (400, r#"{"error":"authorization_pending"}"#.into()),
                (400, r#"{"error":"slow_down"}"#.into()),
                (400, r#"{"error":"authorization_pending"}"#.into()),
                (
                    200,
                    r#"{"access_token":"access-4","refresh_token":"refresh-4","expires_in":3600,"token_type":"Bearer"}"#
                        .into(),
                ),
            ],
        );
        let runtime = TestRuntime::default();
        run_device_flow_inner(&test_client(), &issuer, &runtime).unwrap();
        assert_eq!(
            *runtime.sleeps.lock().unwrap(),
            vec![
                Duration::from_secs(2),
                Duration::from_secs(2),
                Duration::from_secs(7),
                Duration::from_secs(7),
            ]
        );
    }

    #[test]
    fn device_flow_respects_short_expiration() {
        let issuer = spawn_issuer(
            r#"{"device_code":"dev-5","user_code":"DDDD-3333","interval":5,"expires_in":2}"#,
            vec![(200, r#"{}"#.into())],
        );
        let runtime = TestRuntime::default();
        let error = run_device_flow_inner(&test_client(), &issuer, &runtime).unwrap_err();
        assert!(error.to_string().contains("timed out after 2s"));
        assert_eq!(
            *runtime.sleeps.lock().unwrap(),
            vec![Duration::from_secs(2)]
        );
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
        let runtime = TestRuntime::default();
        device_login_inner(&store, &test_client(), &issuer, &runtime).unwrap();
        let saved = store.load_auth().unwrap().unwrap();
        assert_eq!(saved.access, "access-3");
        assert_eq!(saved.refresh, "refresh-3");
        assert_eq!(saved.expires_at_ms, TEST_UNIX_TIME_MS + 3_600_000);
        assert_eq!(saved.issuer, CANONICAL_ISSUER);
        assert_eq!(saved.client_id, CLIENT_ID);
    }
}
