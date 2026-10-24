//! Proxy-owned OAuth state. No CLI/keychain import, account rotation, or onboarding.
pub(crate) use super::refresh::AccessError;
use super::refresh::RefreshManager;
use super::{
    config::Config,
    replay::{self, Result},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Credential {
    pub profile_id: String,
    pub project_id: Option<String>,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
}
impl Credential {
    fn validate(&self) -> Result<()> {
        if self.profile_id.is_empty()
            || self.profile_id.len() > 128
            || self.access_token.is_empty()
            || self.access_token.len() > 16384
            || http::HeaderValue::from_str(&format!("Bearer {}", self.access_token)).is_err()
            || self
                .refresh_token
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 16384)
            || self
                .project_id
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 4096)
        {
            return Err("invalid");
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
    #[cfg(test)]
    before_save: Option<Arc<dyn Fn() + Send + Sync>>,
}
pub(crate) struct Lock {
    file: File,
    path: PathBuf,
}
impl Lock {
    pub fn epoch(&mut self) -> Result<String> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| "auth lock read failed")?;
        let mut epoch = String::new();
        std::io::Read::by_ref(&mut self.file)
            .take(129)
            .read_to_string(&mut epoch)
            .map_err(|_| "auth lock read failed")?;
        if epoch.len() > 128 {
            return Err("invalid auth lock revision");
        }
        Ok(epoch)
    }
    pub fn advance(&mut self) -> Result<()> {
        self.file
            .set_len(0)
            .map_err(|_| "auth revision write failed")?;
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|_| "auth revision write failed")?;
        self.file
            .write_all(uuid::Uuid::new_v4().to_string().as_bytes())
            .map_err(|_| "auth revision write failed")?;
        self.file
            .sync_all()
            .map_err(|_| "auth revision sync failed")
    }
}
// Closing the dedicated file releases the OS advisory lock. It is NEVER renamed
// or removed, including logout; atomic auth.json replacement cannot split it.
impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(test)]
            before_save: None,
        }
    }
    #[cfg(test)]
    pub fn with_before_save(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.before_save = Some(hook);
        self
    }
    pub fn configured() -> Self {
        Self::new(crate::paths::provider_auth_file("antigravity"))
    }
    pub fn load(&self) -> Result<Option<Credential>> {
        match fs::symlink_metadata(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("unreadable"),
            Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 65536 => {
                return Err("invalid");
            }
            Ok(meta) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if meta.permissions().mode() & 0o077 != 0 {
                        return Err("unreadable");
                    }
                }
            }
        }
        let mut raw = Vec::new();
        File::open(&self.path)
            .map_err(|_| "unreadable")?
            .take(65537)
            .read_to_end(&mut raw)
            .map_err(|_| "unreadable")?;
        let value = replay::parse_json(&raw, 65536).map_err(|_| "invalid")?;
        let credential: Credential = serde_json::from_value(value).map_err(|_| "invalid")?;
        credential.validate()?;
        Ok(Some(credential))
    }
    pub async fn lock(&self, timeout: Duration) -> Result<Lock> {
        let parent = self.path.parent().ok_or("invalid auth directory")?;
        fs::create_dir_all(parent).map_err(|_| "auth directory creation failed")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                .map_err(|_| "auth directory permissions failed")?;
        }
        let path = parent.join("auth.lock");
        if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
            return Err("invalid auth lock");
        }
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path).map_err(|_| "auth lock open failed")?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(Lock {
                        file,
                        path: self.path.clone(),
                    });
                }
                Err(std::fs::TryLockError::WouldBlock)
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err("Antigravity auth lock timed out");
                }
                Err(_) => return Err("Antigravity auth lock failed"),
            }
        }
    }
    pub fn save(&self, lock: &Lock, credential: &Credential) -> Result<()> {
        if lock.path != self.path {
            return Err("wrong auth lock");
        }
        credential.validate()?;
        #[cfg(test)]
        if let Some(hook) = &self.before_save {
            hook();
        }
        crate::auth::write_atomically(self.path.to_str().ok_or("invalid auth path")?, credential)
            .map_err(|_| "auth write failed")
    }
    pub async fn logout(&self) -> Result<()> {
        let mut lock = self.lock(Duration::from_secs(10)).await?;
        // The store resolver is independent of transport configuration. Advance
        // the stable epoch even when the credential file is already absent.
        lock.advance()?;
        self.clear(&lock)
    }

    pub fn clear(&self, lock: &Lock) -> Result<()> {
        if lock.path != self.path {
            return Err("wrong auth lock");
        }
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("auth deletion failed"),
        }
    }
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) const LOGIN_SAVED: &str = "Antigravity credentials saved. Experimental native access and model entitlement are NOT verified; start a fresh fixed-model session.";

