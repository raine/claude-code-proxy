//! Read-only Codex subscription limits, separate from proxy request accounting.
//!
//! GET https://chatgpt.com/backend-api/wham/usage is the Codex backend-client
//! usage endpoint (openai/codex: codex-rs/backend-client/src/client/rate_limit_resets.rs).
//! Never generates tokens, refreshes OAuth tokens, writes credentials, or selects
//! a profile. Expired credentials require login/inference to refresh them: monitor
//! refresh must not race the inference provider's rotating refresh token.
use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};

use crate::account_profiles::ProfileStore;
use crate::providers::codex::auth::token_store::{StoredAuth, auth_for_profile};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 4;

/// Safe snapshot data: no credentials, account identifiers, paths, or upstream errors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccountLimit {
    pub provider: String,
    pub name: String,
    pub status: String,
    pub five_hour: Option<LimitWindow>,
    pub seven_day: Option<LimitWindow>,
    /// Last successful usage retrieval, not the last failed attempt.
    pub updated_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LimitWindow {
    pub used_percent: f64,
    pub reset_at_ms: Option<u64>,
}

struct CachedLimit {
    identity: u64,
    row: AccountLimit,
}

pub struct AccountLimitsCollector {
    client: Option<reqwest::Client>,
    endpoint: String,
    cache: BTreeMap<String, CachedLimit>,
    last_refresh: Option<Instant>,
}

impl Default for AccountLimitsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountLimitsCollector {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .https_only(true)
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(REQUEST_TIMEOUT)
                .user_agent(concat!("claude-code-proxy/", env!("CARGO_PKG_VERSION")))
                .build()
                .ok(),
            endpoint: USAGE_URL.to_owned(),
            cache: BTreeMap::new(),
            last_refresh: None,
        }
    }

    /// Call from the monitor's background task, not while holding its snapshot lock.
    /// Internally throttled to once a minute. Each profile uses its own saved auth,
    /// regardless of runtime selection. Failures retain last successful windows.
    pub async fn refresh(&mut self) -> Vec<AccountLimit> {
        self.refresh_profiles(&ProfileStore::configured()).await
    }

    async fn refresh_profiles(&mut self, profiles: &ProfileStore) -> Vec<AccountLimit> {
        if self
            .last_refresh
            .is_some_and(|last| last.elapsed() < REFRESH_INTERVAL)
        {
            return self.rows();
        }
        // Directory failure must not erase previously collected data.
        let Ok(names) = profiles.list("codex") else {
            for cached in self.cache.values_mut() {
                cached.row.status = "Stale: cannot read profiles".to_owned();
            }
            self.last_refresh = Some(Instant::now());
            return self.rows();
        };
        self.refresh_named_profiles(profiles, names).await
    }

    async fn refresh_named_profiles(
        &mut self,
        profiles: &ProfileStore,
        names: Vec<String>,
    ) -> Vec<AccountLimit> {
        if self
            .last_refresh
            .is_some_and(|last| last.elapsed() < REFRESH_INTERVAL)
        {
            return self.rows();
        }
        let now = now_ms();
        let mut pending = Vec::new();
        let mut next = BTreeMap::new();
        for name in names {
            // Only locally parseable, logged-in Codex profiles participate. A
            // missing/corrupt file is not a fabricated zero-percent account.
            let Ok(Some(auth)) = auth_for_profile(profiles, &name) else {
                continue;
            };
            if auth.access.trim().is_empty() || auth.refresh.trim().is_empty() {
                continue;
            }
            let identity = auth_identity(&auth);
            let row = self
                .cache
                .remove(&name)
                .filter(|cached| cached.identity == identity)
                .map(|cached| cached.row)
                .unwrap_or(AccountLimit {
                    provider: "codex".to_owned(),
                    name: name.clone(),
                    status: "Not yet retrieved".to_owned(),
                    five_hour: None,
                    seven_day: None,
                    updated_at_ms: None,
                });
            if auth.expires <= now {
                let mut row = row;
                row.status = "Expired auth (no monitor refresh)".to_owned();
                next.insert(name, CachedLimit { identity, row });
            } else {
                pending.push((name, identity, row, auth));
            }
        }
        let client = self.client.as_ref();
        let endpoint = self.endpoint.as_str();
        let mut results = stream::iter(pending.into_iter().map(
            |(name, identity, mut row, auth)| async move {
                let result = match client {
                    Some(client) => fetch_usage(client, endpoint, &auth).await,
                    None => Err("Usage client unavailable"),
                };
                match result {
                    Ok((five_hour, seven_day)) => {
                        row.five_hour = five_hour;
                        row.seven_day = seven_day;
                        row.updated_at_ms = Some(now_ms());
                        row.status = if row.five_hour.is_some() || row.seven_day.is_some() {
                            "Available"
                        } else {
                            "Limits unavailable"
                        }
                        .to_owned();
                    }
                    Err(status) => {
                        row.status = if row.updated_at_ms.is_some() {
                            format!("Stale: {status}")
                        } else {
                            status.to_owned()
                        };
                    }
                }
                (name, CachedLimit { identity, row })
            },
        ))
        .buffer_unordered(MAX_CONCURRENT_REQUESTS);
        while let Some((name, cached)) = results.next().await {
            next.insert(name, cached);
        }
        self.cache = next;
        self.last_refresh = Some(Instant::now());
        self.rows()
    }

    fn rows(&self) -> Vec<AccountLimit> {
        self.cache
            .values()
            .map(|cached| cached.row.clone())
            .collect()
    }

    /// Test-only injection: literal loopback addresses only, no DNS, redirects,
    /// ambient proxies, or production configuration override for credential URLs.
    #[cfg(test)]
    fn for_loopback(endpoint: &str) -> Self {
        let url = reqwest::Url::parse(endpoint).unwrap();
        let host: std::net::IpAddr = url
            .host_str()
            .unwrap()
            .trim_matches(['[', ']'])
            .parse()
            .unwrap();
        assert!(host.is_loopback());
        assert!(matches!(url.scheme(), "http" | "https"));
        assert!(url.username().is_empty() && url.password().is_none());
        Self {
            client: Some(
                reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(REQUEST_TIMEOUT)
                    .build()
                    .unwrap(),
            ),
            endpoint: endpoint.to_owned(),
            cache: BTreeMap::new(),
            last_refresh: None,
        }
    }
}

