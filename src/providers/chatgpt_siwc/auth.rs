use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use rand::RngCore;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, oneshot};

use super::store::{Profile, Store, Tokens, validate_label};

pub(super) const ISSUER: &str = "https://auth.openai.com";
pub(super) const RESOURCE: &str = "https://api.openai.com/v1";
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";

#[derive(Clone)]
pub(super) struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub discovery: String,
    pub jwks: String,
    pub revoke: String,
    pub api: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: format!("{ISSUER}/api/accounts/authorize"),
            token: format!("{ISSUER}/api/accounts/oauth/token"),
            discovery: format!("{ISSUER}/.well-known/openid-configuration"),
            jwks: format!("{ISSUER}/.well-known/jwks.json"),
            revoke: format!("{ISSUER}/api/accounts/oauth/revoke"),
            api: RESOURCE.into(),
        }
    }
}

#[derive(Clone)]
pub(super) struct Auth {
    pub store: Store,
    pub profile: String,
    pub endpoints: Endpoints,
    http: reqwest::Client,
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(super) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .user_agent(concat!(
            "claude-code-proxy/",
            env!("CARGO_PKG_VERSION"),
            " SIWC"
        ))
        .build()
        .expect("SIWC HTTP client")
}

pub(super) async fn read_json(mut response: reqwest::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("HTTP response interrupted"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= 2 * 1024 * 1024,
            "HTTP JSON response exceeds limit"
        );
        bytes.extend(chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid HTTP JSON response"))
}

pub(super) struct Attempt {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub redirect_uri: String,
    pub host_id: String,
    pub selected: Option<Profile>,
    pub issued_client: Option<String>,
}

fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

impl Attempt {
    pub fn new(redirect_uri: String, host_id: String, selected: Option<Profile>) -> Self {
        Self {
            state: random_secret(),
            nonce: random_secret(),
            verifier: random_secret(),
            redirect_uri,
            host_id,
            selected,
            issued_client: None,
        }
    }

    pub fn authorize_url(
        &self,
        endpoints: &Endpoints,
        consent: bool,
        hints: bool,
    ) -> Result<url::Url> {
        let mut url = url::Url::parse(&endpoints.authorize)?;
        let client_id = self
            .selected
            .as_ref()
            .map(|p| p.client_id.as_str())
            .or(self.issued_client.as_deref())
            .unwrap_or(DYNAMIC_CLIENT);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(self.verifier.as_bytes()));
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client_id)
            .append_pair("ext_agent_host_id", &self.host_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", &self.redirect_uri)
            .append_pair("scope", SCOPES)
            .append_pair("resource", RESOURCE)
            .append_pair("state", &self.state)
            .append_pair("nonce", &self.nonce)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", &challenge);
        if client_id == DYNAMIC_CLIENT {
            q.append_pair("agent_name_hint", "claude-code-proxy");
        }
        if consent {
            q.append_pair("prompt", "consent");
        }
        if hints && let Some(profile) = &self.selected {
            if let Some(tokens) = &profile.tokens {
                q.append_pair("id_token_hint", &tokens.id_token);
            }
            if let Some(email) = &profile.email {
                q.append_pair("login_hint", email);
            }
        }
        drop(q);
        Ok(url)
    }

    pub fn callback(&mut self, query: &HashMap<String, String>) -> Result<String> {
        ensure!(
            query.get("state") == Some(&self.state),
            "OAuth state mismatch"
        );
        if query.contains_key("error") {
            bail!("OAuth authorization declined or failed; no tokens exchanged");
        }
        let expected = self
            .selected
            .as_ref()
            .map(|p| p.client_id.as_str())
            .or(self.issued_client.as_deref());
        let client = query
            .get("client_id")
            .map(String::as_str)
            .or(expected)
            .context("Registration incomplete: no issued client_id")?;
        ensure!(
            !client.is_empty()
                && client != DYNAMIC_CLIENT
                && client.len() <= 512
                && client
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
            "Invalid issued client_id"
        );
        ensure!(
            expected.is_none_or(|id| id == client),
            "OAuth client_id changed for selected registration"
        );
        let code = query
            .get("code")
            .filter(|s| !s.is_empty())
            .context("OAuth callback has no authorization code")?
            .clone();
        self.issued_client = Some(client.to_string());
        Ok(code)
    }
}

