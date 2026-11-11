//! Configuration and the per-repo cache layout under `$CHARTER_HOME` (design 3.9):
//! `repos/<rid-hex>/{git/, state.json, outgoing/, lock}`; the lock is held while a
//! [`Repo`] is alive, for the whole of a push, fetch or keepalive.

use std::path::{Path, PathBuf};
use std::time::Duration;

use charter_core::{Nid, Rid};
use charter_store::PipelineConfig;

use crate::git::Git;
use crate::ids::hex32;
use crate::{Result, SyncError};

/// Data lifetime at create (design 2.3): 30 days of 2 s blocks.
pub const DATA_LIFETIME: u64 = 1_296_000;
/// Control-plane lifetime at create: the maximum, 182.5 days.
pub const CONTROL_LIFETIME: u64 = 7_884_000;
/// Warn when a required entity expires sooner than this (7 days).
pub const WARN_EXPIRY_BLOCKS: u64 = 302_400;
/// Blocks per day on Arkiv (2 s blocks).
pub const BLOCKS_PER_DAY: u64 = 43_200;

#[derive(Debug, Clone)]
pub struct Config {
    pub home: PathBuf,
    /// `minLifetime` for refs, manifests and chunks (test override: `--lifetime`).
    pub data_lifetime: u64,
    /// `minLifetime` for peer, bind, id, idacc and anchor entities.
    pub control_lifetime: u64,
    pub pipeline: PipelineConfig,
    /// Peers to sync besides the delegates (`remote.<name>.arkivPeers`).
    pub extra_peers: Vec<Nid>,
    /// Chunk txs handed to the pipeline at once (bounds memory for 100 MiB packs).
    pub chunk_batch: usize,
    /// Display alias written into this peer's `peer` document.
    pub alias: String,
}

impl Config {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            data_lifetime: DATA_LIFETIME,
            control_lifetime: CONTROL_LIFETIME,
            pipeline: PipelineConfig::default(),
            extra_peers: vec![],
            chunk_batch: 256,
            alias: String::new(),
        }
    }

    /// A config for the in-memory fake: no waiting between receipt polls.
    pub fn for_fake(home: PathBuf) -> Self {
        let mut c = Self::new(home);
        c.pipeline.poll = Duration::ZERO;
        c
    }

    /// `$CHARTER_HOME` (or the pre-rename `$ARKIV_GIT_HOME`), else [`home_under`] `$HOME`.
    pub fn default_home() -> PathBuf {
        if let Some(h) = env_compat("CHARTER_HOME", "ARKIV_GIT_HOME") {
            return PathBuf::from(h);
        }
        home_under(Path::new(&std::env::var_os("HOME").unwrap_or_default()))
    }
}

/// `<user_home>/.charter`, unless only the pre-rename `<user_home>/.arkiv-git` exists.
pub fn home_under(user_home: &Path) -> PathBuf {
    let new = user_home.join(".charter");
    let old = user_home.join(".arkiv-git");
    if !new.exists() && old.is_dir() {
        old
    } else {
        new
    }
}

/// Reads env var `new`, falling back to its pre-rename name `old`; empty counts as unset.
pub fn env_compat(new: &str, old: &str) -> Option<std::ffi::OsString> {
    [new, old]
        .into_iter()
        .find_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()))
}

/// An opened, locked repo cache.
#[derive(Debug)]
pub struct Repo {
    pub rid: Rid,
    pub dir: PathBuf,
    git: Git,
    _lock: std::fs::File,
}

impl Repo {
    /// Creates the layout if needed and takes the repo lock (blocks while another process
    /// holds it).
    pub fn open(home: &Path, rid: &Rid) -> Result<Self> {
        let (dir, lock) = lock_file(home, rid)?;
        lock.lock()
            .map_err(|e| SyncError::io(dir.join("lock").display(), e))?;
        Self::locked(rid, dir, lock)
    }

    /// Like [`Repo::open`], but `None` instead of waiting when another process holds the
    /// lock (the UI sync loop skips a round rather than delay a push).
    pub fn try_open(home: &Path, rid: &Rid) -> Result<Option<Self>> {
        let (dir, lock) = lock_file(home, rid)?;
        match lock.try_lock() {
            Ok(()) => Self::locked(rid, dir, lock).map(Some),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => {
                Err(SyncError::io(dir.join("lock").display(), e))
            }
        }
    }

    fn locked(rid: &Rid, dir: PathBuf, lock: std::fs::File) -> Result<Self> {
        let git = Git::init_cache(&dir.join("git"))?;
        Ok(Self {
            rid: *rid,
            dir,
            git,
            _lock: lock,
        })
    }

