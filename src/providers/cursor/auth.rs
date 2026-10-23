use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::auth::{AuthStorage, KeychainFileAuthStore, SystemKeychain};
use crate::{config, paths};

pub const KEYCHAIN_SERVICE: &str = "claude-code-proxy.cursor";
pub const KEYCHAIN_ACCOUNT: &str = "auth";

const REFRESH_EXPIRY_SKEW_MS: u64 = 60_000;
const CURSOR_WEBSITE_URL: &str = "https://cursor.com";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct StoredCursorAuth {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub api_key: Option<String>,
    pub expires: Option<u64>,
    pub user_id: Option<String>,
    pub email: Option<String>,
    pub source: String,
}

pub type DefaultCursorAuthStore = KeychainFileAuthStore<StoredCursorAuth, SystemKeychain>;

pub struct CursorTokenStore<S: AuthStorage<StoredCursorAuth>> {
    store: S,
}

impl<S: AuthStorage<StoredCursorAuth>> CursorTokenStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn load_auth(&self) -> anyhow::Result<Option<CursorAuth>> {
        let Some(stored) = self.store.load()? else {
            return Ok(None);
        };
        if stored.access_token.trim().is_empty() {
            return Ok(None);
        }
        let auth = enrich(stored, self.auth_path());
        self.refresh_if_needed(auth)
    }

    pub fn save_auth(&self, auth: StoredCursorAuth) -> anyhow::Result<CursorAuth> {
        if auth.access_token.trim().is_empty() {
            anyhow::bail!("Cursor auth accessToken is required");
        }
        self.store.save(auth.clone())?;
        Ok(enrich(auth, self.auth_path()))
    }

    pub fn clear_auth(&self) -> anyhow::Result<()> {
        self.store.clear()
    }

    pub fn auth_path(&self) -> String {
        self.store.path()
    }

    fn refresh_if_needed(&self, auth: CursorAuth) -> anyhow::Result<Option<CursorAuth>> {
        let Some(refresh_token) = auth.refresh_token.as_deref() else {
            return Ok(Some(auth));
        };
        let Some(expires) = auth.expires else {
            return Ok(Some(auth));
        };
        if expires > now_ms() + REFRESH_EXPIRY_SKEW_MS {
            return Ok(Some(auth));
        }

        let Some(refreshed) = refresh_cursor_auth(refresh_token)? else {
            return Ok(Some(auth));
        };
        self.save_auth(StoredCursorAuth {
            access_token: refreshed.access_token,
            refresh_token: Some(refreshed.refresh_token),
            api_key: auth.api_key,
        })
        .map(Some)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorLogin {
    pub login_url: String,
    pub uuid: String,
    pub verifier: String,
}

#[derive(Debug, Deserialize)]
pub struct RefreshResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "refreshToken")]
    refresh_token: String,
}

pub fn file_store() -> CursorTokenStore<DefaultCursorAuthStore> {
    let primary = paths::provider_auth_file("cursor");
    let legacy = paths::provider_legacy_auth_file("cursor");
    CursorTokenStore::new(KeychainFileAuthStore::new(
        primary.to_string_lossy().to_string(),
        legacy.to_string_lossy().to_string(),
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        use_macos_keychain(),
        SystemKeychain,
    ))
}

pub fn load_cursor_auth() -> anyhow::Result<Option<CursorAuth>> {
    if let Some(token) = env_cursor_token() {
        return Ok(Some(enrich(
            StoredCursorAuth {
                access_token: token,
                refresh_token: None,
                api_key: None,
            },
            "environment".to_string(),
        )));
    }
    file_store().load_auth()
}

/// Load only the bearer token for call sites that do not need auth metadata.
pub fn load_cursor_token() -> Option<String> {
    load_cursor_auth()
        .ok()
        .flatten()
        .map(|auth| auth.access_token)
}

pub fn save_cursor_auth(auth: StoredCursorAuth) -> anyhow::Result<CursorAuth> {
    file_store().save_auth(auth)
}

pub fn clear_cursor_auth() -> anyhow::Result<()> {
    file_store().clear_auth()
}

pub fn cursor_auth_location() -> String {
    file_store().auth_path()
}

pub fn missing_auth_message() -> String {
    [
        "Cursor authentication was not found.",
        "Run `claude-code-proxy cursor auth login`, or set CCP_CURSOR_AUTH_TOKEN/CURSOR_AUTH_TOKEN.",
        "The proxy stores Cursor credentials in its own claude-code-proxy.cursor storage, not Cursor Agent's Keychain/auth.json.",
    ]
    .join(" ")
}

pub fn expired_auth_message(auth: &CursorAuth) -> String {
    let expires = auth
        .expires
        .map(format_unix_ms)
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "Cursor access token from {} is expired or near expiry ({}). Run `claude-code-proxy cursor auth login` again or set CCP_CURSOR_AUTH_TOKEN.",
        auth.source, expires
    )
}