#[derive(Clone, Deserialize)]
struct IdClaims {
    sub: String,
    iss: String,
    aud: Value,
    exp: u64,
    iat: u64,
    nonce: Option<String>,
    azp: Option<String>,
    email: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    id_token: Option<String>,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
    earliest_refresh_at: Option<u64>,
}

impl Auth {
    pub fn new(profile: String) -> Result<Self> {
        validate_label(&profile)?;
        Ok(Self {
            store: Store::default_path(),
            profile,
            endpoints: Endpoints::default(),
            http: http_client(),
        })
    }

    #[cfg(test)]
    pub fn for_test(store: Store, endpoints: Endpoints) -> Self {
        Self {
            store,
            profile: "default".into(),
            endpoints,
            http: http_client(),
        }
    }

    async fn discovery(&self) -> Result<Value> {
        let response = self
            .http
            .get(&self.endpoints.discovery)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("OpenAI discovery network error"))?;
        ensure!(response.status().is_success(), "OpenAI discovery failed");
        let doc = read_json(response).await?;
        ensure!(
            doc["issuer"] == ISSUER
                && doc["jwks_uri"] == self.endpoints.jwks
                && doc["token_endpoint"] == self.endpoints.token
                && doc["revocation_endpoint"] == self.endpoints.revoke,
            "Unexpected OpenAI discovery endpoints"
        );
        Ok(doc)
    }

    async fn verify_id(
        &self,
        token: &str,
        client_id: &str,
        nonce: &str,
        refresh: bool,
    ) -> Result<IdClaims> {
        ensure!(token.len() <= 64 * 1024, "ID token exceeds limit");
        let header =
            decode_header(token).map_err(|_| anyhow::anyhow!("Invalid ID token header"))?;
        ensure!(header.alg == Algorithm::RS256, "ID token must use RS256");
        let kid = header.kid.context("ID token missing key ID")?;
        let doc = self.discovery().await?;
        let response = self
            .http
            .get(doc["jwks_uri"].as_str().context("Missing JWKS URI")?)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("JWKS network error"))?;
        ensure!(response.status().is_success(), "JWKS request failed");
        let raw = read_json(response).await?;
        let keys: JwkSet = serde_json::from_value(raw.clone()).context("Invalid JWKS")?;
        let key = keys
            .find(&kid)
            .context("ID token key is absent from official JWKS")?;
        let raw_key = raw["keys"]
            .as_array()
            .and_then(|keys| keys.iter().find(|k| k["kid"] == kid))
            .context("Invalid signing key")?;
        ensure!(
            raw_key["kty"] == "RSA"
                && raw_key.get("alg").is_none_or(|v| v == "RS256")
                && raw_key.get("use").is_none_or(|v| v == "sig"),
            "Invalid JWKS signing key type"
        );
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_required_spec_claims(&["iss", "sub", "aud", "exp"]);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[client_id]);
        validation.validate_nbf = true;
        validation.leeway = 0;
        let key = DecodingKey::from_jwk(key).context("Invalid RSA signing key")?;
        let claims = decode::<IdClaims>(token, &key, &validation)
            .map_err(|_| anyhow::anyhow!("ID token signature or claims validation failed"))?
            .claims;
        ensure!(
            !claims.sub.is_empty() && claims.iat <= now() + 60 && claims.exp > claims.iat,
            "Invalid ID token identity or lifetime"
        );
        ensure!(
            (refresh && claims.nonce.is_none()) || claims.nonce.as_deref() == Some(nonce),
            "ID token nonce mismatch"
        );
        let multiple_audiences = claims.aud.as_array().is_some_and(|a| a.len() > 1);
        ensure!(
            !multiple_audiences || claims.azp.as_deref() == Some(client_id),
            "ID token authorized party missing or invalid"
        );
        ensure!(
            claims.azp.as_deref().is_none_or(|azp| azp == client_id),
            "ID token authorized party mismatch"
        );
        Ok(claims)
    }

    fn token_set(response: TokenResponse, prior: Option<&Tokens>, nonce: &str) -> Result<Tokens> {
        ensure!(
            !response.access_token.is_empty()
                && !response.refresh_token.is_empty()
                && response.token_type.eq_ignore_ascii_case("Bearer")
                && response.expires_in > 0,
            "Invalid OAuth token response"
        );
        let scopes = match response.scope {
            Some(scope) => scope.split_whitespace().map(String::from).collect(),
            None => prior
                .map(|t| t.scopes.clone())
                .context("OAuth response omitted granted scopes")?,
        };
        let id_token = response
            .id_token
            .or_else(|| prior.map(|t| t.id_token.clone()))
            .context("Missing ID token")?;
        Ok(Tokens {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            id_token,
            expires_at: now().saturating_add(response.expires_in),
            earliest_refresh_at: response.earliest_refresh_at,
            scopes,
            nonce: nonce.to_string(),
        })
    }

    pub async fn exchange(&self, attempt: &Attempt, code: &str) -> Result<bool> {
        let client_id = attempt
            .issued_client
            .as_deref()
            .context("No issued client_id")?;
        let response = self
            .http
            .post(&self.endpoints.token)
            .timeout(Duration::from_secs(30))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id),
                ("code", code),
                ("code_verifier", &attempt.verifier),
                ("redirect_uri", &attempt.redirect_uri),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(|_| {
                anyhow::anyhow!("OAuth code exchange interrupted; start a fresh sign-in")
            })?;
        let status = response.status();
        let raw = read_json(response).await?;
        if !status.is_success() {
            if raw["error"] == "invalid_grant" {
                bail!("invalid_grant: start fresh authorization with the issued client ID");
            }
            bail!("OAuth code exchange failed (HTTP {})", status.as_u16());
        }
        let response: TokenResponse = serde_json::from_value(raw)
            .map_err(|_| anyhow::anyhow!("Invalid OAuth token response"))?;
        let claims = self
            .verify_id(
                response.id_token.as_deref().context("Missing ID token")?,
                client_id,
                &attempt.nonce,
                false,
            )
            .await?;
        if let Some(selected) = &attempt.selected {
            ensure!(
                claims.sub == selected.subject && claims.iss == selected.issuer,
                "Returning account identity mismatch; original credentials retained"
            );
        }
        let tokens = Self::token_set(response, None, &attempt.nonce)?;
        let permitted = tokens.permits_inference();
        let mut locked = self.store.lock().await?;
        ensure!(
            locked.state.ext_agent_host_id == attempt.host_id,
            "SIWC host changed during authorization"
        );
        let current = locked.state.profiles.get(&self.profile);
        ensure!(
            current.map(|p| p.revision) == attempt.selected.as_ref().map(|p| p.revision),
            "Account changed during authorization; repeat sign-in"
        );
        ensure!(
            !locked
                .state
                .profiles
                .iter()
                .any(|(label, p)| label != &self.profile && p.client_id == client_id),
            "This registration already belongs to another profile; select that profile"
        );
        locked.state.profiles.insert(
            self.profile.clone(),
            Profile {
                client_id: client_id.into(),
                issuer: claims.iss,
                subject: claims.sub,
                email: claims.email,
                revision: attempt.selected.as_ref().map_or(1, |p| p.revision + 1),
                tokens: Some(tokens),
                refresh_pending: false,
            },
        );
        locked.save()?;
        Ok(permitted)
    }

    pub async fn access_token(&self) -> Result<String> {
        let mut locked = self.store.lock().await?;
        let profile = locked
            .state
            .profiles
            .get_mut(&self.profile)
            .context("SIWC profile is not signed in; run siwc auth login")?;
        ensure!(
            !profile.refresh_pending,
            "Previous refresh outcome is uncertain; run siwc auth login to reauthorize safely"
        );
        let old = profile
            .tokens
            .clone()
            .context("SIWC profile is signed out; run siwc auth login")?;
        ensure!(
            old.permits_inference(),
            "ChatGPT plan permission is disabled; run siwc auth login --consent"
        );
        if old.expires_at > now() + 60
            || (old.expires_at > now() && old.earliest_refresh_at.is_some_and(|t| t > now()))
        {
            return Ok(old.access_token);
        }
        ensure!(
            old.earliest_refresh_at.is_none_or(|t| t <= now()),
            "Access token expired before earliest_refresh_at; retry later or reauthorize"
        );
        let client_id = profile.client_id.clone();
        let subject = profile.subject.clone();
        profile.refresh_pending = true;
        profile.revision += 1;
        locked.save()?;
        // Durable before network I/O: cancellation, crashes, or lost replies must not reuse a rotating token.
        let response = self
            .http
            .post(&self.endpoints.token)
            .timeout(Duration::from_secs(30))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client_id),
                ("refresh_token", &old.refresh_token),
                ("resource", RESOURCE),
            ])
            .send()
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "Refresh outcome uncertain; credentials retained, reauthorization required"
                )
            })?;
        let status = response.status();
        let raw = read_json(response).await?;
        if !status.is_success() {
            let code = raw["error"]
                .as_str()
                .or_else(|| raw.pointer("/error/code").and_then(Value::as_str))
                .unwrap_or("");
            let terminal = matches!(
                code,
                "invalid_grant"
                    | "invalid_refresh_token"
                    | "token_expired"
                    | "refresh_token_expired"
                    | "refresh_token_invalidated"
                    | "refresh_token_reused"
            );
            let profile = locked
                .state
                .profiles
                .get_mut(&self.profile)
                .expect("locked profile");
            profile.refresh_pending = false;
            if terminal {
                profile.tokens = None;
            }
            locked.save()?;
            if terminal {
                bail!("Refresh token is unusable; run siwc auth login (saved client ID retained)");
            }
            if code == "invalid_client" {
                bail!("OAuth invalid_client: verify saved client registration");
            }
            bail!(
                "Token refresh failed (HTTP {}); credentials retained",
                status.as_u16()
            );
        }
        let response: TokenResponse = serde_json::from_value(raw)
            .map_err(|_| anyhow::anyhow!("Invalid refresh response; reauthorization required"))?;
        if let Some(id) = &response.id_token {
            let claims = self.verify_id(id, &client_id, &old.nonce, true).await?;
            ensure!(
                claims.sub == subject,
                "Refresh account identity mismatch; reauthorization required"
            );
        }
        let tokens = Self::token_set(response, Some(&old), &old.nonce)?;
        let access = tokens.access_token.clone();
        let permitted = tokens.permits_inference();
        let profile = locked
            .state
            .profiles
            .get_mut(&self.profile)
            .expect("locked profile");
        profile.tokens = Some(tokens);
        profile.refresh_pending = false;
        locked.save()?;
        ensure!(
            permitted,
            "ChatGPT plan permission was removed; inference stopped"
        );
        Ok(access)
    }

    pub async fn logout(&self) -> Result<bool> {
        let mut locked = self.store.lock().await?;
        let Some(profile) = locked.state.profiles.get(&self.profile) else {
            return Ok(true);
        };
        let mut confirmed = profile.tokens.is_none();
        if let Some(tokens) = &profile.tokens
            && let Ok(doc) = self.discovery().await
        {
            for attempt in 0..3 {
                let response = self
                    .http
                    .post(
                        doc["revocation_endpoint"]
                            .as_str()
                            .context("Missing revocation endpoint")?,
                    )
                    .timeout(Duration::from_secs(15))
                    .form(&[
                        ("token", tokens.refresh_token.as_str()),
                        ("token_type_hint", "refresh_token"),
                        ("client_id", profile.client_id.as_str()),
                    ])
                    .send()
                    .await;
                match response {
                    Ok(response) if response.status() == reqwest::StatusCode::OK => {
                        confirmed = true;
                        break;
                    }
                    Ok(response) if !response.status().is_server_error() => break,
                    _ if attempt < 2 => {
                        tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await
                    }
                    _ => break,
                }
            }
        }
        let profile = locked
            .state
            .profiles
            .get_mut(&self.profile)
            .expect("locked profile");
        profile.tokens = None;
        profile.refresh_pending = false;
        profile.revision += 1;
        locked.save()?;
        Ok(confirmed)
    }

    pub async fn session_active(&self) -> Result<bool> {
        let locked = self.store.lock().await?;
        Ok(locked
            .state
            .profiles
            .get(&self.profile)
            .and_then(|p| p.tokens.as_ref())
            .is_some_and(Tokens::permits_inference))
    }

    pub async fn status(&self) -> Result<Value> {
        let locked = self.store.lock().await?;
        let profiles: Vec<_> = locked.state.profiles.iter().map(|(label, p)| serde_json::json!({
            "profile": label, "selected": label == &self.profile, "client_id": p.client_id, "subject": p.subject, "email": p.email,
            "signed_in": p.tokens.is_some(), "plan_permission": p.tokens.as_ref().is_some_and(Tokens::permits_inference),
            "expires_at": p.tokens.as_ref().map(|t| t.expires_at), "reauthorization_required": p.refresh_pending,
        })).collect();
        Ok(
            serde_json::json!({"selected_profile":self.profile,"ext_agent_host_id":locked.state.ext_agent_host_id,"profiles":profiles}),
        )
    }

    pub async fn login(&self, consent: bool, no_browser: bool) -> Result<()> {
        let locked = self.store.lock().await?;
        let host_id = locked.state.ext_agent_host_id.clone();
        let selected = locked.state.profiles.get(&self.profile).cloned();
        drop(locked);
        let mut issued_client = None;
        for retry in 0..2 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let redirect = format!(
                "http://127.0.0.1:{}/auth/callback",
                listener.local_addr()?.port()
            );
            let mut attempt = Attempt::new(redirect, host_id.clone(), selected.clone());
            attempt.issued_client = issued_client;
            let url = attempt.authorize_url(&self.endpoints, consent, !no_browser)?;
            let callback = wait_callback(listener, attempt.state.clone());
            tokio::pin!(callback);
            if no_browser {
                // Never print a retained ID token: hint-free authorization is equally valid.
                println!("Continue with ChatGPT:\n{url}");
            } else {
                open_browser(url.as_str())?;
                println!("Continue with ChatGPT in your browser. Waiting for authorization…");
            }
            let query = callback.await?;
            let code = attempt.callback(&query)?;
            match self.exchange(&attempt, &code).await {
                Ok(true) => {
                    println!(
                        "Signed in to SIWC profile '{}'; ChatGPT plan permission granted.",
                        self.profile
                    );
                    return Ok(());
                }
                Ok(false) => {
                    println!(
                        "Signed in to SIWC profile '{}', but ChatGPT plan permission is disabled. Run siwc auth login --consent before inference.",
                        self.profile
                    );
                    return Ok(());
                }
                Err(e) if retry == 0 && e.to_string().starts_with("invalid_grant:") => {
                    issued_client = attempt.issued_client.clone();
                    println!(
                        "Authorization code expired; starting fresh authorization with the issued client ID."
                    );
                }
                Err(e) => return Err(e),
            }
        }
        bail!("OAuth authorization did not complete")
    }
}

