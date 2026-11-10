//! This machine's device key (docs/p5/head-model.md 4.1): a seed at `device/seed`, outside
//! `keys/`, so copying an identity's keys to another machine makes a new device there.
//! Its signing key signs heads and snapshot manifests; its per-repo control address `C_d`
//! creates the head and inline pieces, its bulk address `B_d` bulk pieces and snapshots.

use std::path::{Path, PathBuf};

use charter_crypto::MasterSeed;

use crate::keys::{Keys, write_secret};
use crate::{Result, SyncError};

pub fn device_seed_path(home: &Path) -> PathBuf {
    home.join("device").join("seed")
}

/// The device keys of `home`, created on first use.
pub fn device_keys(home: &Path) -> Result<Keys> {
    let p = device_seed_path(home);
    match std::fs::read(&p) {
        Ok(bytes) => {
            let seed: [u8; 32] = bytes.try_into().map_err(|_| {
                SyncError::coded(
                    charter_core::ErrorCode::BadFormat,
                    format!("{} is not 32 bytes", p.display()),
                )
            })?;
            Ok(Keys::from_seed(MasterSeed::from_bytes(seed)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let seed = MasterSeed::generate()
                .map_err(|e| SyncError::coded(charter_core::ErrorCode::Internal, e.to_string()))?;
            write_secret(&p, seed.expose())?;
            Ok(Keys::from_seed(seed))
        }
        Err(e) => Err(SyncError::io(p.display(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_key_is_created_once_and_differs_per_home() {
        let (a, b) = (
            tempfile::tempdir().expect("a"),
            tempfile::tempdir().expect("b"),
        );
        let first = device_keys(a.path()).expect("create");
        assert_eq!(device_keys(a.path()).expect("load").nid(), first.nid());
        assert_ne!(device_keys(b.path()).expect("other").nid(), first.nid());
        std::fs::write(device_seed_path(a.path()), b"short").expect("w");
        assert!(device_keys(a.path()).is_err());
    }
}
