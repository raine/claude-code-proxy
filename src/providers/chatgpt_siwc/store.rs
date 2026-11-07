//! Private, atomic storage. No provider credentials are imported.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expires_at: u64,
    pub earliest_refresh_at: Option<u64>,
    pub scopes: Vec<String>,
    pub nonce: String,
}

impl Tokens {
    pub fn permits_inference(&self) -> bool {
        ["resource.invoke", "chatgpt.tokens.use.direct"]
            .iter()
            .all(|scope| self.scopes.iter().any(|s| s == scope))
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Profile {
    pub client_id: String,
    pub issuer: String,
    pub subject: String,
    pub email: Option<String>,
    pub revision: u64,
    pub tokens: Option<Tokens>,
    #[serde(default)]
    pub refresh_pending: bool,
}

#[derive(Serialize, Deserialize)]
pub(super) struct State {
    pub version: u32,
    pub ext_agent_host_id: String,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Clone)]
pub(super) struct Store {
    root: PathBuf,
}

pub(super) struct LockedStore {
    root: PathBuf,
    _lock: File,
    pub state: State,
}

pub(super) fn validate_label(label: &str) -> Result<()> {
    ensure!(
        !label.is_empty()
            && label.len() <= 64
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Profile label must contain 1–64 ASCII letters, digits, '-' or '_'"
    );
    Ok(())
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn default_path() -> Self {
        Self::new(crate::paths::config_dir().join("siwc"))
    }

    pub async fn lock(&self) -> Result<LockedStore> {
        prepare_directory(&self.root)?;
        let path = self.root.join("state.lock");
        check_private(&path, false)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options
            .open(&path)
            .context("Cannot open SIWC credential lock")?;
        check_private(&path, false)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock)
                    if tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(25)).await
                }
                Err(_) => bail!("SIWC credential store is busy or cannot be locked; retry later"),
            }
        }
        // Remove only our own abandoned atomic-write files while holding the lock.
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name
                .strip_prefix(".state-")
                .and_then(|n| n.strip_suffix(".tmp"))
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .is_some()
            {
                check_private(&entry.path(), false)?;
                fs::remove_file(entry.path())?;
            }
        }
        let path = self.root.join("state.json");
        check_private(&path, false)?;
        let (state, fresh) = match fs::read(&path) {
            Ok(raw) => (
                serde_json::from_slice::<State>(&raw)
                    .context("Invalid SIWC credential store; refusing to overwrite it")?,
                false,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                State {
                    version: 1,
                    ext_agent_host_id: format!("urn:uuid:{}", uuid::Uuid::new_v4()),
                    profiles: BTreeMap::new(),
                },
                true,
            ),
            Err(e) => return Err(e).context("Cannot read SIWC credential store"),
        };
        ensure!(
            state.version == 1,
            "Unsupported SIWC credential store version"
        );
        let host = state
            .ext_agent_host_id
            .strip_prefix("urn:uuid:")
            .and_then(|s| uuid::Uuid::parse_str(s).ok());
        ensure!(
            host.is_some_and(|id| id.get_version_num() == 4),
            "Invalid stored SIWC host ID; refusing to replace it"
        );
        let locked = LockedStore {
            root: self.root.clone(),
            _lock: lock,
            state,
        };
        if fresh {
            locked.save()?;
        }
        Ok(locked)
    }
}

impl LockedStore {
    pub fn save(&self) -> Result<()> {
        let target = self.root.join("state.json");
        check_private(&target, false)?;
        let temp = self
            .root
            .join(format!(".state-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&serde_json::to_vec(&self.state)?)?;
            file.sync_all()?;
            fs::rename(&temp, &target)?;
            File::open(&self.root)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result.context("Cannot atomically save SIWC credentials")
    }
}

fn prepare_directory(root: &Path) -> Result<()> {
    #[cfg(not(unix))]
    bail!("SIWC file credentials currently require Unix owner-only permissions");
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(root)?;
        check_private(root, true)
    }
}

fn check_private(path: &Path, directory: bool) -> Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("Cannot inspect SIWC storage permissions"),
    };
    ensure!(
        !meta.file_type().is_symlink()
            && (if directory {
                meta.is_dir()
            } else {
                meta.is_file()
            }),
        "SIWC storage must be a regular private file/directory, never a symlink"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "SIWC storage permissions are too broad; use chmod 700 for its directory and chmod 600 for its files"
        );
        if !directory {
            ensure!(
                meta.nlink() == 1,
                "SIWC credential files must not have hard links"
            );
        }
    }
    Ok(())
}
