use super::replay::{Result, keys, parse_json};
use crate::environment::ANTIGRAVITY_USER_AGENT;
use http::{HeaderMap, HeaderValue, header::USER_AGENT};
use serde_json::{Value, json};
use std::{ffi::OsString, path::Path};
use url::Url;

pub(super) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone)]
pub(crate) struct Config {
    pub enabled: bool,
    pub base_url: Url,
    pub token_url: Url,
    pub authorization_url: Url,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub project: Option<String>,
    pub native_user_agent: HeaderValue,
}

fn default_user_agent() -> String {
    format!("claude-code-proxy/{}", env!("CARGO_PKG_VERSION"))
}

fn parse_user_agent(value: &str) -> Result<HeaderValue> {
    if value.is_empty()
        || value.len() > 4096
        || value.starts_with(' ')
        || value.ends_with(' ')
        || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
    {
        return Err("invalid Antigravity user agent");
    }
    let mut header = HeaderValue::from_str(value).map_err(|_| "invalid Antigravity user agent")?;
    header.set_sensitive(true);
    Ok(header)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: Url::parse("https://daily-cloudcode-pa.googleapis.com").unwrap(),
            token_url: Url::parse("https://oauth2.googleapis.com/token").unwrap(),
            authorization_url: Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap(),
            client_id: None,
            client_secret: None,
            project: None,
            native_user_agent: parse_user_agent(&default_user_agent())
                .expect("built-in proxy identity is a valid header"),
        }
    }
}

impl Config {
    pub fn validate_endpoint(raw: &str, test_loopback: bool) -> Result<Url> {
        let url = Url::parse(raw).map_err(|_| "invalid Antigravity endpoint")?;
        let loopback = url.host_str().is_some_and(|host| {
            host.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        if !(url.scheme() == "https" || (test_loopback && loopback && url.scheme() == "http"))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
        {
            return Err("invalid Antigravity endpoint");
        }
        Ok(url)
    }

    pub fn load() -> Result<Self> {
        Self::read(&crate::paths::config_dir().join("config.json"), |key| {
            std::env::var_os(key)
        })
    }

    pub fn read(path: &Path, env: impl Fn(&str) -> Option<OsString>) -> Result<Self> {
        // Preserve absence versus invalid Unicode for this header only. Other
        // settings retain their existing non-Unicode-as-absent behavior.
        let user_agent = env(ANTIGRAVITY_USER_AGENT);
        let env = |key: &str| env(key).and_then(|value| value.into_string().ok());
        let file = match std::fs::read(path) {
            Ok(raw) => {
                parse_json(&raw, 1024 * 1024).map_err(|_| "invalid Antigravity configuration")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
            Err(_) => return Err("unreadable Antigravity configuration"),
        };
        if !file.is_object() {
            return Err("invalid Antigravity configuration root");
        }
        let section = file
            .get("antigravity")
            .cloned()
            .unwrap_or_else(|| json!({}));
        keys(
            &section,
            &[
                "enabled",
                "baseUrl",
                "clientId",
                "clientSecret",
                "projectId",
                "userAgent",
            ],
            &[],
        )?;
        let mut result = Self::default();
        let file_user_agent = section
            .get("userAgent")
            .map(|value| value.as_str().ok_or("invalid Antigravity user agent"))
            .transpose()?;
        if let Some(value) = user_agent.as_ref() {
            result.native_user_agent =
                parse_user_agent(value.to_str().ok_or("invalid Antigravity user agent")?)?;
        } else if let Some(value) = file_user_agent {
            result.native_user_agent = parse_user_agent(value)?;
        }
        if let Some(enabled) = section.get("enabled") {
            result.enabled = enabled.as_bool().ok_or("invalid enabled flag")?;
        }
        if let Some(enabled) = env("CCP_ANTIGRAVITY_ENABLED") {
            result.enabled = match enabled.as_str() {
                "1" | "true" => true,
                "0" | "false" => false,
                _ => return Err("invalid enabled flag"),
            };
        }
        fn setting(section: &Value, key: &str, supplied: Option<String>) -> Result<Option<String>> {
            let value = supplied.or(section
                .get(key)
                .map(|v| v.as_str().unwrap_or("").to_owned()));
            if value
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 4096 || v.chars().any(char::is_control))
            {
                return Err("invalid Antigravity setting");
            }
            Ok(value)
        }
        if let Some(base) = setting(&section, "baseUrl", env("CCP_ANTIGRAVITY_BASE_URL"))? {
            result.base_url = Self::validate_endpoint(&base, false)?;
            if result.base_url.path() != "/" {
                return Err("Antigravity base URL must be an origin");
            }
        }
        result.client_id = setting(&section, "clientId", env("CCP_ANTIGRAVITY_CLIENT_ID"))?;
        result.client_secret = setting(
            &section,
            "clientSecret",
            env("CCP_ANTIGRAVITY_CLIENT_SECRET"),
        )?;
        result.project = setting(&section, "projectId", env("CCP_ANTIGRAVITY_PROJECT_ID"))?;
        Ok(result)
    }

    pub fn native_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, self.native_user_agent.clone());
        headers
    }

