//! Bulk pieces a refused push already paid for (issue #26, head-model 7.4). Readers take a
//! piece's bytes from its tx's calldata, live or not, so a piece that landed for a push
//! whose head tx was refused (`E_CONCURRENT_PUSH`) can be named by the retry instead of
//! being uploaded again; the retry first reads each one back and must find exactly its
//! bytes, sent by this device's `B_d`. Bulk pieces come from `B_d`, never from `C_d`, so
//! reuse leaves `pieces_created` and the rollback rule untouched.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use charter_core::Rid;
use charter_store::{Address, Arkiv};
use futures_util::future::try_join_all;

use crate::Result;
use crate::state::{read_json, write_json};

/// Piece txs read back at once.
const PARALLEL_READS: usize = 8;

/// Where each bulk piece of the last upload landed, by the hex sha256 of its bytes.
type Record = BTreeMap<String, (u64, u32)>;

fn path(outgoing: &Path) -> PathBuf {
    outgoing.join("bulk-pieces.json")
}

fn sha_hex(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(b))
}

/// The landing of every piece the last upload recorded whose tx still holds exactly its
/// bytes from `bulk`; `None` for the rest. No record (every first attempt): no read.
pub(crate) async fn reusable<A: Arkiv>(
    a: &A,
    outgoing: &Path,
    rid: &Rid,
    bulk: Address,
    pieces: &[&[u8]],
) -> Result<Vec<Option<(u64, u32)>>> {
    // A record that does not parse is only a missed saving.
    let record: Record = read_json(&path(outgoing))
        .ok()
        .flatten()
        .unwrap_or_default();
    let mut out: Vec<Option<(u64, u32)>> = vec![None; pieces.len()];
    let claims: Vec<(usize, (u64, u32))> = pieces
        .iter()
        .enumerate()
        .filter_map(|(i, p)| record.get(&sha_hex(p)).map(|at| (i, *at)))
        .collect();
    let senders = BTreeSet::from([bulk.into_array()]);
    for window in claims.chunks(PARALLEL_READS) {
        let reads = window.iter().map(|&(i, (block, index))| {
            let senders = &senders;
            async move {
                let mined = a.tx_at(block, index).await?;
                let intact = crate::pieces::bodies(a, rid, senders, mined) == [pieces[i]];
                Ok::<_, crate::SyncError>((i, intact.then_some((block, index))))
            }
        });
        for (i, at) in try_join_all(reads).await? {
            out[i] = at;
        }
    }
    Ok(out)
}

/// Records where each piece of this upload landed, for a retry of the same push.
pub(crate) fn record(outgoing: &Path, pieces: &[&[u8]], at: &[(u64, u32)]) -> Result<()> {
    let r: Record = pieces
        .iter()
        .map(|p| sha_hex(p))
        .zip(at.iter().copied())
        .collect();
    write_json(&path(outgoing), &r)
}

/// The push landed: nothing is left to reuse.
pub(crate) fn clear(outgoing: &Path) {
    let _ = std::fs::remove_file(path(outgoing));
}