fn now_ms() -> u64 {
    (time::OffsetDateTime::now_utc()
        .unix_timestamp_nanos()
        .max(0)
        / 1_000_000) as u64
}

// A private, nonserialized discriminator prevents retaining another account's
// windows when a named profile is overwritten. Prefer account ID across rotations.
fn auth_identity(auth: &StoredAuth) -> u64 {
    let mut hasher = DefaultHasher::new();
    auth.account_id
        .as_deref()
        .unwrap_or(&auth.access)
        .hash(&mut hasher);
    hasher.finish()
}

async fn fetch_usage(
    client: &reqwest::Client,
    endpoint: &str,
    auth: &StoredAuth,
) -> Result<(Option<LimitWindow>, Option<LimitWindow>), &'static str> {
    let mut authorization =
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", auth.access))
            .map_err(|_| "Invalid auth headers")?;
    authorization.set_sensitive(true);
    let mut request = client
        .get(endpoint)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .header(reqwest::header::ACCEPT, "application/json");
    if let Some(account_id) = auth.account_id.as_ref() {
        let mut value = reqwest::header::HeaderValue::from_str(account_id)
            .map_err(|_| "Invalid auth headers")?;
        value.set_sensitive(true);
        request = request.header("chatgpt-account-id", value);
    }
    // Never expose reqwest errors, redirect locations, or upstream bodies.
    let mut response = request.send().await.map_err(|_| "Usage request failed")?;
    match response.status().as_u16() {
        200..=299 => {}
        401 | 403 => return Err("Authentication rejected"),
        429 => return Err("Usage endpoint rate limited"),
        _ => return Err("Usage endpoint unavailable"),
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Usage response failed")?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err("Invalid usage response");
        }
        body.extend_from_slice(&chunk);
    }
    parse_usage(&body)
}