    pub fn client_builder(&self) -> reqwest::ClientBuilder {
        reqwest::Client::builder()
            .retry(reqwest::retry::never())
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(std::time::Duration::from_secs(180))
            .user_agent(default_user_agent())
    }

    pub fn client(&self) -> Result<reqwest::Client> {
        self.client_builder()
            .build()
            .map_err(|_| "Antigravity HTTP client initialization failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_consumer_default_origin_preserves_oauth_and_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let mut configurations = vec![Config::default(), Config::read(&path, |_| None).unwrap()];
        std::fs::write(&path, br#"{"antigravity":{}}"#).unwrap();
        configurations.push(Config::read(&path, |_| None).unwrap());
        for config in configurations {
            assert_eq!(
                config.base_url.as_str(),
                "https://daily-cloudcode-pa.googleapis.com/"
            );
            assert_eq!(
                config.token_url.as_str(),
                "https://oauth2.googleapis.com/token"
            );
            assert_eq!(
                config.authorization_url.as_str(),
                "https://accounts.google.com/o/oauth2/v2/auth"
            );
            assert_eq!(config.native_headers()["user-agent"], default_user_agent());
            assert!(!config.enabled);
        }
    }

    #[test]
    fn antigravity_consumer_base_url_precedence_preserves_explicit_origins() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let daily = "https://daily-cloudcode-pa.googleapis.com";
        let production = "https://cloudcode-pa.googleapis.com";
        let custom = "https://cloudcode-pa.googleapis.com.example.invalid:8443";
        for (file, environment, expected) in [
            (None, None, daily),
            (Some(production), None, production),
            (Some(custom), None, custom),
            (Some(daily), Some(production), production),
            (Some(production), Some(custom), custom),
            (Some(custom), Some(daily), daily),
        ] {
            let mut section = json!({});
            if let Some(origin) = file {
                section["baseUrl"] = json!(origin);
            }
            std::fs::write(&path, json!({"antigravity":section}).to_string()).unwrap();
            let config = Config::read(&path, |key| {
                if key == "CCP_ANTIGRAVITY_BASE_URL" {
                    environment.map(OsString::from)
                } else {
                    None
                }
            })
            .unwrap();
            assert_eq!(config.base_url, Url::parse(expected).unwrap());
        }
    }

    #[test]
    fn antigravity_consumer_invalid_selected_origin_never_falls_back() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        for invalid in [
            "",
            "http://example.invalid",
            "http://127.0.0.1:1234",
            "https://user:secret@example.invalid",
            "https://example.invalid/native",
            "https://example.invalid/?query=value",
            "https://example.invalid/#fragment",
        ] {
            std::fs::write(
                &path,
                br#"{"antigravity":{"baseUrl":"https://cloudcode-pa.googleapis.com"}}"#,
            )
            .unwrap();
            assert!(
                Config::read(&path, |key| {
                    (key == "CCP_ANTIGRAVITY_BASE_URL").then(|| invalid.into())
                })
                .is_err()
            );
            std::fs::write(
                &path,
                json!({"antigravity":{"baseUrl":invalid}}).to_string(),
            )
            .unwrap();
            assert!(Config::read(&path, |_| None).is_err());
        }
        std::fs::write(&path, br#"{"antigravity":{"authMethod":"gcp"}}"#).unwrap();
        assert!(Config::read(&path, |_| None).is_err());
    }

    #[test]
    fn antigravity_user_agent_file_setting_is_accepted_without_enabling_provider() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        std::fs::write(
            &path,
            br#"{"antigravity":{"userAgent":"file-agent/7  (exact bytes)"}}"#,
        )
        .unwrap();
        let config = Config::read(&path, |_| None).expect("valid provider-specific override");
        assert!(!config.enabled);
    }