pub(super) async fn wait_callback(
    listener: tokio::net::TcpListener,
    expected_state: String,
) -> Result<HashMap<String, String>> {
    use axum::{Router, extract::Query, http::StatusCode, response::IntoResponse, routing::get};
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let app = Router::new().route(
        "/auth/callback",
        get(move |Query(query): Query<HashMap<String, String>>| {
            let tx = tx.clone();
            let expected_state = expected_state.clone();
            async move {
                if query.get("state") != Some(&expected_state) {
                    return (StatusCode::BAD_REQUEST, "OAuth state mismatch").into_response();
                }
                let Some(tx) = tx.lock().await.take() else {
                    return (StatusCode::CONFLICT, "Authorization callback already used")
                        .into_response();
                };
                let _ = tx.send(query);
                (
                    [
                        ("cache-control", "no-store"),
                        ("referrer-policy", "no-referrer"),
                        ("content-security-policy", "default-src 'none'"),
                    ],
                    "Authorization received. Return to the terminal for validation results.",
                )
                    .into_response()
            }
        }),
    );
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => { result?; bail!("OAuth callback listener stopped") },
        result = tokio::time::timeout(Duration::from_secs(300), rx) => {
            let _ = shutdown_tx.send(());
            let _ = tokio::time::timeout(Duration::from_secs(1), &mut server).await;
            result.context("OAuth authorization timed out")?.context("OAuth callback closed")
        },
        _ = tokio::signal::ctrl_c() => bail!("OAuth authorization cancelled"),
    }
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("/usr/bin/open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    #[cfg(not(target_os = "macos"))]
    let result = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    ensure!(
        result.is_ok_and(|status| status.success()),
        "Cannot open browser; retry siwc auth login --no-browser"
    );
    Ok(())
}
