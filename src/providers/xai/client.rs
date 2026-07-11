use std::time::Duration;

use crate::config;
use crate::retry::{MAX_RATE_LIMIT_RETRIES, compute_backoff_delay};

use super::auth::constants::{TIER_DENIED_HINT, api_base_url};
use super::auth::manager::XaiAuthManager;
use super::auth::token_store::{StoredAuth, file_store};
use super::continuation::ContinuationCandidate;
use super::translate::request::ResponsesRequest;

#[derive(Debug)]
pub struct XaiError {
    pub status: u16,
    pub message: String,
    pub detail: Option<String>,
    pub retry_after: Option<String>,
    /// When true, caller should treat this as OAuth entitlement failure (not a bad login).
    pub tier_denied: bool,
}

pub struct XaiResponse {
    pub body: Vec<u8>,
    pub status: u16,
}

pub struct XaiHttpClient {
    client: reqwest::blocking::Client,
    auth_manager: XaiAuthManager<crate::auth::FileAuthStore<StoredAuth>>,
}

impl Default for XaiHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl XaiHttpClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()
                .expect("failed to create HTTP client"),
            auth_manager: XaiAuthManager::new(file_store()),
        }
    }

    pub fn post_responses(
        &self,
        body: &ResponsesRequest,
        continuation: Option<&ContinuationCandidate>,
    ) -> Result<XaiResponse, XaiError> {
        let mut body_json = serde_json::to_value(body).map_err(|e| XaiError {
            status: 500,
            message: "Failed to serialize request".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            tier_denied: false,
        })?;
        apply_continuation(&mut body_json, continuation);

        let body_str = serde_json::to_string(&body_json).map_err(|e| XaiError {
            status: 500,
            message: "Failed to serialize request".to_string(),
            detail: Some(e.to_string()),
            retry_after: None,
            tier_denied: false,
        })?;

        // Prefer OAuth; fall back to API key when missing OAuth or on tier 403.
        let oauth_result = self.auth_manager.get_auth();
        if let Ok(mut auth) = oauth_result {
            let mut attempt = 0u32;
            loop {
                match self.attempt_post(&auth.access, &body_str) {
                    Ok(response) if response.status == 401 && attempt == 0 => {
                        match self.auth_manager.force_refresh() {
                            Ok(new_auth) => {
                                auth = new_auth;
                                attempt += 1;
                                continue;
                            }
                            Err(e) => {
                                let msg = e.to_string();
                                // Refresh already explained tier denial; try API key if set.
                                if msg.contains("403") || msg.contains("SuperGrok") {
                                    if let Some(resp) = self.try_api_key_fallback(&body_str) {
                                        return resp;
                                    }
                                    return Err(XaiError {
                                        status: 403,
                                        message: TIER_DENIED_HINT.to_string(),
                                        detail: Some(msg),
                                        retry_after: None,
                                        tier_denied: true,
                                    });
                                }
                                return Err(XaiError {
                                    status: 401,
                                    message: "Unauthorized".to_string(),
                                    detail: Some(msg),
                                    retry_after: None,
                                    tier_denied: false,
                                });
                            }
                        }
                    }
                    Ok(response) if response.status == 403 => {
                        // Entitlement: keep OAuth tokens; try API key once.
                        if let Some(resp) = self.try_api_key_fallback(&body_str) {
                            return resp;
                        }
                        return Err(map_error_response(403, &response.body));
                    }
                    Ok(response) if response.status == 429 => {
                        if attempt < MAX_RATE_LIMIT_RETRIES {
                            let delay = compute_backoff_delay(attempt, None);
                            std::thread::sleep(Duration::from_millis(delay.wait_ms));
                            attempt += 1;
                            continue;
                        }
                        return Err(XaiError {
                            status: 429,
                            message: "Rate limited".to_string(),
                            detail: Some(String::from_utf8_lossy(&response.body).into_owned()),
                            retry_after: Some("5".into()),
                            tier_denied: false,
                        });
                    }
                    Ok(response) if response.status >= 400 => {
                        return Err(map_error_response(response.status, &response.body));
                    }
                    Ok(response) => return Ok(response),
                    Err(err) if err.status == 429 && attempt < MAX_RATE_LIMIT_RETRIES => {
                        let delay = compute_backoff_delay(attempt, err.retry_after.as_deref());
                        std::thread::sleep(Duration::from_millis(delay.wait_ms));
                        attempt += 1;
                    }
                    Err(err) => return Err(err),
                }
            }
        }

        if let Some(resp) = self.try_api_key_fallback(&body_str) {
            return resp;
        }

        let oauth_err = oauth_result.err().map(|e| e.to_string());
        Err(XaiError {
            status: 401,
            message: "Not authenticated".to_string(),
            detail: Some(oauth_err.unwrap_or_else(|| {
                "Run `claude-code-proxy xai auth login` (or `xai auth device`), \
                 or set CCP_XAI_API_KEY / XAI_API_KEY"
                    .into()
            })),
            retry_after: None,
            tier_denied: false,
        })
    }

    fn try_api_key_fallback(&self, body: &str) -> Option<Result<XaiResponse, XaiError>> {
        let api_key = config::xai_api_key()?;
        Some(match self.attempt_post(&api_key, body) {
            Ok(response) if response.status >= 400 => {
                Err(map_error_response(response.status, &response.body))
            }
            Ok(response) => Ok(response),
            Err(err) => Err(err),
        })
    }

    fn attempt_post(&self, bearer: &str, body: &str) -> Result<XaiResponse, XaiError> {
        // api_base_url() already pins to HTTPS *.x.ai with default fallback.
        let base = api_base_url();
        let url = format!("{base}/responses");

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream, application/json")
            .header("Authorization", format!("Bearer {bearer}"))
            .header("x-grok-source", "claude-code-proxy")
            .body(body.to_string())
            .send()
            .map_err(|e| XaiError {
                status: 0,
                message: "Network error".to_string(),
                detail: Some(e.to_string()),
                retry_after: None,
                tier_denied: false,
            })?;

        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = resp.bytes().map(|b| b.to_vec()).unwrap_or_default();

        if status == 429 {
            return Err(XaiError {
                status: 429,
                message: "Rate limited".to_string(),
                detail: Some(String::from_utf8_lossy(&body).into_owned()),
                retry_after,
                tier_denied: false,
            });
        }

        Ok(XaiResponse { body, status })
    }
}

