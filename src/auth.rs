use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::to_string_pretty;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::marker::PhantomData;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

pub trait AuthStorage<T>: Send + Sync
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn load(&self) -> Result<Option<T>>;
    fn save(&self, value: T) -> Result<()>;
    fn clear(&self) -> Result<()>;
    fn path(&self) -> String;

    /// Filesystem location used to coordinate credential mutations across
    /// processes. This is intentionally separate from `path()`, which may be a
    /// user-facing backend label such as `macOS Keychain`.
    fn coordination_path(&self) -> Option<std::path::PathBuf> {
        None
    }
}

pub trait Keychain: Send + Sync {
    fn read(&self, service: &str, account: &str) -> Result<Option<String>>;
    fn write(&self, service: &str, account: &str, value: &str) -> Result<()>;
    fn delete(&self, service: &str, account: &str) -> Result<()>;
}

#[derive(Default)]
pub struct StubKeychain;

impl Keychain for StubKeychain {
    fn read(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
        Ok(())
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        Ok(())
    }
}

#[derive(Default, Clone, Copy)]
pub struct SystemKeychain;

#[cfg(target_os = "macos")]
impl Keychain for SystemKeychain {
    fn read(&self, service: &str, account: &str) -> Result<Option<String>> {
        let output = run_security(&["find-generic-password", "-s", service, "-a", account, "-w"])?;
        if output.status.success() {
            let mut raw = String::from_utf8(output.stdout)
                .map_err(|err| anyhow::anyhow!("Keychain value is not valid UTF-8: {err}"))?;
            trim_one_trailing_newline(&mut raw);
            return Ok(Some(raw));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("could not be found") || stderr.contains("specified item could not") {
            return Ok(None);
        }
        Err(anyhow::anyhow!("Keychain read failed: {}", stderr.trim()))
    }

    fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
        anyhow::bail!("Keychain write is not available through non-interactive compatibility mode")
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        let output = run_security(&["delete-generic-password", "-s", service, "-a", account])?;
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("could not be found") || stderr.contains("specified item could not") {
            return Ok(());
        }
        Err(anyhow::anyhow!("Keychain delete failed: {}", stderr.trim()))
    }
}

#[cfg(target_os = "macos")]
fn run_security(args: &[&str]) -> Result<std::process::Output> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let mut child = Command::new("/usr/bin/security")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow::anyhow!("Failed to start /usr/bin/security: {err}"))?;
    let start = Instant::now();
    loop {
        if child
            .try_wait()
            .map_err(|err| anyhow::anyhow!("Failed waiting for /usr/bin/security: {err}"))?
            .is_some()
        {
            return child.wait_with_output().map_err(|err| {
                anyhow::anyhow!("Failed collecting /usr/bin/security output: {err}")
            });
        }
        if start.elapsed() >= Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("Timed out reading macOS Keychain");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "macos")]
fn trim_one_trailing_newline(value: &mut String) {
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl Keychain for SystemKeychain {
    fn read(&self, _service: &str, _account: &str) -> Result<Option<String>> {
        Ok(None)
    }

    fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
        anyhow::bail!("Keychain storage is not available on this platform")
    }

    fn delete(&self, _service: &str, _account: &str) -> Result<()> {
        Ok(())
    }
}

pub struct FileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    file: String,
    legacy_file: String,
    _marker: std::marker::PhantomData<T>,
}

impl<T> FileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    pub fn new(file: String, legacy_file: String) -> Self {
        Self {
            file,
            legacy_file,
            _marker: Default::default(),
        }
    }

    fn load_with_source(&self) -> Option<(T, String)> {
        if let Some(parsed) = load_auth_file::<T>(&self.file) {
            return Some((parsed, self.file.clone()));
        }
        if self.file == self.legacy_file {
            return None;
        }
        load_auth_file::<T>(&self.legacy_file).map(|parsed| (parsed, self.legacy_file.clone()))
    }
}

impl<T> AuthStorage<T> for FileAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn load(&self) -> Result<Option<T>> {
        Ok(self.load_with_source().map(|(parsed, _path)| parsed))
    }

    fn save(&self, value: T) -> Result<()> {
        write_atomically(&self.file, &value)
    }

    fn clear(&self) -> Result<()> {
        for path in [&self.file, &self.legacy_file] {
            if let Err(err) = fs::remove_file(path)
                && err.kind() != io::ErrorKind::NotFound
            {
                return Err(anyhow::Error::from(err));
            }
        }
        Ok(())
    }

    fn path(&self) -> String {
        self.file.clone()
    }

    fn coordination_path(&self) -> Option<std::path::PathBuf> {
        Some(std::path::PathBuf::from(&self.file))
    }
}

