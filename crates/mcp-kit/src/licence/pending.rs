//! Durable purchase continuity. The lock is released by the OS even after Ctrl+C.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Session {
    pub processor_session_id: String,
    pub url: String,
    pub claim_secret: String,
}

// No Debug: this contains a claim secret and buyer email, never a raw licence.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pending {
    pub version: u32,
    pub created_at_ms: i64,
    pub endpoint: String,
    pub publishable_key: String,
    /// Preserve the exact bytes: Money hashes the raw request body for idempotency.
    pub body: String,
    pub idempotency_key: String,
    /// Persisted before the first create request. An uncertain create is never
    /// blindly replayed while Money can release its idempotency claim on 5xx.
    pub create_attempted: bool,
    pub session: Option<Session>,
}

pub(super) struct Store {
    pub path: PathBuf,
    _lock: File,
}

fn beside(token: &Path, suffix: &str) -> PathBuf {
    let mut name = token.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

impl Store {
    pub fn lock(token: &Path) -> Result<Self, String> {
        let path = beside(token, ".pending-checkout.json");
        let parent = path.parent().ok_or("no config directory on this machine")?;
        std::fs::create_dir_all(parent)
            .map_err(|_| "cannot create the checkout state directory")?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let lock = options
            .open(beside(token, ".checkout.lock"))
            .map_err(|_| "cannot open the checkout state lock")?;
        lock.try_lock_exclusive()
            .map_err(|_| "another licence buy is already running; resume it instead")?;
        Ok(Self { path, _lock: lock })
    }

    pub fn load(&self) -> Result<Option<Pending>, String> {
        let file = match File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err("cannot read pending checkout; do not start another purchase".into())
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file
                .metadata()
                .map_err(|_| "cannot inspect pending checkout")?
                .permissions()
                .mode()
                & 0o777
                != 0o600
            {
                return Err(
                    "pending checkout must have mode 0600; do not start another purchase".into(),
                );
            }
        }
        let mut bytes = Vec::new();
        file.take((1 << 20) + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read pending checkout")?;
        if bytes.len() > 1 << 20 {
            return Err("pending checkout is too large".into());
        }
        let record: Pending = serde_json::from_slice(&bytes).map_err(|_| {
            "invalid pending checkout; use licence recovery before starting another purchase"
        })?;
        if record.version != 1 || record.idempotency_key.is_empty() || record.created_at_ms <= 0 {
            return Err("unsupported pending checkout; use licence recovery before starting another purchase".into());
        }
        Ok(Some(record))
    }

    pub fn save(&self, record: &Pending) -> Result<(), String> {
        let bytes = serde_json::to_vec(record).map_err(|_| "cannot encode pending checkout")?;
        super::write_private(&self.path, &bytes).map_err(|_| {
            "cannot save pending checkout; use licence recovery before another purchase".into()
        })
    }

    pub fn clear(&self) -> Result<(), String> {
        std::fs::remove_file(&self.path).map_err(|_| "cannot remove completed pending checkout")?;
        sync_parent(&self.path).map_err(|_| "cannot sync completed pending checkout".into())
    }
}

pub(super) fn sync_parent(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Same-directory atomic replacement, including on Windows; never follows a stale temp symlink.
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    sync_parent(path)
}