pub fn create_cursor_login() -> CursorLogin {
    let verifier = random_base64_url(32);
    let challenge = base64_url(Sha256::digest(verifier.as_bytes()).as_ref());
    let uuid = uuid::Uuid::new_v4().to_string();
    let login_url = format!(
        "{CURSOR_WEBSITE_URL}/loginDeepControl?challenge={challenge}&uuid={uuid}&mode=login&redirectTarget=cli"
    );
    CursorLogin {
        login_url,
        uuid,
        verifier,
    }
}

pub fn run_cursor_login() -> anyhow::Result<Option<CursorAuth>> {
    let login = create_cursor_login();
    println!("Open this URL to authenticate with Cursor:");
    println!("{}", login.login_url);
    println!();
    if let Err(err) = open_cursor_login_url(
        &login.login_url,
        std::env::consts::OS,
        std::process::Command::status,
    ) {
        println!("Could not open browser automatically: {err}");
    }
    println!("Waiting for Cursor login...");
    let result = wait_for_cursor_login(&login, 150, |attempt| {
        if attempt > 0 && attempt % 10 == 0 {
            print!(".");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    })?;
    let Some(result) = result else {
        return Ok(None);
    };
    save_cursor_auth(StoredCursorAuth {
        access_token: result.access_token,
        refresh_token: Some(result.refresh_token),
        api_key: None,
    })
    .map(Some)
}

pub fn wait_for_cursor_login(
    login: &CursorLogin,
    max_attempts: usize,
    mut on_progress: impl FnMut(usize),
) -> anyhow::Result<Option<RefreshResponse>> {
    let client = reqwest::blocking::Client::new();
    let base = config::cursor_base_url().trim_end_matches('/').to_string();
    let mut consecutive_errors = 0usize;

    for attempt in 0..max_attempts {
        let delay =
            Duration::from_millis((1000.0 * 1.2_f64.powi(attempt as i32)).min(10_000.0) as u64);
        let url = format!(
            "{base}/auth/poll?uuid={}&verifier={}",
            login.uuid, login.verifier
        );
        match client
            .get(url)
            .header("content-type", "application/json")
            .send()
        {
            Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {
                consecutive_errors = 0;
                on_progress(attempt);
                std::thread::sleep(delay);
            }
            Ok(resp) if resp.status().is_success() => {
                let parsed: serde_json::Value = resp.json()?;
                return Ok(parse_cursor_auth_tokens(&parsed));
            }
            Ok(_) | Err(_) => {
                consecutive_errors += 1;
                if consecutive_errors >= 3 {
                    return Ok(None);
                }
                std::thread::sleep(delay);
            }
        }
    }
    Ok(None)
}

fn refresh_cursor_auth(refresh_token: &str) -> anyhow::Result<Option<RefreshResponse>> {
    let client = reqwest::blocking::Client::new();
    let url = format!(
        "{}/auth/refresh",
        config::cursor_base_url().trim_end_matches('/')
    );
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .bearer_auth(refresh_token)
        .body("{}")
        .send()?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let parsed: serde_json::Value = resp.json()?;
    Ok(parse_cursor_auth_tokens(&parsed))
}

fn parse_cursor_auth_tokens(parsed: &serde_json::Value) -> Option<RefreshResponse> {
    Some(RefreshResponse {
        access_token: parsed.get("accessToken")?.as_str()?.to_string(),
        refresh_token: parsed.get("refreshToken")?.as_str()?.to_string(),
    })
}

fn env_cursor_token() -> Option<String> {
    env_cursor_token_from(|key| std::env::var(key).ok())
}

fn env_cursor_token_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    get("CCP_CURSOR_AUTH_TOKEN")
        .filter(|token| !token.trim().is_empty())
        .or_else(|| get("CURSOR_AUTH_TOKEN").filter(|token| !token.trim().is_empty()))
}

fn enrich(stored: StoredCursorAuth, source: String) -> CursorAuth {
    let claims = parse_jwt_claims(&stored.access_token);
    CursorAuth {
        expires: token_expiry_ms(&stored.access_token),
        user_id: claims
            .as_ref()
            .and_then(|claims| claims.get("sub"))
            .and_then(|sub| sub.as_str())
            .map(str::to_string),
        email: claims
            .as_ref()
            .and_then(|claims| claims.get("email"))
            .and_then(|email| email.as_str())
            .map(str::to_string),
        source,
        access_token: stored.access_token,
        refresh_token: stored.refresh_token,
        api_key: stored.api_key,
    }
}

fn token_expiry_ms(token: &str) -> Option<u64> {
    parse_jwt_claims(token)?
        .get("exp")?
        .as_u64()
        .map(|exp| exp * 1000)
}