pub struct KeychainFileAuthStore<T, K = SystemKeychain>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
    K: Keychain,
{
    file_store: FileAuthStore<T>,
    keychain: K,
    service: String,
    account: String,
    use_keychain: bool,
    keychain_path: String,
    active_backend: std::sync::Mutex<KeychainFileBackend>,
    _marker: PhantomData<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum KeychainFileBackend {
    Undetermined,
    Keychain,
    File(String),
    FileFallback {
        path: String,
        fallback_reason: Option<&'static str>,
    },
}

impl<T, K> KeychainFileAuthStore<T, K>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
    K: Keychain,
{
    pub fn new(
        file: String,
        legacy_file: String,
        service: impl Into<String>,
        account: impl Into<String>,
        use_keychain: bool,
        keychain: K,
    ) -> Self {
        let active_backend = if use_keychain {
            KeychainFileBackend::Undetermined
        } else {
            KeychainFileBackend::File(file.clone())
        };
        Self {
            file_store: FileAuthStore::new(file, legacy_file),
            keychain,
            service: service.into(),
            account: account.into(),
            use_keychain,
            keychain_path: "macOS Keychain".to_string(),
            active_backend: std::sync::Mutex::new(active_backend),
            _marker: PhantomData,
        }
    }

    fn set_active_backend(&self, backend: KeychainFileBackend) {
        *self
            .active_backend
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = backend;
    }

    fn active_backend(&self) -> KeychainFileBackend {
        self.active_backend
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl<T, K> AuthStorage<T> for KeychainFileAuthStore<T, K>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
    K: Keychain,
{
    fn load(&self) -> Result<Option<T>> {
        if let Some((parsed, path)) = self.file_store.load_with_source() {
            self.set_active_backend(if self.use_keychain {
                KeychainFileBackend::FileFallback {
                    path,
                    fallback_reason: None,
                }
            } else {
                KeychainFileBackend::File(path)
            });
            return Ok(Some(parsed));
        }
        if self.use_keychain
            && let Some(raw) = self.keychain.read(&self.service, &self.account)?
        {
            let parsed = serde_json::from_str::<T>(&raw)
                .map(Some)
                .map_err(|err| anyhow::anyhow!("Failed to parse Keychain auth JSON: {err}"))?;
            self.set_active_backend(KeychainFileBackend::Keychain);
            return Ok(parsed);
        }
        self.set_active_backend(KeychainFileBackend::Undetermined);
        Ok(None)
    }

    fn save(&self, value: T) -> Result<()> {
        if self.use_keychain {
            // A readable file is authoritative on load. Keep updating that
            // backend instead of writing a newer value only to Keychain and
            // leaving the next process to load stale file credentials.
            if self.file_store.load_with_source().is_some() {
                self.file_store.save(value)?;
                self.set_active_backend(KeychainFileBackend::FileFallback {
                    path: self.file_store.path(),
                    fallback_reason: Some("existing file fallback remains authoritative"),
                });
                return Ok(());
            }
            let raw = serde_json::to_string(&value)?;
            if self
                .keychain
                .write(&self.service, &self.account, &raw)
                .is_ok()
            {
                self.set_active_backend(KeychainFileBackend::Keychain);
                return Ok(());
            }
            self.file_store.save(value)?;
            self.set_active_backend(KeychainFileBackend::FileFallback {
                path: self.file_store.path(),
                fallback_reason: Some("macOS Keychain write unavailable"),
            });
            return Ok(());
        }
        self.file_store.save(value)?;
        self.set_active_backend(KeychainFileBackend::File(self.file_store.path()));
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        if self.use_keychain {
            self.keychain.delete(&self.service, &self.account)?;
        }
        self.file_store.clear()?;
        self.set_active_backend(if self.use_keychain {
            KeychainFileBackend::Undetermined
        } else {
            KeychainFileBackend::File(self.file_store.path())
        });
        Ok(())
    }

    fn path(&self) -> String {
        match self.active_backend() {
            KeychainFileBackend::Undetermined | KeychainFileBackend::Keychain => {
                self.keychain_path.clone()
            }
            KeychainFileBackend::FileFallback {
                path,
                fallback_reason: Some(reason),
            } => format!("File fallback: {path} ({reason})"),
            KeychainFileBackend::FileFallback {
                path,
                fallback_reason: None,
            } => format!("File fallback: {path}"),
            KeychainFileBackend::File(path) => path,
        }
    }

    fn coordination_path(&self) -> Option<std::path::PathBuf> {
        self.file_store.coordination_path()
    }
}

pub fn load_auth_file<T: DeserializeOwned>(path: &str) -> Option<T> {
    let mut file = File::open(path).ok()?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).ok()?;
    serde_json::from_str::<T>(&raw).ok()
}

pub fn load_auth_file_value(path: &std::path::Path) -> Option<serde_json::Value> {
    let mut file = File::open(path).ok()?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw).ok()
}

pub fn load_auth_file_with_legacy<T: DeserializeOwned>(
    primary: &std::path::Path,
    legacy: &std::path::Path,
) -> Option<T> {
    if let Some(value) = load_auth_file_value(primary) {
        return serde_json::from_value(value).ok();
    }
    if primary == legacy {
        None
    } else {
        load_auth_file_value(legacy).and_then(|value| serde_json::from_value(value).ok())
    }
}

pub fn delete_auth_file(primary: &std::path::Path, legacy: &std::path::Path) -> io::Result<()> {
    if let Err(err) = fs::remove_file(primary)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err);
    }
    if primary != legacy
        && let Err(err) = fs::remove_file(legacy)
        && err.kind() != io::ErrorKind::NotFound
    {
        return Err(err);
    }
    Ok(())
}

