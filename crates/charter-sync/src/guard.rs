//! Fail-closed guard against plaintext writes to a private repo (docs/p2/private-repos.md,
//! "Unsealed-write guard and local permissions"). Every charter-sync path that sends a
//! repo's control rows or data (identity and op writes, pushes, keepalive) calls
//! [`require_sealed`] first, so a caller that forgets the sealing selector fails instead of
//! publishing a private repo in the clear.

use std::path::{Path, PathBuf};

use charter_core::{ErrorCode, Rid};
use charter_store::Arkiv;

use crate::ids::to_text;
use crate::{Result, SyncError};

/// `$CHARTER_HOME/private/<rid hex>.json`: this machine's keyring for private repo `rid`
/// (charter-private's `Keyring::path`; a test there keeps the two equal).
pub fn keyring_path(home: &Path, rid: &Rid) -> PathBuf {
    home.join("private")
        .join(format!("{}.json", hex::encode(rid)))
}

/// `E_UNSEALED_PRIVATE` when `a` does not seal under `rid`'s own keys (a plain backend, or
/// one sealing another repo) and this machine holds a keyring for `rid`. One `stat`, none
/// on a backend sealing `rid`; a `stat` that fails for any reason but "absent" refuses too.
pub fn require_sealed<A: Arkiv>(a: &A, home: &Path, rid: &Rid) -> Result<()> {
    if a.seals_for(rid) || matches!(keyring_path(home, rid).try_exists(), Ok(false)) {
        return Ok(());
    }
    Err(SyncError::coded(
        ErrorCode::UnsealedPrivate,
        format!(
            "{} is a private repo: refusing to write to it unsealed",
            to_text(rid)
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use charter_store::fake::MemArkiv;

    #[test]
    fn refuses_only_a_plain_backend_on_a_repo_with_a_keyring() {
        let home = tempfile::tempdir().expect("home");
        let (a, rid) = (MemArkiv::new(7), [3u8; 32]);
        assert!(require_sealed(&a, home.path(), &rid).is_ok(), "public repo");
        let p = keyring_path(home.path(), &rid);
        std::fs::create_dir_all(p.parent().expect("dir")).expect("mkdir");
        std::fs::write(&p, b"{}").expect("keyring");
        let e = require_sealed(&a, home.path(), &rid).expect_err("private");
        assert_eq!(e.code(), ErrorCode::UnsealedPrivate);
    }
}