fn parse_usage(raw: &[u8]) -> Result<(Option<LimitWindow>, Option<LimitWindow>), &'static str> {
    let value: serde_json::Value =
        serde_json::from_slice(raw).map_err(|_| "Invalid usage response")?;
    if !value.is_object() {
        return Err("Invalid usage response");
    }
    let Some(rate_limit) = value.get("rate_limit") else {
        return Ok((None, None));
    };
    if rate_limit.is_null() {
        return Ok((None, None));
    }
    if !rate_limit.is_object() {
        return Err("Invalid usage response");
    }
    let mut five_hour = None;
    let mut seven_day = None;
    for key in ["primary_window", "secondary_window"] {
        let Some(window) = rate_limit.get(key).filter(|window| !window.is_null()) else {
            continue;
        };
        let slot = match window
            .get("limit_window_seconds")
            .and_then(serde_json::Value::as_u64)
        {
            Some(18_000) => &mut five_hour,
            Some(604_800) => &mut seven_day,
            // Never infer a duration from primary/secondary ordering.
            _ => continue,
        };
        let used_percent = window
            .get("used_percent")
            .and_then(serde_json::Value::as_f64)
            .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
            .ok_or("Invalid usage response")?;
        let reset_at_ms = match window.get("reset_at") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|seconds| seconds.checked_mul(1000))
                    .ok_or("Invalid usage response")?,
            ),
        };
        *slot = Some(LimitWindow {
            used_percent,
            reset_at_ms,
        });
    }
    Ok((five_hour, seven_day))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(
        value: serde_json::Value,
    ) -> Result<(Option<LimitWindow>, Option<LimitWindow>), &'static str> {
        parse_usage(&serde_json::to_vec(&value).unwrap())
    }

    #[test]
    fn windows_are_classified_by_duration_not_order() {
        let (five, seven) = parse(json!({"rate_limit": {
            "primary_window": {"limit_window_seconds": 604800, "used_percent": 73.5, "reset_at": 1700000000},
            "secondary_window": {"limit_window_seconds": 18000, "used_percent": 12, "reset_at": 1700000010}
        }})).unwrap();
        assert_eq!(
            five.unwrap(),
            LimitWindow {
                used_percent: 12.0,
                reset_at_ms: Some(1700000010000)
            }
        );
        assert_eq!(
            seven.unwrap(),
            LimitWindow {
                used_percent: 73.5,
                reset_at_ms: Some(1700000000000)
            }
        );
    }

    #[test]
    fn unknown_missing_and_null_windows_do_not_fabricate_limits() {
        for value in [
            json!({}),
            json!({"rate_limit": null}),
            json!({"rate_limit": {
                "primary_window": {"used_percent": 10},
                "secondary_window": {"limit_window_seconds": 3600, "used_percent": 20}
            }}),
        ] {
            assert_eq!(parse(value).unwrap(), (None, None));
        }
        let (five, seven) = parse(json!({"rate_limit": {
            "primary_window": null,
            "secondary_window": {"limit_window_seconds": 18000, "used_percent": 0}
        }}))
        .unwrap();
        assert_eq!(five.unwrap().reset_at_ms, None);
        assert_eq!(seven, None);
    }

    #[test]
    fn invalid_percentages_and_resets_have_sanitized_errors() {
        for used in [json!(-1), json!(100.1), json!("secret-token"), json!(null)] {
            let result = parse(json!({"rate_limit": {"primary_window": {
                "limit_window_seconds": 18000, "used_percent": used
            }}}));
            assert_eq!(result.unwrap_err(), "Invalid usage response");
        }
        for reset in [
            json!(-1),
            json!(1.5),
            json!(u64::MAX),
            json!("secret-account"),
        ] {
            assert!(
                parse(json!({"rate_limit": {"primary_window": {
                    "limit_window_seconds": 18000, "used_percent": 100, "reset_at": reset
                }}}))
                .is_err()
            );
        }
        for raw in [b"secret-access".as_slice(), b"[]", b"{\"rate_limit\":\"secret\"}",
            b"{\"rate_limit\":{\"primary_window\":{\"limit_window_seconds\":18000,\"used_percent\":NaN}}}"] {
            assert_eq!(parse_usage(raw).unwrap_err(), "Invalid usage response");
        }
    }

    fn save_profile(profiles: &ProfileStore, name: &str, expires: u64) {
        let path = profiles.credential_path("codex", name).unwrap();
        crate::auth::write_atomically(
            &path.to_string_lossy(),
            &json!({
                "access": format!("secret-access-{name}"), "refresh": "secret-refresh",
                "expires": expires, "accountId": format!("secret-account-{name}")
            }),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn per_profile_auth_stale_cache_and_expired_profiles() {
        use axum::{Router, extract::State, http::HeaderMap, routing::get};
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        type TestState = (Arc<Mutex<Vec<(String, String)>>>, Arc<AtomicBool>);
        async fn usage(
            State((seen, fail)): State<TestState>,
            headers: HeaderMap,
        ) -> (http::StatusCode, axum::Json<serde_json::Value>) {
            seen.lock().unwrap().push((
                headers["authorization"].to_str().unwrap().to_owned(),
                headers["chatgpt-account-id"].to_str().unwrap().to_owned(),
            ));
            if fail.load(Ordering::Relaxed) {
                return (
                    http::StatusCode::INTERNAL_SERVER_ERROR,
                    axum::Json(json!({"error": "secret-response"})),
                );
            }
            (
                http::StatusCode::OK,
                axum::Json(json!({"rate_limit": {"primary_window": {
                    "limit_window_seconds": 18000, "used_percent": 25, "reset_at": 1700000000
                }}})),
            )
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let app = Router::new()
            .route("/usage", get(usage))
            .with_state((seen.clone(), fail.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/usage", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let profiles = ProfileStore::new(dir.path().to_owned());
        save_profile(&profiles, "work", u64::MAX);
        save_profile(&profiles, "personal", u64::MAX);
        save_profile(&profiles, "expired", 1);
        let work_path = profiles.credential_path("codex", "work").unwrap();
        let original_auth = std::fs::read(&work_path).unwrap();
        let mut collector = AccountLimitsCollector::for_loopback(&endpoint);
        let rows = collector
            .refresh_named_profiles(
                &profiles,
                vec!["work".into(), "personal".into(), "expired".into()],
            )
            .await;
        let named: Vec<_> = rows.iter().filter(|row| row.name != "default").collect();
        assert_eq!(named.len(), 3);
        assert_eq!(seen.lock().unwrap().len(), 2);
        for name in ["work", "personal"] {
            assert!(seen.lock().unwrap().contains(&(
                format!("Bearer secret-access-{name}"),
                format!("secret-account-{name}")
            )));
        }
        assert!(
            rows.iter()
                .find(|row| row.name == "expired")
                .unwrap()
                .status
                .starts_with("Expired")
        );
        assert!(!serde_json::to_string(&rows).unwrap().contains("secret"));
        collector
            .refresh_named_profiles(
                &profiles,
                vec!["work".into(), "personal".into(), "expired".into()],
            )
            .await;
        assert_eq!(seen.lock().unwrap().len(), 2, "refresh is throttled");
        fail.store(true, Ordering::Relaxed);
        collector.last_refresh = None;
        let stale = collector
            .refresh_named_profiles(
                &profiles,
                vec!["work".into(), "personal".into(), "expired".into()],
            )
            .await;
        let work = stale.iter().find(|row| row.name == "work").unwrap();
        assert!(work.status.starts_with("Stale:"));
        assert_eq!(work.five_hour.as_ref().unwrap().used_percent, 25.0);
        assert_eq!(
            work.updated_at_ms,
            rows.iter()
                .find(|row| row.name == "work")
                .unwrap()
                .updated_at_ms
        );
        assert!(!serde_json::to_string(&stale).unwrap().contains("secret"));
        assert_eq!(
            std::fs::read(&work_path).unwrap(),
            original_auth,
            "monitor must never rotate auth"
        );
        // Replacing a saved login must not display the previous account's cache.
        crate::auth::write_atomically(
            &work_path.to_string_lossy(),
            &json!({
                "access": "new-secret-access", "refresh": "new-secret-refresh",
                "expires": u64::MAX, "accountId": "different-secret-account"
            }),
        )
        .unwrap();
        collector.last_refresh = None;
        let replaced = collector
            .refresh_named_profiles(&profiles, vec!["work".into()])
            .await;
        assert_eq!(replaced.len(), 1);
        assert_eq!(replaced[0].five_hour, None);
        assert_eq!(replaced[0].updated_at_ms, None);
        server.abort();
    }

    #[tokio::test]
    async fn redirects_are_not_followed() {
        use axum::{Router, response::Redirect, routing::get};
        let app = Router::new().route(
            "/usage",
            get(|| async { Redirect::temporary("http://127.0.0.1:1/leak") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/usage", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let collector = AccountLimitsCollector::for_loopback(&endpoint);
        let auth = StoredAuth {
            access: "secret".into(),
            refresh: "secret".into(),
            expires: u64::MAX,
            account_id: None,
        };
        assert_eq!(
            fetch_usage(collector.client.as_ref().unwrap(), &endpoint, &auth)
                .await
                .unwrap_err(),
            "Usage endpoint unavailable"
        );
        server.abort();
    }
}
