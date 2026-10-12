use anyhow::{bail, Context, Result};
use dbfork_model::{CloneRequest, DatabaseConfig, Resource};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub name: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    #[serde(default = "default_env_files")]
    pub env_files: Vec<PathBuf>,
    #[serde(default = "protected_branches")]
    pub protected_branches: Vec<String>,
}
fn default_branch() -> String {
    "main".into()
}
fn default_env_files() -> Vec<PathBuf> {
    vec![".env".into()]
}
fn protected_branches() -> Vec<String> {
    vec![
        "main".into(),
        "master".into(),
        "production".into(),
        "prod".into(),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub project: Project,
    pub databases: Vec<DatabaseConfig>,
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let value: Self = toml::from_str(&fs::read_to_string(path).context("read .dbfork.toml")?)
            .context("invalid .dbfork.toml")?;
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.project.name.is_empty() || self.databases.is_empty() {
            bail!("project name and at least one database are required")
        }
        let mut names = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for db in &self.databases {
            if db.name.is_empty() || !names.insert(&db.name) {
                bail!("database names must be nonempty and unique")
            }
            if !valid_key(&db.connection_env) || !keys.insert(&db.connection_env) {
                bail!("connection_env values must be valid, unique environment keys")
            }
            if db.source_env.as_ref().is_some_and(|k| !valid_key(k)) {
                bail!("invalid source_env")
            }
        }
        if self.project.env_files.is_empty() {
            bail!("at least one env file is required")
        }
        let mut paths = BTreeSet::new();
        for p in &self.project.env_files {
            if p.as_os_str().is_empty()
                || p.is_absolute()
                || p.components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
                || !paths.insert(p)
            {
                bail!("env_files must be unique relative paths without traversal")
            }
            if p == Path::new(".dbfork.toml") || p.starts_with(".git") || p.starts_with(".dbfork") {
                bail!("env_files cannot overwrite configuration or state")
            }
        }
        Ok(())
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        private_write(path, toml::to_string_pretty(self)?.as_bytes())
    }
}
fn valid_key(k: &str) -> bool {
    !k.is_empty()
        && k.bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Environment {
    pub branch: String,
    pub parent: String,
    pub status: String,
    pub resources: Vec<Resource>,
    pub created_at: u64,
    #[serde(default)]
    pub pending: Vec<CloneRequest>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub project_id: String,
    #[serde(default)]
    pub sources: BTreeMap<String, Resource>,
    #[serde(default)]
    pub environments: BTreeMap<String, Environment>,
    #[serde(default)]
    pub bindings: BTreeMap<PathBuf, String>,
}
impl State {
    pub fn load(path: &Path) -> Result<Self> {
        let state: Self = serde_json::from_slice(&fs::read(path).context("read project state")?)
            .context("invalid project state")?;
        if state.version != 1 {
            bail!("unsupported state version {}", state.version)
        }
        Ok(state)
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        private_write(path, &serde_json::to_vec_pretty(self)?)
    }
}

/// Atomic replacement on the same filesystem. Secrets never appear in a shared temporary directory.
pub fn private_write(path: &Path, bytes: &[u8]) -> Result<()> {
    reject_symlinks(path)?;
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    Ok(())
}
pub fn reject_symlinks(path: &Path) -> Result<()> {
    for p in path.ancestors() {
        match fs::symlink_metadata(p) {
            Ok(m) if m.file_type().is_symlink() => bail!("refusing symbolic link: {}", p.display()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
pub fn private_dir(path: &Path) -> Result<()> {
    reject_symlinks(path)?;
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub struct Lock(File);
fn lock_is_contended(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    // LockFileEx reports ERROR_LOCK_VIOLATION instead of WouldBlock on Windows.
    #[cfg(windows)]
    if error.raw_os_error() == Some(33) {
        return true;
    }
    false
}
impl Lock {
    pub fn acquire(dir: &Path) -> Result<Self> {
        private_dir(dir)?;
        let path = dir.join("lock");
        reject_symlinks(&path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let start = Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Self(file)),
                Err(e) if lock_is_contended(&e) && start.elapsed() < Duration::from_secs(30) => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                Err(_) => bail!(
                    "project is locked by another dbfork-proto process; retry after it finishes"
                ),
            }
        }
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    #[test]
    fn windows_lock_violation_is_retryable() {
        assert!(lock_is_contended(&std::io::Error::from_raw_os_error(33)));
        assert!(!lock_is_contended(&std::io::Error::from_raw_os_error(5)));
    }
    #[test]
    fn rejects_traversal_and_duplicates() {
        let mut c:Config=toml::from_str("[project]\nname='test'\n[[databases]]\nname='db'\nengine='sqlite'\nprovider='local'\nconnection_env='DATABASE_URL'\n").unwrap();
        assert!(c.validate().is_ok());
        c.project.env_files = vec!["../.env".into()];
        assert!(c.validate().is_err());
    }
    #[test]
    fn writes_and_replaces_atomically() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().canonicalize().unwrap().join("state");
        private_write(&p, b"one").unwrap();
        private_write(&p, b"two").unwrap();
        assert_eq!(fs::read(p).unwrap(), b"two");
    }
}