fn apply_continuation(
    body: &mut serde_json::Value,
    continuation: Option<&ContinuationCandidate>,
) {
    let Some(candidate) = continuation else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if let Some(ref prev_id) = candidate.previous_response_id {
        obj.insert(
            "previous_response_id".to_string(),
            serde_json::json!(prev_id),
        );
    }
    if let Some(ref delta) = candidate.input_delta {
        obj.insert(
            "input".to_string(),
            serde_json::to_value(delta).unwrap_or_default(),
        );
    }
}

fn map_error_response(status: u16, body: &[u8]) -> XaiError {
    let text = String::from_utf8_lossy(body).into_owned();
    if status == 403 {
        return XaiError {
            status: 403,
            message: TIER_DENIED_HINT.to_string(),
            detail: Some(text),
            retry_after: None,
            tier_denied: true,
        };
    }
    let message = if status == 401 {
        "Authentication failed. Run `claude-code-proxy xai auth login` or set CCP_XAI_API_KEY."
            .to_string()
    } else {
        "Upstream error".to_string()
    };
    XaiError {
        status,
        message,
        detail: Some(text),
        retry_after: None,
        tier_denied: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::xai::auth::constants::pin_xai_https_origin;

    #[test]
    fn pin_accepts_api_x_ai() {
        assert!(pin_xai_https_origin("https://api.x.ai/v1", "https://api.x.ai/v1").is_ok());
    }

    #[test]
    fn pin_rejects_http_and_foreign_hosts() {
        assert!(pin_xai_https_origin("http://api.x.ai/v1", "https://api.x.ai/v1").is_err());
        assert!(pin_xai_https_origin("https://evil.example/v1", "https://api.x.ai/v1").is_err());
    }

    #[test]
    fn map_403_is_tier_denied() {
        let err = map_error_response(403, br#"{"error":"permission"}"#);
        assert!(err.tier_denied);
        assert!(err.message.contains("CCP_XAI_API_KEY"));
    }
}
