use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use super::constants::{OAUTH_CALLBACK_PATH, OAUTH_PORT, issuer};
use super::jwt::TokenResponse;
use super::pkce::{
    PkceCodes, build_authorize_url, exchange_code_for_tokens, generate_pkce, generate_state,
};

const BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

pub struct BrowserLoginConfig {
    pub issuer: String,
    pub port: u16,
    pub timeout: Duration,
}

impl BrowserLoginConfig {
    pub fn new(issuer: impl Into<String>) -> Self {
        Self {
            issuer: issuer.into(),
            port: OAUTH_PORT,
            timeout: BROWSER_LOGIN_TIMEOUT,
        }
    }

    fn redirect_uri(&self, bound_port: u16) -> String {
        format!("http://127.0.0.1:{bound_port}{OAUTH_CALLBACK_PATH}")
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        params.insert(key.into_owned(), value.into_owned());
    }
    params
}

fn extract_request_path_and_query(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut buf = [0; 4096];
    let n = stream.read(&mut buf).ok()?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _method = parts.next()?;
    let path_and_query = parts.next()?;
    let mut split = path_and_query.splitn(2, '?');
    let path = split.next().unwrap_or("").to_string();
    let query = split.next().unwrap_or("").to_string();
    Some((path, query))
}

fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let status_line = match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        404 => "404 Not Found",
        500 => "500 Internal Server Error",
        _ => "500 Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

pub fn run_browser_login() -> Result<TokenResponse, anyhow::Error> {
    let config = BrowserLoginConfig::new(issuer());
    run_browser_login_with_config(&config)
}

pub fn run_browser_login_with_config(
    config: &BrowserLoginConfig,
) -> Result<TokenResponse, anyhow::Error> {
    let pkce = generate_pkce();
    let state = generate_state();

    // Prefer canonical port; fall back to an ephemeral port if busy.
    let listener = match TcpListener::bind(format!("127.0.0.1:{}", config.port)) {
        Ok(l) => l,
        Err(_) => TcpListener::bind("127.0.0.1:0")
            .map_err(|e| anyhow::anyhow!("Failed to bind OAuth callback listener: {e}"))?,
    };
    let bound_port = listener
        .local_addr()
        .map_err(|e| anyhow::anyhow!("Failed to read bound port: {e}"))?
        .port();
    let redirect_uri = config.redirect_uri(bound_port);
    let auth_url = build_authorize_url(&config.issuer, &redirect_uri, &pkce, &state)?;

    listener
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("Failed to set non-blocking: {e}"))?;

    println!("Open this URL in your browser to authorize:\n\n  {auth_url}\n");

    let deadline = std::time::Instant::now() + config.timeout;
    loop {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("OAuth timeout");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                return handle_callback(
                    &mut stream,
                    &config.issuer,
                    &redirect_uri,
                    &pkce,
                    &state,
                );
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => anyhow::bail!("Server error: {e}"),
        }
    }
}

fn handle_callback(
    stream: &mut TcpStream,
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> Result<TokenResponse, anyhow::Error> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    let (path, query) = match extract_request_path_and_query(stream) {
        Some(pair) => pair,
        None => {
            write_response(stream, 400, "text/plain", "Bad request");
            anyhow::bail!("Bad request");
        }
    };

    if path != OAUTH_CALLBACK_PATH {
        write_response(stream, 404, "text/plain", "Not found");
        anyhow::bail!("Not found");
    }

    let params = parse_query(&query);
    if let Some(error) = params.get("error") {
        write_response(stream, 400, "text/plain", &format!("Auth failed: {error}"));
        anyhow::bail!("{error}");
    }

    let code = match params.get("code") {
        Some(c) => c.clone(),
        None => {
            write_response(stream, 400, "text/plain", "Auth failed: Invalid callback");
            anyhow::bail!("Invalid callback");
        }
    };

    let received_state = params.get("state").cloned().unwrap_or_default();
    if received_state != state {
        write_response(stream, 400, "text/plain", "Auth failed: Invalid callback");
        anyhow::bail!("Invalid callback: state mismatch");
    }

    match exchange_code_for_tokens(issuer, &code, pkce, redirect_uri) {
        Ok(tokens) => {
            write_response(
                stream,
                200,
                "text/html",
                "<html><body><h1>Authorization Successful</h1><p>You can close this window.</p></body></html>",
            );
            Ok(tokens)
        }
        Err(e) => {
            write_response(stream, 500, "text/plain", &e.to_string());
            Err(e)
        }
    }
}
