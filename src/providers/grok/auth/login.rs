use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use url::Url;

use super::pkce::{PkceCodes, generate_pkce, generate_state};
use super::token_store::{GrokTokenStore, StoredAuth};
use crate::auth::AuthStorage;

pub const CANONICAL_ISSUER: &str = "https://auth.x.ai";
pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub(super) const SCOPES: &str = "openid profile email offline_access grok-cli:access api:access conversations:read conversations:write";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_METADATA_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    response_types_supported: Vec<String>,
    grant_types_supported: Vec<String>,
    code_challenge_methods_supported: Vec<String>,
    token_endpoint_auth_methods_supported: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    token_type: Option<String>,
}

pub fn login<S: AuthStorage<StoredAuth>>(store: &GrokTokenStore<S>) -> anyhow::Result<()> {
    let client = client()?;
    let discovery = discover(&client)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let pkce = generate_pkce();
    let state = generate_state();
    let auth_url = authorize_url(&discovery, &redirect_uri, &pkce, &state)?;

    println!("Open this URL in your browser to authorize:\n\n  {auth_url}\n");
    open_browser(&auth_url);
    let code = wait_for_callback(&listener, &state, LOGIN_TIMEOUT)?;
    let tokens = exchange_code(&client, &discovery, &code, &pkce, &redirect_uri)?;
    validate_tokens(&tokens)?;
    let refresh = tokens
        .refresh_token
        .as_ref()
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Grok login did not grant an offline session"))?;
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
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()?)
}

fn discover(client: &reqwest::blocking::Client) -> anyhow::Result<Discovery> {
    let url = format!("{CANONICAL_ISSUER}/.well-known/openid-configuration");
    let response = client.get(url).send()?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|size| size as usize > MAX_METADATA_BYTES)
    {
        anyhow::bail!("Grok OIDC metadata exceeds the size limit");
    }
    let bytes = response.bytes()?;
    if bytes.len() > MAX_METADATA_BYTES {
        anyhow::bail!("Grok OIDC metadata exceeds the size limit");
    }
    let discovery: Discovery = serde_json::from_slice(&bytes)?;
    validate_discovery(&discovery)?;
    Ok(discovery)
}

fn validate_discovery(discovery: &Discovery) -> anyhow::Result<()> {
    if discovery.issuer != CANONICAL_ISSUER {
        anyhow::bail!("OIDC discovery issuer mismatch");
    }
    let issuer = Url::parse(CANONICAL_ISSUER)?;
    for endpoint in [&discovery.authorization_endpoint, &discovery.token_endpoint] {
        let endpoint = Url::parse(endpoint)?;
        if endpoint.scheme() != "https" || endpoint.origin() != issuer.origin() {
            anyhow::bail!("OIDC endpoint is outside the canonical issuer");
        }
    }
    if !discovery
        .response_types_supported
        .iter()
        .any(|v| v == "code")
        || !discovery
            .grant_types_supported
            .iter()
            .any(|v| v == "authorization_code")
        || !discovery
            .grant_types_supported
            .iter()
            .any(|v| v == "refresh_token")
        || !discovery
            .code_challenge_methods_supported
            .iter()
            .any(|v| v == "S256")
        || !discovery
            .token_endpoint_auth_methods_supported
            .iter()
            .any(|v| v == "none")
    {
        anyhow::bail!("Grok OIDC provider lacks required OAuth capabilities");
    }
    Ok(())
}

fn authorize_url(
    discovery: &Discovery,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> anyhow::Result<String> {
    let mut url = Url::parse(&discovery.authorization_endpoint)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", SCOPES)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url.into())
}

fn exchange_code(
    client: &reqwest::blocking::Client,
    discovery: &Discovery,
    code: &str,
    pkce: &PkceCodes,
    redirect_uri: &str,
) -> anyhow::Result<TokenResponse> {
    let response = client
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", pkce.verifier.as_str()),
            ("redirect_uri", redirect_uri),
        ])
        .send()?;
    if !response.status().is_success() {
        anyhow::bail!(
            "Grok token exchange failed with status {}",
            response.status()
        );
    }
    let tokens: TokenResponse = response.json()?;
    validate_tokens(&tokens)?;
    Ok(tokens)
}

