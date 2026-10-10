use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use super::constants::{ISSUER, OAUTH_PORT};
use super::jwt::TokenResponse;
use super::pkce::{
    PkceCodes, build_authorize_url, exchange_code_for_tokens, generate_pkce, generate_state,
};

const BROWSER_LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

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

    fn redirect_uri(&self) -> String {
        format!("http://localhost:{}/auth/callback", self.port)
    }
}

// ---------------------------------------------------------------------------
// Query parsing helpers
// ---------------------------------------------------------------------------

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

    // Parse first line: GET /path?query HTTP/1.1
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

// ---------------------------------------------------------------------------
// Main browser login flow
// ---------------------------------------------------------------------------

pub fn run_browser_login() -> Result<TokenResponse, anyhow::Error> {
    let config = BrowserLoginConfig::new(ISSUER);
    run_browser_login_with_config(&config)
}

pub fn run_browser_login_with_config(
    config: &BrowserLoginConfig,
) -> Result<TokenResponse, anyhow::Error> {
    let pkce = generate_pkce();
    let state = generate_state();
    let redirect_uri = config.redirect_uri();
    let auth_url = build_authorize_url(&config.issuer, &redirect_uri, &pkce, &state)?;

    let listener = TcpListener::bind(format!("127.0.0.1:{}", config.port))
        .map_err(|e| anyhow::anyhow!("Failed to bind to port {}: {e}", config.port))?;

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
                match handle_callback(&mut stream, &config.issuer, &redirect_uri, &pkce, &state) {
                    // Ignore unrelated or malformed requests and keep waiting for
                    // the actual OAuth callback.
                    Ok(None) => continue,
                    Ok(Some(tokens)) => return Ok(tokens),
                    Err(e) => return Err(e),
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                anyhow::bail!("Server error: {e}");
            }
        }
    }
}

