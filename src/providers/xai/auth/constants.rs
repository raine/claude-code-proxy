use crate::config;

pub const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
pub const SCOPE: &str = "openid profile email offline_access grok-cli:access api:access";
pub const DEFAULT_ISSUER: &str = "https://auth.x.ai";
pub const DEFAULT_API_BASE: &str = "https://api.x.ai/v1";
/// Canonical loopback port used by OpenCode / Kilo / ecosystem for xAI PKCE.
pub const OAUTH_PORT: u16 = 56121;
pub const OAUTH_CALLBACK_PATH: &str = "/callback";
/// Refresh up to 1h early (access tokens ~6h on SuperGrok flows; same policy as Hermes).
pub const REFRESH_MARGIN_MS: u64 = 60 * 60 * 1000;
pub const DEVICE_POLL_SAFETY_MARGIN_MS: u64 = 500;
pub const GRANT_DEVICE_CODE: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Shared copy for SuperGrok OAuth tier gating (refresh or inference HTTP 403).
/// Re-login usually will not fix this; API key is the escape hatch.
pub const TIER_DENIED_HINT: &str = "\
xAI returned 403 for this SuperGrok/X Premium+ OAuth session. \
xAI may restrict OAuth API access by tier even when the in-app subscription is active. \
Re-running `xai auth login` usually will not fix this. \
Set CCP_XAI_API_KEY or XAI_API_KEY as a fallback, or check your plan at https://x.ai/grok.";

pub fn issuer() -> String {
    let raw = config::xai_oauth_issuer();
    match pin_xai_https_origin(&raw, DEFAULT_ISSUER) {
        Ok(url) => url,
        Err(reason) => {
            eprintln!("warning: ignoring xAI oauth issuer override ({reason}); using {DEFAULT_ISSUER}");
            DEFAULT_ISSUER.to_string()
        }
    }
}

pub fn api_base_url() -> String {
    let raw = config::xai_base_url();
    match pin_xai_https_origin(&raw, DEFAULT_API_BASE) {
        Ok(url) => url.trim_end_matches('/').to_string(),
        Err(reason) => {
            eprintln!("warning: ignoring xAI base URL override ({reason}); using {DEFAULT_API_BASE}");
            DEFAULT_API_BASE.to_string()
        }
    }
}

/// Pin OAuth/inference origins to HTTPS xAI hosts. On failure returns Err reason
/// so callers can fall back to defaults (Hermes-style: never ship tokens off-host).
pub fn pin_xai_https_origin(candidate: &str, fallback: &str) -> Result<String, String> {
    let trimmed = candidate.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(fallback.to_string());
    }
    let url = url::Url::parse(trimmed).map_err(|e| format!("invalid URL: {e}"))?;
    if url.scheme() != "https" {
        return Err(format!("non-HTTPS origin: {trimmed}"));
    }
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host == "x.ai" || host.ends_with(".x.ai") {
        Ok(trimmed.to_string())
    } else {
        Err(format!("host {host:?} is not an xAI origin"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_accepts_api_x_ai() {
        assert_eq!(
            pin_xai_https_origin("https://api.x.ai/v1/", DEFAULT_API_BASE).unwrap(),
            "https://api.x.ai/v1"
        );
    }

    #[test]
    fn pin_rejects_foreign_and_http() {
        assert!(pin_xai_https_origin("http://api.x.ai/v1", DEFAULT_API_BASE).is_err());
        assert!(pin_xai_https_origin("https://evil.example/v1", DEFAULT_API_BASE).is_err());
    }
}
