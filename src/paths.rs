use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static PROCESS_LOG_FILE: OnceLock<PathBuf> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct DirResolverEnv {
    pub platform: String,
    pub env: HashMap<String, String>,
    pub home: String,
}

impl Default for DirResolverEnv {
    fn default() -> Self {
        // Directory resolution depends on only these variables. Copying the
        // entire process environment on every log/config lookup made request
        // hot paths pay for unrelated (and sometimes very large) variables.
        let env = [
            "CCP_CONFIG_DIR",
            "APPDATA",
            "LOCALAPPDATA",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
        ]
        .into_iter()
        .filter_map(|key| {
            std::env::var_os(key)
                .map(|value| (key.to_string(), value.to_string_lossy().into_owned()))
        })
        .collect();
        Self {
            platform: std::env::consts::OS.into(),
            env,
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
        return join_with_sep(&appdata, &["claude-code-proxy"]);
    }

    if deps.platform == "darwin" {
        return join_with_sep(&deps.home, &[".config", "claude-code-proxy"]);
    }

    let base = deps.env.get("XDG_CONFIG_HOME").cloned().unwrap_or_else(|| {
        join_with_sep(&deps.home, &[".config"])
            .to_string_lossy()
            .into_owned()
    });
    join_with_sep(&base, &["claude-code-proxy"])
}

pub fn resolve_state_dir(deps: &DirResolverEnv) -> PathBuf {
    if deps.platform == "win32" {
        let local = deps
            .env
            .get("LOCALAPPDATA")
            .cloned()
            .unwrap_or_else(|| format!("{}\\AppData\\Local", deps.home));
        return join_with_sep(&local, &["claude-code-proxy"]);
    }

    let base = deps.env.get("XDG_STATE_HOME").cloned().unwrap_or_else(|| {
        join_with_sep(&deps.home, &[".local", "state"])
            .to_string_lossy()
            .into_owned()
    });
    join_with_sep(&base, &["claude-code-proxy"])
}

pub fn legacy_config_dir(deps: &DirResolverEnv) -> PathBuf {
    join_with_sep(&deps.home, &[".config", "claude-code-proxy"])
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

pub fn log_file() -> PathBuf {
    PROCESS_LOG_FILE
        .get_or_init(|| {
            resolve_log_file_for_process(
                &DirResolverEnv::default(),
                std::env::current_exe().ok().as_deref(),
                std::process::id(),
            )
        })
        .clone()
}

fn resolve_log_file_for_process(
    deps: &DirResolverEnv,
    executable: Option<&Path>,
    process_id: u32,
) -> PathBuf {
    if let Some(path) = executable.and_then(|path| cargo_test_log_file(path, process_id)) {
        return path;
    }
    resolve_state_dir(deps).join("proxy.log")
}

fn cargo_test_log_file(executable: &Path, process_id: u32) -> Option<PathBuf> {
    // Cargo test and nextest executables use
    // `<target>/<profile>/deps/<target-name>-<metadata-hash>`. Keep their logs beside other
    // build artifacts so unit and integration test processes cannot append to the user's log.
    let deps_dir = executable.parent()?;
    if deps_dir.file_name()?.to_str()? != "deps" {
        return None;
    }
    let stem = executable.file_stem()?.to_str()?;
    let (_, metadata_hash) = stem.rsplit_once('-')?;
    if metadata_hash.len() < 8 || !metadata_hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let profile_dir = deps_dir.parent()?;
    Some(
        profile_dir
            .join(".test-logs")
            .join(process_id.to_string())
            .join("proxy.log"),
    )
}

pub fn provider_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    provider_auth_file_for_env(provider, &deps)
}

pub fn provider_legacy_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    provider_legacy_auth_file_for_env(provider, &deps)
}

fn provider_auth_file_for_env(provider: &str, deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join(provider).join("auth.json")
}

fn provider_legacy_auth_file_for_env(provider: &str, deps: &DirResolverEnv) -> PathBuf {
    // An explicit config directory is an isolation boundary. Falling back to
    // the default HOME path here can make a test or isolated profile read or
    // delete the user's real credentials.
    if deps.env.contains_key("CCP_CONFIG_DIR") {
        return provider_auth_file_for_env(provider, deps);
    }
    legacy_config_dir(deps).join(provider).join("auth.json")
}

fn join_with_sep(base: &str, parts: &[&str]) -> PathBuf {
    let sep = '/';
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_test_binary_uses_process_isolated_log_file() {
        let deps = DirResolverEnv {
            platform: "linux".into(),
            env: HashMap::from([("XDG_STATE_HOME".into(), "/real-state".into())]),
            home: "/home/tester".into(),
        };
        let executable = Path::new("/repo/target/debug/deps/foundation-0123456789abcdef");

        assert_eq!(
            resolve_log_file_for_process(&deps, Some(executable), 4242),
            PathBuf::from("/repo/target/debug/.test-logs/4242/proxy.log")
        );
    }

    #[test]
    fn normal_binary_keeps_production_log_file() {
        let deps = DirResolverEnv {
            platform: "linux".into(),
            env: HashMap::from([("XDG_STATE_HOME".into(), "/real-state".into())]),
            home: "/home/tester".into(),
        };
        let executable = Path::new("/repo/target/debug/claude-code-proxy");

        assert_eq!(
            resolve_log_file_for_process(&deps, Some(executable), 4242),
            PathBuf::from("/real-state/claude-code-proxy/proxy.log")
        );
    }

    #[test]
    fn deps_binary_without_cargo_hash_keeps_production_log_file() {
        let deps = DirResolverEnv {
            platform: "darwin".into(),
            env: HashMap::new(),
            home: "/Users/tester".into(),
        };
        let executable = Path::new("/repo/target/debug/deps/manual-helper");

        assert_eq!(
            resolve_log_file_for_process(&deps, Some(executable), 4242),
            PathBuf::from("/Users/tester/.local/state/claude-code-proxy/proxy.log")
        );
    }

    #[test]
    fn explicit_config_dir_disables_default_home_auth_fallback() {
        let deps = DirResolverEnv {
            platform: "darwin".into(),
            env: HashMap::from([("CCP_CONFIG_DIR".into(), "/tmp/isolated-ccproxy".into())]),
            home: "/Users/real-user".into(),
        };

        let primary = provider_auth_file_for_env("grok", &deps);
        let legacy = provider_legacy_auth_file_for_env("grok", &deps);
        assert_eq!(
            primary,
            PathBuf::from("/tmp/isolated-ccproxy/grok/auth.json")
        );
        assert_eq!(legacy, primary);
    }

    #[test]
    fn default_config_keeps_home_legacy_auth_path() {
        let deps = DirResolverEnv {
            platform: "linux".into(),
            env: HashMap::from([("XDG_CONFIG_HOME".into(), "/xdg/config".into())]),
            home: "/home/tester".into(),
        };

        assert_eq!(
            provider_auth_file_for_env("grok", &deps),
            PathBuf::from("/xdg/config/claude-code-proxy/grok/auth.json")
        );
        assert_eq!(
            provider_legacy_auth_file_for_env("grok", &deps),
            PathBuf::from("/home/tester/.config/claude-code-proxy/grok/auth.json")
        );
    }
}