fn handle_callback(
    stream: &mut TcpStream,
    issuer: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
    state: &str,
) -> Result<Option<TokenResponse>, anyhow::Error> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

    let (path, query) = match extract_request_path_and_query(stream) {
        Some(pair) => pair,
        None => {
            write_response(stream, 400, "text/plain", "Bad request");
            return Ok(None);
        }
    };

    if path != "/auth/callback" {
        write_response(stream, 404, "text/plain", "Not found");
        return Ok(None);
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
            Ok(Some(tokens))
        }
        Err(e) => {
            write_response(stream, 500, "text/plain", &e.to_string());
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::codex::auth::test_http;
    use std::sync::Arc;

    /// A minimal TCP-based mock auth server for browser login tests.
    struct MockBrowserLoginAuthServer {
        server: test_http::MockServer,
    }

    impl MockBrowserLoginAuthServer {
        fn new() -> Self {
            let server = test_http::spawn_mock_server(
                "mock browser login auth server should become ready",
                |request| {
                    if request.contains("/oauth/token") {
                        let body = r#"{"access_token":"bt_at","refresh_token":"bt_rt","expires_in":3600,"id_token":"bt_id"}"#;
                        test_http::json_response(200, body)
                    } else {
                        test_http::json_response(404, r#"{"error":"not found"}"#)
                    }
                },
            );
            Self { server }
        }

        fn url(&self) -> &str {
            &self.server.url
        }
    }

    #[test]
    fn handle_callback_ignores_wrong_path() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let handle = std::thread::spawn(move || {
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    let result = handle_callback(
                        &mut stream,
                        "http://fake-issuer",
                        "http://localhost:1455/auth/callback",
                        &PkceCodes {
                            verifier: "v".into(),
                            challenge: "c".into(),
                        },
                        "test_state",
                    );
                    assert!(
                        matches!(result, Ok(None)),
                        "non-callback request must return Ok(None)"
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let response = test_http::send_get(addr, "/wrong/path");
        handle.join().unwrap();
        assert!(response.contains("404"), "unexpected response: {response}");
        assert!(
            response.contains("Not found"),
            "unexpected response: {response}"
        );
    }

    #[test]
    fn callback_rejects_wrong_state() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let handle = std::thread::spawn(move || {
            let pkce = PkceCodes {
                verifier: "v".into(),
                challenge: "c".into(),
            };
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    let result = handle_callback(
                        &mut stream,
                        "http://fake-issuer",
                        "http://localhost:1455/auth/callback",
                        &pkce,
                        "expected_state",
                    );
                    assert!(result.is_err());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let response = test_http::send_get(addr, "/auth/callback?code=test_code&state=wrong_state");
        handle.join().unwrap();
        assert!(response.contains("400"));
        assert!(response.contains("Invalid callback"));
    }

    #[test]
    fn callback_rejects_missing_code() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let handle = std::thread::spawn(move || {
            let pkce = PkceCodes {
                verifier: "v".into(),
                challenge: "c".into(),
            };
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    let result = handle_callback(
                        &mut stream,
                        "http://fake-issuer",
                        "http://localhost:1455/auth/callback",
                        &pkce,
                        "state",
                    );
                    assert!(result.is_err());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let response = test_http::send_get(addr, "/auth/callback?state=state");
        handle.join().unwrap();
        assert!(response.contains("400"));
    }

    #[test]
    fn callback_rejects_error_param() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let handle = std::thread::spawn(move || {
            let pkce = PkceCodes {
                verifier: "v".into(),
                challenge: "c".into(),
            };
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    let result = handle_callback(
                        &mut stream,
                        "http://fake-issuer",
                        "http://localhost:1455/auth/callback",
                        &pkce,
                        "state",
                    );
                    assert!(result.is_err());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let response = test_http::send_get(addr, "/auth/callback?error=access_denied&state=state");
        handle.join().unwrap();
        assert!(response.contains("400"));
        assert!(response.contains("access_denied"));
    }

    #[test]
    fn callback_success_exchanges_tokens() {
        let auth_server = MockBrowserLoginAuthServer::new();
        let auth_url = auth_server.url().to_string();
        let callback_result = Arc::new(std::sync::Mutex::new(None));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let callback_result_thread = callback_result.clone();
        let handle = std::thread::spawn(move || {
            let pkce = PkceCodes {
                verifier: "test_verifier".into(),
                challenge: "test_challenge".into(),
            };
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    let result = handle_callback(
                        &mut stream,
                        &auth_url,
                        &format!("http://localhost:{port}/auth/callback"),
                        &pkce,
                        "expected_state",
                    );
                    let stored = match result {
                        Ok(Some(tokens)) => Ok((tokens.access_token, tokens.refresh_token)),
                        Ok(None) => Err("callback ignored non-callback request".to_string()),
                        Err(err) => Err(err.to_string()),
                    };
                    *callback_result_thread.lock().unwrap() = Some(stored);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let response =
            test_http::send_get(addr, "/auth/callback?code=test_code&state=expected_state");
        handle.join().unwrap();
        assert!(response.contains("200"), "unexpected response: {response}");
        let tokens = callback_result
            .lock()
            .unwrap()
            .take()
            .expect("callback handler should store a result")
            .unwrap();
        assert_eq!(tokens.0, "bt_at");
        assert_eq!(tokens.1, "bt_rt");

        // auth_server dropped here, closing the mock server
        drop(auth_server);
    }

    #[test]
    fn callback_exchange_failure_returns_error() {
        let fail_server = test_http::spawn_mock_server(
            "mock browser login failure server should become ready",
            |request| {
                let body = if request.contains("/oauth/token") {
                    r#"{"error":"bad exchange"}"#
                } else {
                    r#"{"error":"nf"}"#
                };
                test_http::json_response(500, body)
            },
        );
        let fail_issuer = fail_server.url.clone();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let fail_issuer_clone = fail_issuer.clone();
        let handle = std::thread::spawn(move || {
            let pkce = PkceCodes {
                verifier: "v".into(),
                challenge: "c".into(),
            };
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    let result = handle_callback(
                        &mut stream,
                        &fail_issuer_clone,
                        &format!("http://localhost:{port}/auth/callback"),
                        &pkce,
                        "expected_state",
                    );
                    assert!(result.is_err());
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        let response =
            test_http::send_get(addr, "/auth/callback?code=test_code&state=expected_state");
        handle.join().unwrap();
        assert!(response.contains("500"), "unexpected response: {response}");
        assert!(
            response.contains("Token exchange failed"),
            "unexpected response: {response}"
        );
        drop(fail_server);
    }

    #[test]
    fn browser_login_ignores_non_callback_request() {
        // A non-callback request must not terminate an in-flight login.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = BrowserLoginConfig {
            issuer: "http://fake-issuer".into(),
            port,
            timeout: Duration::from_secs(1),
        };

        let handle = std::thread::spawn(move || {
            run_browser_login_with_config(&config).map_err(|err| err.to_string())
        });

        let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        // Wait for the login thread to bind. A bare connect is itself a
        // non-callback probe and must not kill the login either.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::net::TcpStream::connect(addr).is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "login thread did not bind in time"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let response = test_http::send_get(addr, "/");
        assert!(response.contains("404"), "unexpected response: {response}");

        let result = handle.join().unwrap();
        assert!(
            matches!(result, Err(ref err) if err.contains("OAuth timeout")),
            "non-callback request must not terminate browser login: {result:?}"
        );
    }

    #[test]
    fn browser_login_times_out() {
        // Get a free port nobody is sending callbacks to
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let config = BrowserLoginConfig {
            issuer: "http://fake-issuer".into(),
            port,
            timeout: Duration::from_millis(100),
        };

        // run_browser_login_with_config will bind to the port and timeout
        let result = run_browser_login_with_config(&config);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("OAuth timeout"),
            "expected OAuth timeout error"
        );
    }
}
