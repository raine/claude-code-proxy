use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::to_string_pretty;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::marker::PhantomData;

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
        use security_framework::passwords::get_generic_password;
        use security_framework_sys::base::errSecItemNotFound;

        match get_generic_password(service, account) {
            Ok(raw) => String::from_utf8(raw)
                .map(Some)
                .map_err(|error| anyhow::anyhow!("Keychain value is not valid UTF-8: {error}")),
            Err(error) if error.code() == errSecItemNotFound => Ok(None),
            Err(error) => Err(anyhow::anyhow!("Keychain read failed: {error}")),
        }
    }

    fn write(&self, service: &str, account: &str, value: &str) -> Result<()> {
        security_framework::passwords::set_generic_password(service, account, value.as_bytes())
            .map_err(|error| anyhow::anyhow!("Keychain write failed: {error}"))
    }

    fn delete(&self, service: &str, account: &str) -> Result<()> {
        use security_framework::passwords::delete_generic_password;
        use security_framework_sys::base::errSecItemNotFound;

        match delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(error) if error.code() == errSecItemNotFound => Ok(()),
            Err(error) => Err(anyhow::anyhow!("Keychain delete failed: {error}")),
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

    fn clear_legacy_then_primary(&self) -> Result<()> {
        if self.file != self.legacy_file
            && let Err(error) = fs::remove_file(&self.legacy_file)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
        if let Err(error) = fs::remove_file(&self.file)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(error.into());
        }
        Ok(())
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

    /// Promote a readable file fallback to Keychain.
    ///
    /// The caller must hold the store's mutation lock. The current value is
    /// first rewritten to the primary mode-0600 file, then written to and read
    /// back from Keychain. File credentials are removed only after that
    /// verification succeeds, with the primary file removed last so an
    /// interruption never loses the recoverable copy.
    pub fn migrate_file_to_keychain(&self) -> Result<bool> {
        if !self.use_keychain {
            return Ok(false);
        }
        let Some((value, _source)) = self.file_store.load_with_source() else {
            return Ok(false);
        };

        self.file_store.save(value.clone())?;
        let raw = serde_json::to_string(&value)?;
        self.keychain.write(&self.service, &self.account, &raw)?;
        match self.keychain.read(&self.service, &self.account)? {
            Some(stored) if stored == raw => {}
            Some(_) => anyhow::bail!("Keychain verification returned different credentials"),
            None => anyhow::bail!("Keychain verification could not find the stored credentials"),
        }
        self.file_store.clear_legacy_then_primary()?;
        self.set_active_backend(KeychainFileBackend::Keychain);
        Ok(true)
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
            let keychain_verified = self
                .keychain
                .write(&self.service, &self.account, &raw)
                .and_then(
                    |()| match self.keychain.read(&self.service, &self.account)? {
                        Some(stored) if stored == raw => Ok(()),
                        Some(_) => {
                            anyhow::bail!("Keychain verification returned different credentials")
                        }
                        None => {
                            anyhow::bail!("Keychain verification could not find stored credentials")
                        }
                    },
                )
                .is_ok();
            if keychain_verified {
                self.set_active_backend(KeychainFileBackend::Keychain);
                return Ok(());
            }
            self.file_store.save(value)?;
            self.set_active_backend(KeychainFileBackend::FileFallback {
                path: self.file_store.path(),
                fallback_reason: Some("macOS Keychain write or verification unavailable"),
            });
            return Ok(());
        }
        self.file_store.save(value)?;
        self.set_active_backend(KeychainFileBackend::File(self.file_store.path()));
        Ok(())
    }

    fn clear(&self) -> Result<()> {
        let keychain_error = self
            .use_keychain
            .then(|| self.keychain.delete(&self.service, &self.account))
            .and_then(Result::err);
        let file_error = self.file_store.clear().err();
        match (keychain_error, file_error) {
            (Some(keychain), Some(file)) => {
                anyhow::bail!(
                    "Failed to clear Keychain credentials ({keychain}) and file fallback ({file})"
                )
            }
            (Some(error), None) | (None, Some(error)) => return Err(error),
            (None, None) => {}
        }
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

struct TemporaryAuthFile {
    path: std::path::PathBuf,
    remove_on_drop: bool,
}

impl TemporaryAuthFile {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            remove_on_drop: true,
        }
    }

    fn disarm(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for TemporaryAuthFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn write_atomically<T: Serialize>(path: &str, value: &T) -> Result<()> {
    // Serialize before touching the filesystem so serialization failures
    // cannot leave an empty credential temp file or a newly-created directory.
    let payload = to_string_pretty(value)?;
    let dir = std::path::Path::new(path)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid auth path"))?;
    crate::fsutil::create_dir_all_with_mode(dir, 0o700)?;

    let tmp = std::path::PathBuf::from(format!("{path}.tmp-{}", uuid::Uuid::new_v4()));
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
    let mut temporary = TemporaryAuthFile::new(tmp.clone());
    out.write_all(payload.as_bytes())?;
    out.sync_all()?;
    drop(out);
    fs::rename(&tmp, path)?;
    // From this point on the credential lives at its final path. A directory
    // fsync error means durability is uncertain, but must not remove the file
    // that was already published by rename.
    temporary.disarm();
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
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

    #[derive(Clone, Default)]
    struct DeleteFailingKeychain;

    impl Keychain for DeleteFailingKeychain {
        fn read(&self, _service: &str, _account: &str) -> Result<Option<String>> {
            Ok(None)
        }

        fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
            Ok(())
        }

        fn delete(&self, _service: &str, _account: &str) -> Result<()> {
            anyhow::bail!("delete failed")
        }
    }

    #[derive(Clone, Default)]
    struct MismatchingKeychain;

    impl Keychain for MismatchingKeychain {
        fn read(&self, _service: &str, _account: &str) -> Result<Option<String>> {
            Ok(Some(r#"{"source":"different"}"#.to_string()))
        }

        fn write(&self, _service: &str, _account: &str, _value: &str) -> Result<()> {
            Ok(())
        }

        fn delete(&self, _service: &str, _account: &str) -> Result<()> {
            Ok(())
        }
    }

    fn temp_auth_path(dir: &tempfile::TempDir, name: &str) -> String {
        dir.path().join(name).to_string_lossy().to_string()
    }

    struct SerializationFailure;

    impl Serialize for SerializationFailure {
        fn serialize<S>(&self, _serializer: S) -> std::result::Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            Err(serde::ser::Error::custom("simulated serialization failure"))
        }
    }

    #[test]
    fn atomic_write_serializes_before_touching_the_filesystem() {
        let temp = tempfile::TempDir::new().unwrap();
        let auth_dir = temp.path().join("not-created");
        let file = auth_dir.join("auth.json");

        assert!(write_atomically(file.to_str().unwrap(), &SerializationFailure).is_err());
        assert!(!auth_dir.exists());
    }

    #[test]
    fn atomic_write_removes_temp_file_when_rename_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let destination = temp.path().join("auth.json");
        fs::create_dir(&destination).unwrap();

        assert!(
            write_atomically(destination.to_str().unwrap(), &json!({"token": "secret"})).is_err()
        );
        assert!(destination.is_dir());
        assert!(
            fs::read_dir(temp.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("auth.json.tmp-"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_private_directory_and_file() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::TempDir::new().unwrap();
        let auth_dir = temp.path().join("credentials");
        let file = auth_dir.join("auth.json");
        write_atomically(file.to_str().unwrap(), &json!({"token": "secret"})).unwrap();

        assert_eq!(
            fs::metadata(&auth_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
    fn keychain_file_store_migrates_verified_file_and_removes_primary_last() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&legacy, &json!({"source": "legacy"})).unwrap();

        let keychain = MockKeychain::default();
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy.clone(),
            "svc",
            "acct",
            true,
            keychain.clone(),
        );

        assert!(store.migrate_file_to_keychain().unwrap());
        assert_eq!(store.path(), "macOS Keychain");
        assert!(!std::path::Path::new(&file).exists());
        assert!(!std::path::Path::new(&legacy).exists());
        let stored = keychain.raw("svc", "acct").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&stored).unwrap()["source"],
            json!("legacy")
        );
    }

    #[test]
    fn keychain_file_store_keeps_file_when_migration_write_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&file, &json!({"source": "file"})).unwrap();
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy,
            "svc",
            "acct",
            true,
            ReadOnlyKeychain::default(),
        );

        assert!(store.migrate_file_to_keychain().is_err());
        assert_eq!(store.load().unwrap().unwrap()["source"], json!("file"));
        assert!(std::path::Path::new(&file).exists());
    }

    #[test]
    fn keychain_file_store_keeps_file_when_migration_verification_differs() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&file, &json!({"source": "file"})).unwrap();
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy,
            "svc",
            "acct",
            true,
            MismatchingKeychain,
        );

        assert!(store.migrate_file_to_keychain().is_err());
        assert_eq!(store.load().unwrap().unwrap()["source"], json!("file"));
        assert!(std::path::Path::new(&file).exists());
    }

    #[test]
    fn keychain_file_store_clears_file_even_when_keychain_delete_fails() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp_auth_path(&temp, "auth.json");
        let legacy = temp_auth_path(&temp, "legacy.json");
        write_atomically(&file, &json!({"source": "file"})).unwrap();
        let store: KeychainFileAuthStore<serde_json::Value, _> = KeychainFileAuthStore::new(
            file.clone(),
            legacy,
            "svc",
            "acct",
            true,
            DeleteFailingKeychain,
        );

        assert!(store.clear().is_err());
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
            format!("File fallback: {file} (macOS Keychain write or verification unavailable)")
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