    pub fn git(&self) -> &Git {
        &self.git
    }
    pub fn git_dir(&self) -> PathBuf {
        self.dir.join("git")
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    pub fn outgoing(&self) -> PathBuf {
        self.dir.join("outgoing")
    }
    pub fn pending_path(&self) -> PathBuf {
        self.outgoing().join("pending.json")
    }
    pub fn tmp(&self) -> PathBuf {
        self.dir.join("tmp")
    }
}

/// Creates `d` and makes it owner-only: `$CHARTER_HOME` and a repo's cache hold the
/// decrypted objects and packs of private repos.
fn owner_only_dir(d: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(d).map_err(|e| SyncError::io(d.display(), e))?;
    std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| SyncError::io(d.display(), e))
}

/// Creates (or truncates) `p` with mode 0600: packs of a private repo are plaintext here.
pub fn create_owner_only(p: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(p)
        .map_err(|e| SyncError::io(p.display(), e))
}

/// [`std::fs::write`] through [`create_owner_only`].
pub fn write_owner_only(p: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    create_owner_only(p)?
        .write_all(bytes)
        .map_err(|e| SyncError::io(p.display(), e))
}

/// Creates the layout of `rid` under `home` (owner-only, tightening an existing home) and
/// opens (without locking) its lock file.
fn lock_file(home: &Path, rid: &Rid) -> Result<(PathBuf, std::fs::File)> {
    let repos = home.join("repos");
    let dir = repos.join(hex32(rid));
    let (outgoing, tmp) = (dir.join("outgoing"), dir.join("tmp"));
    for d in [home, &repos, &dir, &outgoing, &tmp] {
        owner_only_dir(d)?;
    }
    let lock_path = dir.join("lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| SyncError::io(lock_path.display(), e))?;
    Ok((dir, lock))
}

/// `refs/namespaces/<nid-hex>/` (verified, anchored refs of one peer).
pub fn namespace(nid: &Nid) -> String {
    format!("refs/namespaces/{}/", hex32(nid))
}

/// `refs/arkiv/staging/<nid-hex>/` (local, not yet anchored).
pub fn staging(nid: &Nid) -> String {
    format!("refs/arkiv/staging/{}/", hex32(nid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_compat_prefers_new_name_then_falls_back_to_old() {
        // set_var is unsafe in edition 2024, so this leans on PATH and HOME being set.
        let unset = "CHARTER_TEST_SURELY_UNSET_VAR";
        assert_eq!(env_compat(unset, "PATH"), std::env::var_os("PATH"));
        assert_eq!(env_compat("PATH", "HOME"), std::env::var_os("PATH"));
        assert_eq!(env_compat(unset, unset), None);
    }

    #[test]
    fn home_under_falls_back_to_pre_rename_dir_only_when_new_is_absent() {
        let d = tempfile::tempdir().expect("tmp");
        assert_eq!(home_under(d.path()), d.path().join(".charter"));
        std::fs::create_dir(d.path().join(".arkiv-git")).expect("old");
        assert_eq!(home_under(d.path()), d.path().join(".arkiv-git"));
        std::fs::create_dir(d.path().join(".charter")).expect("new");
        assert_eq!(home_under(d.path()), d.path().join(".charter"));
    }

    #[test]
    fn try_open_is_none_while_another_repo_holds_the_lock() {
        let d = tempfile::tempdir().expect("tmp");
        let rid = [2u8; 32];
        let held = Repo::open(d.path(), &rid).expect("open");
        assert!(Repo::try_open(d.path(), &rid).expect("try").is_none());
        drop(held);
        // A child forked by a parallel test holds a copy of the flock until its exec
        // closes it (CLOEXEC), so "free" can lag the drop by a moment.
        let got = (0..50)
            .find_map(|_| {
                let r = Repo::try_open(d.path(), &rid).expect("try");
                r.is_none()
                    .then(|| std::thread::sleep(std::time::Duration::from_millis(20)));
                r
            })
            .expect("free");
        assert!(got.git_dir().join("HEAD").exists());
        assert!(Repo::try_open(d.path(), &rid).expect("try").is_none());
    }

    #[test]
    fn layout_and_lock() {
        let d = tempfile::tempdir().expect("tmp");
        let r = Repo::open(d.path(), &[1u8; 32]).expect("open");
        assert!(r.git_dir().join("HEAD").exists());
        assert!(r.outgoing().is_dir() && r.tmp().is_dir());
        assert!(r.pending_path().starts_with(r.outgoing()));
        assert!(r.state_path().ends_with("state.json"));
        assert!(r.git().refs("refs/").expect("refs").is_empty());
        assert!(namespace(&[0xab; 32]).starts_with("refs/namespaces/abab"));
        assert!(staging(&[0; 32]).starts_with("refs/arkiv/staging/0000"));
        let c = Config::for_fake(d.path().into());
        assert_eq!(c.pipeline.poll, Duration::ZERO);
        assert_eq!(c.data_lifetime, DATA_LIFETIME);
        assert!(!Config::default_home().as_os_str().is_empty());
    }
}