pub fn write_atomically<T: Serialize>(path: &str, value: &T) -> Result<()> {
    let dir = std::path::Path::new(path)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid auth path"))?;
    fs::create_dir_all(dir)?;
    set_mode(dir, 0o700);

    let tmp = format!("{path}.tmp-{}", uuid::Uuid::new_v4());
    #[cfg(unix)]
    let mut out = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?
    };
    #[cfg(not(unix))]
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    out.write_all(to_string_pretty(value)?.as_bytes())?;
    out.sync_all()?;
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err.into());
    }
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    set_mode(std::path::Path::new(path), 0o600);
    Ok(())
}

fn set_mode(path: &std::path::Path, mode: u32) {
    #[cfg(unix)]
    {
        if let Ok(meta) = fs::metadata(path) {
            let mut permissions = meta.permissions();
            permissions.set_mode(mode);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

pub struct InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    inner: std::sync::Arc<std::sync::Mutex<Option<T>>>,
}

impl<T> Default for InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl<T> Clone for InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> AuthStorage<T> for InMemoryAuthStore<T>
where
    T: Serialize + DeserializeOwned + Send + Sync + Clone,
{
    fn load(&self) -> Result<Option<T>> {
        let inner = self
            .inner
            .lock()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        Ok(inner.clone())
    }

    fn save(&self, value: T) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        *inner = Some(value);
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|err| anyhow::anyhow!(err.to_string()))?;
        *inner = None;
        Ok(())
    }

    fn path(&self) -> String {
        "memory".to_string()
    }
}