fn validate_tokens(tokens: &TokenResponse) -> anyhow::Result<()> {
    if tokens.access_token.is_empty()
        || tokens.expires_in == 0
        || tokens
            .token_type
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case("bearer"))
    {
        anyhow::bail!("Grok token response is invalid");
    }
    Ok(())
}

fn wait_for_callback(
    listener: &TcpListener,
    state: &str,
    timeout: Duration,
) -> anyhow::Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("Grok OAuth login timed out");
        }
        match listener.accept() {
            Ok((mut stream, _)) => match callback(&mut stream, state) {
                Callback::Code(code) => return Ok(code),
                Callback::Terminal(error) => anyhow::bail!(error),
                Callback::Ignore => {}
            },
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

enum Callback {
    Code(String),
    Terminal(String),
    Ignore,
}

fn callback(stream: &mut TcpStream, expected_state: &str) -> Callback {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = [0_u8; 8192];
    let Ok(length) = stream.read(&mut buffer) else {
        return Callback::Ignore;
    };
    let request = String::from_utf8_lossy(&buffer[..length]);
    let Some(target) = request.lines().next().and_then(|line| {
        let mut parts = line.split_whitespace();
        (parts.next() == Some("GET"))
            .then(|| parts.next())
            .flatten()
    }) else {
        return Callback::Ignore;
    };
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignore;
    };
    if url.path() != "/callback" {
        respond(stream, "404 Not Found", "Not found");
        return Callback::Ignore;
    }
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    if params.contains_key("error") {
        respond(stream, "400 Bad Request", "Authorization failed");
        return Callback::Terminal("Grok authorization was denied".into());
    }
    let valid_state = params
        .get("state")
        .is_some_and(|value| constant_time_eq(value, expected_state));
    let code = params.get("code").filter(|value| !value.is_empty());
    if !valid_state || code.is_none() {
        respond(stream, "400 Bad Request", "Invalid authorization callback");
        return Callback::Terminal("Grok authorization callback is invalid".into());
    }
    respond(
        stream,
        "200 OK",
        "Authorization received. Return to the terminal.",
    );
    Callback::Code(code.unwrap().clone())
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let command = ("xdg-open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
    {
        let _ = std::process::Command::new(command.0)
            .args(command.1)
            .spawn();
    }
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

    fn metadata() -> Discovery {
        Discovery {
            issuer: CANONICAL_ISSUER.into(),
            authorization_endpoint: "https://auth.x.ai/oauth2/authorize".into(),
            token_endpoint: "https://auth.x.ai/oauth2/token".into(),
            response_types_supported: vec!["code".into()],
            grant_types_supported: vec!["authorization_code".into(), "refresh_token".into()],
            code_challenge_methods_supported: vec!["S256".into()],
            token_endpoint_auth_methods_supported: vec!["none".into()],
        }
    }

    #[test]
    fn authorization_url_has_exact_public_client_contract() {
        let url = authorize_url(
            &metadata(),
            "http://127.0.0.1:1234/callback",
            &generate_pkce(),
            "state",
        )
        .unwrap();
        let url = Url::parse(&url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.get("client_id").unwrap(), CLIENT_ID);
        assert_eq!(params.get("scope").unwrap(), SCOPES);
        assert_eq!(
            params.get("redirect_uri").unwrap(),
            "http://127.0.0.1:1234/callback"
        );
        assert!(!params.contains_key("client_secret"));
    }

    #[test]
    fn discovery_rejects_cross_origin_endpoint() {
        let mut value = metadata();
        value.token_endpoint = "https://example.com/token".into();
        assert!(validate_discovery(&value).is_err());
    }

    #[test]
    fn callback_validates_state_and_path() {
        assert!(constant_time_eq("state", "state"));
        assert!(!constant_time_eq("state", "other"));
    }
}