fn parse_jwt_claims(token: &str) -> Option<serde_json::Value> {
    let mut parts = token.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| {
            let padded = format!("{payload}{}", "=".repeat((4 - payload.len() % 4) % 4));
            base64::engine::general_purpose::URL_SAFE.decode(padded)
        })
        .ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn open_cursor_login_url(
    url: &str,
    platform: &str,
    execute: impl FnOnce(&mut std::process::Command) -> std::io::Result<std::process::ExitStatus>,
) -> anyhow::Result<()> {
    let command = if platform == "macos" {
        ("open", vec![url])
    } else if platform == "windows" {
        windows_browser_command(url)
    } else {
        ("xdg-open", vec![url])
    };
    let status = execute(std::process::Command::new(command.0).args(command.1))?;
    if !status.success() {
        anyhow::bail!("open command exited with {status}");
    }
    Ok(())
}

fn windows_browser_command(url: &str) -> (&str, Vec<&str>) {
    // Keep URL metacharacters out of a command interpreter.
    ("rundll32.exe", vec!["url.dll,FileProtocolHandler", url])
}

#[cfg(test)]
use open_cursor_login_url as open_login_url;

// Run the shared assertions against each provider's private launcher.
#[cfg(test)]
#[allow(clippy::duplicate_mod)]
#[path = "../../../tests/support/oauth_browser_launch.rs"]
mod browser_launch_tests;

fn random_base64_url(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64_url(&bytes)
}

fn base64_url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn use_macos_keychain() -> bool {
    cfg!(target_os = "macos") && std::env::var_os("CCP_CONFIG_DIR").is_none()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn format_unix_ms(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(ts) => ts
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| ms.to_string()),
        Err(_) => ms.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::InMemoryAuthStore;

    #[test]
    fn auth_uses_cursor_auth_token_env() {
        let token = env_cursor_token_from(|key| match key {
            "CURSOR_AUTH_TOKEN" => Some("tok_from_cursor".into()),
            _ => None,
        });
        assert_eq!(token.as_deref(), Some("tok_from_cursor"));
    }

    #[test]
    fn auth_prioritizes_ccp_env_over_cursor_env() {
        let token = env_cursor_token_from(|key| match key {
            "CCP_CURSOR_AUTH_TOKEN" => Some("tok_ccp".into()),
            "CURSOR_AUTH_TOKEN" => Some("tok_cursor".into()),
            _ => None,
        });
        assert_eq!(token.as_deref(), Some("tok_ccp"));
    }

    #[test]
    fn auth_returns_none_when_not_set() {
        assert!(env_cursor_token_from(|_| None).is_none());
    }

    #[test]
    fn stored_auth_uses_camel_case_fields() {
        let auth: StoredCursorAuth = serde_json::from_value(serde_json::json!({
            "accessToken": "access",
            "refreshToken": "refresh",
            "apiKey": "api"
        }))
        .unwrap();
        assert_eq!(auth.access_token, "access");
        assert_eq!(auth.refresh_token.as_deref(), Some("refresh"));

        let value = serde_json::to_value(auth).unwrap();
        assert_eq!(value["accessToken"], "access");
        assert_eq!(value["refreshToken"], "refresh");
        assert!(value.get("access_token").is_none());
    }

    #[test]
    fn cursor_token_store_enriches_jwt_claims() {
        let store = CursorTokenStore::new(InMemoryAuthStore::new());
        let auth = store
            .save_auth(StoredCursorAuth {
                access_token: test_jwt(4_102_444_800, Some("user_1"), Some("me@example.com")),
                refresh_token: Some("refresh".into()),
                api_key: None,
            })
            .unwrap();

        assert_eq!(auth.user_id.as_deref(), Some("user_1"));
        assert_eq!(auth.email.as_deref(), Some("me@example.com"));
        assert_eq!(auth.expires, Some(4_102_444_800_000));
    }

    #[test]
    fn create_login_matches_cursor_deep_control_shape() {
        let login = create_cursor_login();
        assert!(
            login
                .login_url
                .starts_with("https://cursor.com/loginDeepControl?challenge=")
        );
        assert!(login.login_url.contains("&uuid="));
        assert!(login.login_url.contains("&mode=login&redirectTarget=cli"));
        assert!(!login.verifier.contains('='));
    }

    fn test_jwt(exp: u64, sub: Option<&str>, email: Option<&str>) -> String {
        let mut payload = serde_json::json!({ "exp": exp });
        if let Some(sub) = sub {
            payload["sub"] = serde_json::Value::String(sub.to_string());
        }
        if let Some(email) = email {
            payload["email"] = serde_json::Value::String(email.to_string());
        }
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        format!("{header}.{payload}.sig")
    }
}