struct LoginAttempt {
    epoch: String,
    verifier: String,
    state: String,
}

pub(crate) struct Auth {
    pub store: Store,
    pub config: Config,
    pub client: reqwest::Client,
    refresh: Arc<RefreshManager>,
}
impl Auth {
    pub fn new(config: Config, store: Store) -> Result<Self> {
        let client = config.client()?;
        let refresh = RefreshManager::new(store.clone(), client.clone(), &config);
        Ok(Self {
            config,
            store,
            client,
            refresh,
        })
    }
    pub async fn access(&self) -> std::result::Result<Credential, AccessError> {
        self.refresh.access().await
    }
    pub async fn finish_maintenance(&self) -> std::result::Result<(), AccessError> {
        self.refresh.shutdown().await
    }
    #[cfg(test)]
    pub fn with_refresh_timeouts(mut self, lock: Duration, network: Duration) -> Self {
        self.refresh = self.refresh.with_timeouts(lock, network);
        self
    }
    pub async fn logout(&self) -> Result<()> {
        self.store.logout().await
    }
    #[cfg(any(unix, test))]
    pub(crate) async fn login_manual_with_completion<F, Fut>(&self, complete: F) -> Result<()>
    where
        F: FnOnce(url::Url, String) -> Fut,
        Fut: std::future::Future<Output = Result<String>>,
    {
        let attempt = self.begin_login().await?;
        let redirect = super::manual::REDIRECT;
        let authorization = authorization_url(
            &self.config,
            self.client_id()?,
            redirect,
            &attempt.state,
            &attempt.verifier,
        );
        // Completion owns terminal cleanup. No network or persistence before it
        // has successfully restored the terminal and returned a validated code.
        let input = complete(authorization, attempt.state.clone()).await?;
        let code = super::manual::parse_completion(&input, &attempt.state)?;
        self.exchange(attempt, redirect, &code).await
    }

    fn client_id(&self) -> Result<&str> {
        self.config
            .client_id
            .as_deref()
            .ok_or("set CCP_ANTIGRAVITY_CLIENT_ID to your public installed-app OAuth client ID")
    }

    async fn begin_login(&self) -> Result<LoginAttempt> {
        self.client_id()?;
        let mut lock = self.store.lock(Duration::from_secs(10)).await?;
        self.store.load()?;
        Ok(LoginAttempt {
            epoch: lock.epoch()?,
            verifier: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
            state: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
        })
    }

