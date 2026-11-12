use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct DirResolverEnv {
    pub platform: String,
    pub env: HashMap<String, String>,
    pub home: String,
}

impl Default for DirResolverEnv {
    fn default() -> Self {
        Self {
            platform: std::env::consts::OS.into(),
            env: crate::environment::legacy_vars().collect(),
            home: std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| "/".to_string()),
        }
    }
}

pub fn resolve_config_dir(deps: &DirResolverEnv) -> PathBuf {
    if let Some(override_dir) = deps.env.get("CCP_CONFIG_DIR") {
        return Path::new(override_dir).to_path_buf();
    }

    if deps.platform == "win32" {
        let appdata = deps
            .env
            .get("APPDATA")
            .cloned()
            .unwrap_or_else(|| format!("{}\\AppData\\Roaming", deps.home));
        return join_with_sep(&appdata, &["claude-code-proxy"], true);
    }

    if deps.platform == "darwin" {
        return join_with_sep(&deps.home, &[".config", "claude-code-proxy"], false);
    }

    let base = deps.env.get("XDG_CONFIG_HOME").cloned().unwrap_or_else(|| {
        join_with_sep(&deps.home, &[".config"], false)
            .to_string_lossy()
            .into_owned()
    });
    join_with_sep(&base, &["claude-code-proxy"], false)
}

pub fn resolve_state_dir(deps: &DirResolverEnv) -> PathBuf {
    if deps.platform == "win32" {
        let local = deps
            .env
            .get("LOCALAPPDATA")
            .cloned()
            .unwrap_or_else(|| format!("{}\\AppData\\Local", deps.home));
        return join_with_sep(&local, &["claude-code-proxy"], true);
    }

    let base = deps.env.get("XDG_STATE_HOME").cloned().unwrap_or_else(|| {
        join_with_sep(&deps.home, &[".local", "state"], false)
            .to_string_lossy()
            .into_owned()
    });
    join_with_sep(&base, &["claude-code-proxy"], false)
}

pub fn legacy_config_dir(deps: &DirResolverEnv) -> PathBuf {
    join_with_sep(&deps.home, &[".config", "claude-code-proxy"], false)
}

pub fn config_dir() -> PathBuf {
    resolve_config_dir(&DirResolverEnv::default())
}

pub fn state_dir() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default())
}

pub fn codex_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("codex").join("auth.json")
}

pub fn kimi_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("kimi").join("auth.json")
}

pub fn cursor_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("cursor").join("auth.json")
}

pub fn kimi_device_id_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("kimi").join("device_id")
}

pub fn log_file() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default()).join("proxy.log")
}

pub fn provider_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    resolve_config_dir(&deps).join(provider).join("auth.json")
}

pub fn provider_legacy_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    legacy_config_dir(&deps).join(provider).join("auth.json")
}

fn join_with_sep(base: &str, parts: &[&str], win32: bool) -> PathBuf {
    let sep = '/';
    let _ = win32;
    let mut out = String::new();
    for part in std::iter::once(base).chain(parts.iter().copied()) {
        if !out.is_empty() && !out.ends_with(sep) {
            out.push(sep);
        }
        out.push_str(part);
    }
    Path::new(&out).to_path_buf()
}

pub fn resolve_config_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_config_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: env.clone(),
        home: home.to_string(),
    })
}

pub fn resolve_state_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_state_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: env.clone(),
        home: home.to_string(),
    })
}