#[cfg(test)]
pub fn fixture_store<T>() -> InMemoryAuthStore<T>
where
    T: Serialize + serde::de::DeserializeOwned + Send + Sync + Clone,
{
    InMemoryAuthStore::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockKeychain {
        values: Arc<Mutex<HashMap<(String, String), String>>>,
    }

    impl MockKeychain {
        fn set_raw(&self, service: &str, account: &str, value: serde_json::Value) {
            self.values.lock().unwrap().insert(
                (service.to_string(), account.to_string()),
                value.to_string(),
            );
        }

        fn raw(&self, service: &str, account: &str) -> Option<String> {
            self.values
                .lock()
                .unwrap()
                .get(&(service.to_string(), account.to_string()))
                .cloned()
        }
    }

    impl Keychain for MockKeychain {
        fn read(&self, service: &str, account: &str) -> Result<Option<String>> {
            Ok(self.raw(service, account))
        }

        fn write(&self, service: &str, account: &str, value: &str) -> Result<()> {
            self.values.lock().unwrap().insert(
                (service.to_string(), account.to_string()),
                value.to_string(),
            );
            Ok(())
        }

        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.values
                .lock()
                .unwrap()
                .remove(&(service.to_string(), account.to_string()));
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct ReadOnlyKeychain(MockKeychain);

    impl Keychain for ReadOnlyKeychain {
        fn read(&self, service: &str, account: &str) -> Result<Option<String>> {
            self.0.read(service, account)
        }

        fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
            anyhow::bail!("read-only")
        }

        fn delete(&self, service: &str, account: &str) -> Result<()> {
            self.0.delete(service, account)
        }
    }

    fn temp_auth_path(dir: &tempfile::TempDir, name: &str) -> String {
        dir.path().join(name).to_string_lossy().to_string()
    }

    #[test]
    fn keychain_file_store_loads_file_before_keychain() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&file, &json!({"source": "file"})).unwrap();

        let keychain = MockKeychain::default();
        keychain.set_raw("svc", "acct", json!({"source": "keychain"}));

        let store: KeychainFileAuthStore<serde_json::Value, _> =
            KeychainFileAuthStore::new(file.clone(), legacy, "svc", "acct", true, keychain);

        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded["source"], json!("file"));
        assert_eq!(store.path(), format!("File fallback: {file}"));
    }

    #[test]
    fn keychain_display_path_is_separate_from_mutation_coordination_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy,
            "svc",
            "acct",
            true,
            MockKeychain::default(),
        );

        assert_eq!(store.path(), "macOS Keychain");
        assert_eq!(
            store.coordination_path().as_deref(),
            Some(std::path::Path::new(&file))
        );
    }

    #[test]
    fn keychain_file_store_falls_back_to_keychain_when_file_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        let keychain = MockKeychain::default();
        keychain.set_raw("svc", "acct", json!({"source": "keychain"}));

        let store: KeychainFileAuthStore<serde_json::Value, _> =
            KeychainFileAuthStore::new(file, legacy, "svc", "acct", true, keychain);

        let loaded = store.load().unwrap().unwrap();
        assert_eq!(loaded["source"], json!("keychain"));
        assert_eq!(store.path(), "macOS Keychain");
    }

    #[test]
    fn keychain_file_store_reports_the_legacy_file_it_loaded() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&legacy, &json!({"source": "legacy"})).unwrap();

        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file,
            legacy.clone(),
            "svc",
            "acct",
            true,
            MockKeychain::default(),
        );

        assert_eq!(store.load().unwrap().unwrap()["source"], json!("legacy"));
        assert_eq!(store.path(), format!("File fallback: {legacy}"));
    }

    #[test]
    fn keychain_file_store_saves_and_clears_keychain_when_enabled() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");

        let keychain = MockKeychain::default();
        let store: KeychainFileAuthStore<serde_json::Value, _> =
            KeychainFileAuthStore::new(file.clone(), legacy, "svc", "acct", true, keychain.clone());

        store.save(json!({"source": "saved"})).unwrap();
        let raw = keychain.raw("svc", "acct").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&raw).unwrap()["source"],
            json!("saved")
        );
        assert_eq!(store.path(), "macOS Keychain");

        store.clear().unwrap();
        assert!(keychain.raw("svc", "acct").is_none());
        assert!(!std::path::Path::new(&file).exists());
    }

    #[test]
    fn keychain_file_store_keeps_an_existing_file_authoritative_on_save() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&file, &json!({"source": "old-file"})).unwrap();

        let keychain = MockKeychain::default();
        let store: KeychainFileAuthStore<serde_json::Value, _> =
            KeychainFileAuthStore::new(file.clone(), legacy, "svc", "acct", true, keychain.clone());

        store.save(json!({"source": "new-file"})).unwrap();

        assert!(keychain.raw("svc", "acct").is_none());
        assert_eq!(
            store.path(),
            format!("File fallback: {file} (existing file fallback remains authoritative)")
        );
        assert_eq!(store.load().unwrap().unwrap()["source"], json!("new-file"));
    }

    #[test]
    fn keychain_file_store_falls_back_to_file_when_keychain_write_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy,
            "svc",
            "acct",
            true,
            ReadOnlyKeychain::default(),
        );

        store.save(json!({"source": "file-fallback"})).unwrap();
        assert_eq!(
            store.path(),
            format!("File fallback: {file} (macOS Keychain write unavailable)")
        );
        assert_eq!(
            store.load().unwrap().unwrap()["source"],
            json!("file-fallback")
        );
        assert!(std::path::Path::new(&file).exists());
        assert_eq!(store.path(), format!("File fallback: {file}"));
    }

    #[test]
    fn keychain_file_store_uses_file_when_keychain_disabled() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        let keychain = MockKeychain::default();
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy,
            "svc",
            "acct",
            false,
            keychain.clone(),
        );

        store.save(json!({"source": "file"})).unwrap();
        assert!(keychain.raw("svc", "acct").is_none());
        assert_eq!(store.path(), file);
        assert_eq!(store.load().unwrap().unwrap()["source"], json!("file"));
    }
}