    pub async fn login(&self) -> Result<()> {
        self.login_with_opener(|authorization| {
            println!("Explicit browser consent for claude-code-proxy's experimental Antigravity adapter. No official CLI credentials are imported.\n{authorization}");
            open_browser(authorization.as_str());
            Ok(())
        }).await
    }
    pub(crate) async fn login_with_opener(
        &self,
        opener: impl FnOnce(&url::Url) -> Result<()>,
    ) -> Result<()> {
        let attempt = self.begin_login().await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|_| "OAuth callback bind failed")?;
        let redirect = format!(
            "http://127.0.0.1:{}/oauth/callback",
            listener
                .local_addr()
                .map_err(|_| "callback address unavailable")?
                .port()
        );
        let authorization = authorization_url(
            &self.config,
            self.client_id()?,
            &redirect,
            &attempt.state,
            &attempt.verifier,
        );
        opener(&authorization)?;
        let code =
            tokio::time::timeout(Duration::from_secs(180), callback(listener, &attempt.state))
                .await
                .map_err(|_| "OAuth callback timed out")??;
        self.exchange(attempt, &redirect, &code).await?;
        println!("{LOGIN_SAVED}");
        Ok(())
    }

    async fn exchange(&self, attempt: LoginAttempt, redirect: &str, code: &str) -> Result<()> {
        let client_id = self.client_id()?;
        let mut lock = self.store.lock(Duration::from_secs(10)).await?;
        if lock.epoch()? != attempt.epoch {
            return Err("Antigravity login changed during consent; start login again");
        }
        self.store.load()?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("redirect_uri", redirect),
            ("code", code),
            ("code_verifier", &attempt.verifier),
        ];
        if let Some(secret) = self.config.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let response = self
            .client
            .post(self.config.token_url.clone())
            .timeout(Duration::from_secs(30))
            .form(&form)
            .send()
            .await
            .map_err(|_| "OAuth token exchange transport failed")?;
        let token = bounded_json(response).await?;
        let mut credential = Credential {
            profile_id: uuid::Uuid::new_v4().to_string(),
            project_id: self.config.project.clone(),
            access_token: String::new(),
            refresh_token: None,
            expires_at: 0,
        };
        update_tokens(&mut credential, &token)?;
        if credential.project_id.is_none() {
            let response = self
                .client
                .post(
                    self.config
                        .base_url
                        .join("/v1internal:loadCodeAssist")
                        .map_err(|_| "invalid project discovery endpoint")?,
                )
                .timeout(Duration::from_secs(30))
                .bearer_auth(&credential.access_token)
                .headers(self.config.native_headers())
                .json(&json!({"metadata":{"ideType":"ANTIGRAVITY","pluginType":"GEMINI"}}))
                .send()
                .await
                .map_err(|_| "Antigravity project discovery transport failed")?;
            credential.project_id = Some(project_id(&bounded_json(response).await?)?);
        }
        lock.advance()?;
        self.store.save(&lock, &credential)?;
        Ok(())
    }
}

pub(crate) fn authorization_url(
    config: &Config,
    client_id: &str,
    redirect: &str,
    state: &str,
    verifier: &str,
) -> url::Url {
    let scopes = [
        "cloud-platform",
        "userinfo.email",
        "userinfo.profile",
        "cclog",
        "experimentsandconfigs",
    ]
    .map(|s| format!("https://www.googleapis.com/auth/{s}"))
    .join(" ");
    let mut url = config.authorization_url.clone();
    url.query_pairs_mut().extend_pairs([
        ("client_id", client_id),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("scope", &scopes),
        ("state", state),
        ("code_challenge_method", "S256"),
        (
            "code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        ),
    ]);
    url
}

fn open_browser(url: &str) {
    #[cfg(target_os = "linux")]
    let command = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    #[cfg(target_os = "macos")]
    let command = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let command = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    if let Ok(mut child) = command {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

pub(crate) async fn callback(listener: tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, peer) = listener
            .accept()
            .await
            .map_err(|_| "OAuth callback accept failed")?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let mut request = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(2), async {
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream
                    .read(&mut buffer)
                    .await
                    .map_err(|_| "callback read failed")?;
                if n == 0 || request.len() + n > 8192 {
                    return Err("invalid callback");
                }
                request.extend_from_slice(&buffer[..n]);
            }
            Ok(())
        })
        .await;
        if !matches!(read, Ok(Ok(()))) {
            continue;
        }
        let line = std::str::from_utf8(&request)
            .ok()
            .and_then(|r| r.lines().next())
            .unwrap_or("");
        let mut fields = line.split_whitespace();
        if fields.next() != Some("GET") {
            continue;
        }
        let target = fields.next().unwrap_or("");
        let url = match url::Url::parse(&format!("http://127.0.0.1{target}")) {
            Ok(url) => url,
            Err(_) => continue,
        };
        if url.path() != "/oauth/callback" {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let pairs = url.query_pairs().collect::<Vec<_>>();
        if pairs.iter().filter(|(k, _)| k == "state").count() != 1
            || pairs
                .iter()
                .find(|(k, _)| k == "state")
                .is_none_or(|(_, v)| v != state)
        {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let denied = pairs.iter().any(|(k, _)| k == "error");
        let code = pairs
            .iter()
            .filter(|(k, _)| k == "code")
            .collect::<Vec<_>>();
        let result = if denied {
            Err("OAuth consent denied")
        } else if code.len() != 1 || code[0].1.is_empty() {
            Err("invalid OAuth callback")
        } else {
            Ok(code[0].1.to_string())
        };
        let _=stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 29\r\nConnection: close\r\n\r\nReturn to claude-code-proxy.\n").await;
        return result;
    }
}

pub(crate) async fn bounded_json(mut response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        return Err(
            "Antigravity OAuth/project request denied; no fallback or onboarding attempted",
        );
    }
    let mut raw = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "OAuth/project response read failed")?
    {
        if raw.len() + chunk.len() > 65536 {
            return Err("OAuth/project response byte limit");
        }
        raw.extend_from_slice(&chunk);
    }
    replay::parse_json(&raw, 65536)
}

