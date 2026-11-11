//! Proof attachments on ops (docs/p3/programs.md, "Proofs"). An op payload may carry
//! `proofs: [{ kind: "risc0", receipt: <sha256 hex>, chunks: [<sha256 hex>, ..], public }]`;
//! the receipt bytes are the concatenated `data` of `xyz.arkiv.blob` ops whose data hash to
//! `chunks`, in order. [`attach`] verifies each attachment before the fold, so programs see
//! only `op.proofs` results, never receipt bytes.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use charter_program::{ProgramOp, ProofCheck};
use charter_zk::Public;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::cob::json::to_json;

/// COB type of a receipt chunk op: payload `{ data: <hex> }`, addressed by `sha256` of the
/// decoded bytes. Hex text, so `op post --payload-json` can write it; a chunk of at most
/// [`BLOB_CHUNK`] bytes keeps the signed op body under its 62,119 B cap.
pub const BLOB_TYPE: &str = "xyz.arkiv.blob";
/// Largest raw chunk per blob op: 60,000 hex chars plus the op envelope fit the body cap
/// (32,000 did not: 64,120 B).
pub const BLOB_CHUNK: usize = 30_000;
/// The only proof kind this build verifies.
pub const RISC0: &str = "risc0";

/// Verified journals by receipt hash (`None`: the receipt does not verify). The value is a
/// pure function of bytes that hash to the key, so a re-fold reuses it without verifying.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ProofCache {
    journals: BTreeMap<String, Option<Public>>,
    #[serde(skip)]
    dirty: bool,
}

impl ProofCache {
    /// Reads the cache at `path`; a missing or unreadable file is an empty cache.
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Writes the cache to `path` if [`attach`] added entries.
    pub fn save(&self, path: &Path) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        crate::state::write_json(path, self)
    }
}

/// Lowercase hex `sha256` of `data`: how blobs and receipts are addressed.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Chunk bytes of every blob op, by `sha256(data)` hex.
fn blobs(ops: &[ProgramOp]) -> HashMap<String, Vec<u8>> {
    ops.iter()
        .filter(|o| o.type_name == BLOB_TYPE)
        .filter_map(|o| {
            let v = to_json(&o.payload).ok()?;
            hex::decode(v["data"].as_str()?).ok()
        })
        .map(|d| (sha256_hex(&d), d))
        .collect()
}

/// The verified journal of `entry`'s receipt, or `None` when it is missing or invalid.
/// Missing chunks are not cached: they may be posted later.
fn journal(
    entry: &Value,
    blobs: &HashMap<String, Vec<u8>>,
    cache: &mut ProofCache,
) -> Option<Public> {
    let receipt = entry["receipt"].as_str()?;
    if let Some(j) = cache.journals.get(receipt) {
        return j.clone();
    }
    let parts = entry["chunks"].as_array()?.iter();
    let parts = parts.map(|c| blobs.get(c.as_str()?).map(Vec::as_slice));
    let parts = parts.collect::<Option<Vec<&[u8]>>>()?;
    let bytes = parts.concat();
    if sha256_hex(&bytes) != receipt {
        return None;
    }
    let j = charter_zk::journal(&bytes).ok();
    cache.journals.insert(receipt.to_owned(), j.clone());
    cache.dirty = true;
    j
}

fn check(entry: &Value, blobs: &HashMap<String, Vec<u8>>, cache: &mut ProofCache) -> ProofCheck {
    let kind = entry["kind"].as_str().unwrap_or_default().to_owned();
    let public = entry.get("public").cloned().unwrap_or_else(|| json!({}));
    let claimed = serde_json::from_value::<Public>(public.clone()).ok();
    let is_risc0 = kind == RISC0;
    // Exact statement match: a valid receipt of another statement is not ok.
    let ok = is_risc0 && claimed.is_some() && journal(entry, blobs, cache) == claimed;
    ProofCheck {
        kind,
        image_id: if is_risc0 {
            charter_zk::image_id_hex()
        } else {
            String::new()
        },
        ok,
        public,
    }
}

/// `ops` with `proofs` filled from each payload's proof attachments.
pub fn attach(ops: Vec<ProgramOp>, cache: &mut ProofCache) -> Vec<ProgramOp> {
    let blobs = blobs(&ops);
    ops.into_iter()
        .map(|o| {
            let payload = to_json(&o.payload).unwrap_or(Value::Null);
            let entries = payload["proofs"].as_array().cloned().unwrap_or_default();
            let proofs = entries.iter().map(|e| check(e, &blobs, cache)).collect();
            ProgramOp { proofs, ..o }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use charter_core::{Body, CobOpBody, ZERO, max_body_len};

    use super::*;

    #[test]
    fn a_full_blob_op_fits_one_signed_cob_entity() {
        let payload = json!({ "data": hex::encode(vec![0xab; BLOB_CHUNK]) });
        let body = CobOpBody {
            rid: [1; 32],
            type_name: BLOB_TYPE.into(),
            object: ZERO,
            parents: vec![],
            action: "create".into(),
            payload: crate::cob::json::to_cbor(&payload).expect("cbor"),
            ts_ms: u64::MAX,
        };
        let cap = max_body_len(charter_crypto::SIG_LEN);
        assert!(
            body.encode().len() <= cap,
            "{} > {cap}",
            body.encode().len()
        );
    }
}
