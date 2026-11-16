//! Sealed-patch receipts (docs/p4/zk-sealed-patch.md).
//!
//! [`verify`] checks a RISC Zero succinct STARK receipt against the pinned guest [`IMAGE_ID`]
//! and an expected [`Public`] statement. It is cheap and always available.
//! Proving lives in `zk/prover`, outside the workspace, so workspace builds never link the
//! RISC Zero prover.

pub use arkiv_git_zk_core::{Private, Public};
use risc0_zkvm::{InnerReceipt, Receipt};

/// Image id of `zk/methods/guest` (the `sealed_patch` guest), pinned.
/// `zk/prover` refuses to prove when the built guest does not match it.
pub const IMAGE_ID: [u32; 8] = [
    3354357554, 2858077429, 275378841, 3769622342, 1632199423, 736883060, 2225384083, 3399504904,
];

#[derive(Debug, thiserror::Error)]
pub enum ZkError {
    #[error("receipt does not decode: {0}")]
    Decode(String),
    #[error("receipt is not a succinct STARK receipt")]
    NotSuccinct,
    #[error("proof does not verify against the pinned image id: {0}")]
    Proof(String),
    #[error("journal does not decode: {0}")]
    Journal(String),
    #[error("journal does not match the expected public statement")]
    Mismatch,
}

/// [`IMAGE_ID`] as lowercase hex of its little-endian words (RISC Zero's digest form), the
/// `image_id` repo programs see on a verified proof.
pub fn image_id_hex() -> String {
    IMAGE_ID
        .iter()
        .map(|w| hex::encode(w.to_le_bytes()))
        .collect()
}

/// Verifies `receipt_bytes` (bincode `Receipt`) against the pinned image id and returns the
/// public statement its journal commits. Deterministic: the same bytes give the same result.
pub fn journal(receipt_bytes: &[u8]) -> Result<Public, ZkError> {
    let receipt: Receipt =
        bincode::deserialize(receipt_bytes).map_err(|e| ZkError::Decode(e.to_string()))?;
    if !matches!(receipt.inner, InnerReceipt::Succinct(_)) {
        return Err(ZkError::NotSuccinct);
    }
    receipt
        .verify(IMAGE_ID)
        .map_err(|e| ZkError::Proof(e.to_string()))?;
    receipt
        .journal
        .decode()
        .map_err(|e| ZkError::Journal(e.to_string()))
}

/// Verifies `receipt_bytes` (bincode `Receipt`) proves the relation for exactly `expected`.
pub fn verify(receipt_bytes: &[u8], expected: &Public) -> Result<(), ZkError> {
    match journal(receipt_bytes)? == *expected {
        true => Ok(()),
        false => Err(ZkError::Mismatch),
    }
}