pub(super) fn update_tokens(credential: &mut Credential, value: &Value) -> Result<()> {
    credential.access_token = replay::nonempty(&value["access_token"])?.to_string();
    if value
        .get("token_type")
        .is_some_and(|v| !v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("bearer")))
    {
        return Err("unsupported OAuth token type");
    }
    let expires = value["expires_in"]
        .as_u64()
        .filter(|v| *v > 0 && *v <= 86400)
        .ok_or("invalid OAuth expiry")?;
    credential.expires_at = now().checked_add(expires).ok_or("invalid OAuth expiry")?;
    if let Some(refresh) = value.get("refresh_token") {
        credential.refresh_token = Some(replay::nonempty(refresh)?.to_string());
    }
    credential.validate()
}

pub(crate) fn project_id(value: &Value) -> Result<String> {
    let project = &value["cloudaicompanionProject"];
    let id=project.as_str().or_else(||project["id"].as_str()).filter(|s|!s.is_empty() && s.len()<=4096).ok_or("Antigravity project missing; configure your authorized project, no onboarding was attempted")?;
    Ok(id.to_string())
}

pub(crate) fn descriptor(config: &Result<Config>, store: &Store) -> Value {
    let valid = config.is_ok();
    let credential = store.load();
    let project = config
        .as_ref()
        .ok()
        .and_then(|c| c.project.as_ref())
        .is_some()
        || credential
            .as_ref()
            .ok()
            .and_then(|c| c.as_ref())
            .is_some_and(|c| c.project_id.is_some());
    let auth = match &credential {
        Err(error) => *error,
        Ok(None) => "missing",
        Ok(Some(c)) if c.expires_at > now().saturating_add(60) => "ready",
        Ok(Some(c)) if c.refresh_token.is_some() => "refresh_required",
        Ok(Some(_)) => "login_required",
    };
    json!({"supported":true,"enabled":config.as_ref().is_ok_and(|c|c.enabled),"mode":"native","experimental":true,
        "supported_apis":["messages","count_tokens"],"session_policy":"fixed-model","context_window":200000,"backend_window_verified":false,
        "status":{"configuration":if valid {"valid"} else {"invalid"},"auth":auth,"project":if project {"configured"} else {"missing"},
            "configuration_ready":valid && project && (auth=="ready" || (auth=="refresh_required" && config.as_ref().is_ok_and(|c|c.client_id.is_some()))),"live_verified":false}})
}
