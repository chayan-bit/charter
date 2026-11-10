//! The local peer's keys: master seed (`keys/seed`, mode 0600, design 3.9), the composite
//! signing key, and the per-repo EVM signers (design 3.1, decision 3 still open: the
//! derivation lives behind `charter_crypto::evm`, swapped there without touching sync).

use std::path::{Path, PathBuf};

use alloy_signer_local::PrivateKeySigner;
use charter_core::{ErrorCode, Nid, Rid};
use charter_crypto::evm::{self, EvmKey};
use charter_crypto::{MasterSeed, PublicKey, SecretKey};
use charter_store::{Address, B256};

use crate::{Result, SyncError};

/// EVM key index; P1 never rotates EVM keys, so every bind uses index 0.
pub const EVM_INDEX: u32 = 0;

pub struct Keys {
    seed: MasterSeed,
    sk: SecretKey,
    pk: PublicKey,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keys({:?})", self.pk)
    }
}

pub fn seed_path(home: &Path) -> PathBuf {
    home.join("keys").join("seed")
}

fn signer(k: &EvmKey) -> PrivateKeySigner {
    PrivateKeySigner::from_bytes(&B256::from(*k.secret()))
        .unwrap_or_else(|_| unreachable!("derived EVM keys are valid nonzero scalars"))
}

impl Keys {
    pub fn from_seed(seed: MasterSeed) -> Self {
        let sk = seed.signing_key();
        let pk = sk.public_key();
        Self { seed, sk, pk }
    }

    /// Loads `keys/seed`, or `None` if no key was created yet.
    pub fn load(home: &Path) -> Result<Option<Self>> {
        let p = seed_path(home);
        let bytes = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(SyncError::io(p.display(), e)),
        };
        let seed: [u8; 32] = bytes.try_into().map_err(|_| {
            SyncError::coded(
                ErrorCode::BadFormat,
                format!("{} is not 32 bytes", p.display()),
            )
        })?;
        Ok(Some(Self::from_seed(MasterSeed::from_bytes(seed))))
    }

    /// Loads the key or fails with a hint to run `chart keys init`.
    pub fn require(home: &Path) -> Result<Self> {
        Self::load(home)?.ok_or_else(|| {
            SyncError::coded(ErrorCode::Internal, "no key: run `chart keys init` first")
        })
    }

    /// Creates `keys/seed` (mode 0600); refuses to overwrite an existing key.
    pub fn init(home: &Path) -> Result<Self> {
        let p = seed_path(home);
        if p.exists() {
            return Err(SyncError::coded(
                ErrorCode::Internal,
                format!("{} exists; refusing to overwrite a key", p.display()),
            ));
        }
        let seed = MasterSeed::generate()
            .map_err(|e| SyncError::coded(ErrorCode::Internal, e.to_string()))?;
        write_secret(&p, seed.expose())?;
        Ok(Self::from_seed(seed))
    }

    pub fn nid(&self) -> Nid {
        self.pk.nid()
    }
    pub fn sk(&self) -> &SecretKey {
        &self.sk
    }
    pub fn pk(&self) -> &PublicKey {
        &self.pk
    }
    pub fn wallet(&self) -> PrivateKeySigner {
        signer(&evm::wallet(&self.seed, EVM_INDEX))
    }
    pub fn control(&self, rid: &Rid) -> PrivateKeySigner {
        signer(&evm::control(&self.seed, rid, EVM_INDEX))
    }
    pub fn bulk(&self, rid: &Rid) -> PrivateKeySigner {
        signer(&evm::bulk(&self.seed, rid, EVM_INDEX))
    }
    pub fn control_address(&self, rid: &Rid) -> Address {
        self.control(rid).address()
    }
    pub fn bulk_address(&self, rid: &Rid) -> Address {
        self.bulk(rid).address()
    }
}

pub(crate) fn write_secret(p: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = p
        .parent()
        .ok_or_else(|| SyncError::io(p.display(), "no parent"))?;
    std::fs::create_dir_all(dir).map_err(|e| SyncError::io(dir.display(), e))?;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(p).map_err(|e| SyncError::io(p.display(), e))?;
    f.write_all(bytes)
        .and_then(|()| f.sync_all())
        .map_err(|e| SyncError::io(p.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_load_and_refuse_overwrite() {
        let d = tempfile::tempdir().expect("tmp");
        assert!(Keys::load(d.path()).expect("load").is_none());
        assert!(Keys::require(d.path()).is_err());
        let k = Keys::init(d.path()).expect("init");
        let again = Keys::require(d.path()).expect("load");
        assert_eq!(k.nid(), again.nid());
        assert!(Keys::init(d.path()).is_err(), "no overwrite");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(seed_path(d.path())).expect("meta");
            assert_eq!(mode.permissions().mode() & 0o777, 0o600);
        }
        let rid = [3u8; 32];
        assert_ne!(k.control_address(&rid), k.bulk_address(&rid));
        assert_ne!(k.wallet().address(), k.control_address(&rid));
        assert_eq!(k.sk().public_key(), *k.pk());
        assert!(format!("{k:?}").starts_with("Keys("));
        std::fs::write(seed_path(d.path()), b"short").expect("w");
        assert!(Keys::load(d.path()).is_err());
    }
}
