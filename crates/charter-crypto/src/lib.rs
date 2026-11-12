//! charter-crypto: signature algorithm registry, composite ML-DSA-65 + Ed25519
//! signatures (RFC 10073), tagged hashing, seed handling and key derivation.
//!
//! See `docs/p1/design.md` sections 2.1, 3.1 and 7.1, and `docs/p0/pq-signatures.md`.

mod alg;
mod composite;
mod error;
pub mod evm;
mod hash;
mod seed;

pub use alg::{Alg, PK_LEN, SECRET_LEN, SIG_LEN};
pub use composite::{PublicKey, SecretKey, Sig, ed25519_verify};
pub use error::CryptoError;
pub use hash::{h, sha256};
pub use seed::MasterSeed;

/// A 32-byte hash value (SHA-256 output, nid, rid, entity content hash).
pub type Hash32 = [u8; 32];