    #[test]
    fn antigravity_user_agent_precedence_bytes_and_sensitive_request_debug() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        let default = Config::read(&path, |key| match key {
            "CCP_USER_AGENT" | "CCP_CODEX_USER_AGENT" => Some("unrelated-UA".into()),
            _ => None,
        })
        .unwrap();
        assert!(!default.enabled);
        assert_eq!(
            default.native_headers()["user-agent"],
            super::super::super::antigravity_http_tests::expected_native_user_agent()
        );
        std::fs::write(&path, br#"{"antigravity":{"userAgent":"file-UA"}}"#).unwrap();
        assert_eq!(
            Config::read(&path, |_| None).unwrap().native_headers()["user-agent"],
            "file-UA"
        );
        for value in [
            "x".to_owned(),
            "x".repeat(4096),
            "sentinel-UA/9  (exact  interior bytes)".into(),
        ] {
            let config = Config::read(&path, |key| {
                (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| value.clone().into())
            })
            .unwrap();
            let headers = config.native_headers();
            assert_eq!(headers["user-agent"].as_bytes(), value.as_bytes());
            assert!(headers["user-agent"].is_sensitive());
            let request = config
                .client()
                .unwrap()
                .post("http://127.0.0.1:1/test")
                .header("user-agent", "stale-one")
                .header("user-agent", "stale-two")
                .headers(headers.clone())
                .build()
                .unwrap();
            assert_eq!(request.headers().get_all("user-agent").iter().count(), 1);
            assert_eq!(request.headers()["user-agent"].as_bytes(), value.as_bytes());
            assert!(request.headers()["user-agent"].is_sensitive());
            assert!(!format!("{headers:?} {request:?}").contains("sentinel-UA"));
        }
        let result = Config::read(&path, |key| {
            (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| "".into())
        });
        assert_eq!(result.err(), Some("invalid Antigravity user agent"));
    }

    #[test]
    fn antigravity_user_agent_file_validation_and_static_diagnostics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        for value in [
            json!(null),
            json!(42),
            json!(false),
            json!([]),
            json!({}),
            json!(""),
            json!(" "),
            json!("sentinel\r\nUA"),
            json!("sentinel\tUA"),
            json!("sentinel\u{7f}"),
            json!("é"),
            json!("x".repeat(4097)),
        ] {
            std::fs::write(
                &path,
                json!({"antigravity":{"userAgent":value}}).to_string(),
            )
            .unwrap();
            let error = Config::read(&path, |_| None)
                .err()
                .expect("invalid header must fail");
            assert_eq!(error, "invalid Antigravity user agent");
            assert!(!format!("{error} {error:?}").contains("sentinel"));
        }
        std::fs::write(
            &path,
            br#"{"antigravity":{"userAgent":"file-UA","unknown":true}}"#,
        )
        .unwrap();
        assert!(Config::read(&path, |_| None).is_err());
        std::fs::write(&path, b"{not-json").unwrap();
        assert!(
            Config::read(&path, |key| (key == "CCP_ANTIGRAVITY_USER_AGENT")
                .then(|| "valid-UA".into()))
            .is_err()
        );
    }

    #[test]
    fn antigravity_user_agent_ascii_boundaries_and_environment_precedence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        std::fs::write(&path, br#"{"antigravity":{"userAgent":"file-UA"}}"#).unwrap();
        for byte in 0..=127u8 {
            let value = format!("a{}z", char::from(byte));
            let result = Config::read(&path, |key| {
                (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| value.clone().into())
            });
            assert_eq!(result.is_ok(), (0x20..=0x7e).contains(&byte));
        }
        let too_long = "x".repeat(4097);
        assert!(
            Config::read(&path, |key| {
                (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| too_long.clone().into())
            })
            .is_err()
        );
        std::fs::write(&path, br#"{"antigravity":{"userAgent":""}}"#).unwrap();
        let config = Config::read(&path, |key| {
            (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| "environment-UA".into())
        })
        .unwrap();
        assert_eq!(config.native_headers()["user-agent"], "environment-UA");
        std::fs::write(&path, br#"{"antigravity":{"userAgent":42}}"#).unwrap();
        assert!(
            Config::read(&path, |key| {
                (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| "environment-UA".into())
            })
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_user_agent_non_utf8_injected_environment_does_not_fall_back() {
        use std::os::unix::ffi::OsStringExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.json");
        std::fs::write(&path, br#"{"antigravity":{"userAgent":"file-UA"}}"#).unwrap();
        let result = Config::read(&path, |key| {
            (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| std::ffi::OsString::from_vec(vec![0xff]))
        });
        assert_eq!(result.err(), Some("invalid Antigravity user agent"));
    }

    #[test]
    fn antigravity_user_agent_invalid_environment_is_not_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent.json");
        for value in [
            "",
            " ",
            " bad",
            "bad ",
            "bad\tvalue",
            "bad\r\ninjected",
            "bad\0value",
            "bad\u{7f}",
            "é",
        ] {
            let result = Config::read(&path, |key| {
                (key == "CCP_ANTIGRAVITY_USER_AGENT").then(|| value.into())
            });
            assert_eq!(result.err(), Some("invalid Antigravity user agent"));
        }
    }
}
