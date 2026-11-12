//! Bulk pieces a refused push already paid for (issue #26, head-model 7.4). Readers take a
//! piece's bytes from its tx's calldata, live or not, so a piece that landed for a push
//! whose head tx was refused (`E_CONCURRENT_PUSH`) can be named by the retry instead of
//! being uploaded again; the retry first reads each one back and must find exactly its
//! bytes, sent by this device's `B_d`. Bulk pieces come from `B_d`, never from `C_d`, so
//! reuse leaves `pieces_created` and the rollback rule untouched. In a private repo a piece
//! is reused only if it was sealed under the epoch the retry seals under: after a key
//! change (a removal between the attempts) it is uploaded again, so nothing the new head
//! names stays readable under a key a removed member holds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use charter_core::Rid;
use charter_store::{Address, Arkiv, EntityData, MinedTx, tx_rows};
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
    if claims.is_empty() {
        return Ok(out);
    }
    let now = sealed_under(a, rid)?;
    let senders = BTreeSet::from([bulk.into_array()]);
    for window in claims.chunks(PARALLEL_READS) {
        let reads = window.iter().map(|&(i, (block, index))| {
            let (senders, now) = (&senders, &now);
            async move {
                let mined = a.tx_at(block, index).await?;
                let same = mined.as_ref().is_some_and(|m| epochs(m).all(|e| e == *now));
                let intact = same && crate::pieces::bodies(a, rid, senders, mined) == [pieces[i]];
                Ok::<_, crate::SyncError>((i, intact.then_some((block, index))))
            }
        });
        for (i, at) in try_join_all(reads).await? {
            out[i] = at;
        }
    }
    Ok(out)
}

/// The `ag.ep` a piece sent through `a` now carries (`None` when `a` does not seal), read
/// off a sealed empty piece: local crypto, no query.
fn sealed_under<A: Arkiv>(a: &A, rid: &Rid) -> Result<Option<String>> {
    let op = crate::pieces::piece_op(rid, &[])?;
    let tx = MinedTx {
        hash: Default::default(),
        from: Address::ZERO,
        to: Some(charter_store::ARKIV_ADDRESS),
        block: 0,
        index: 0,
        nonce: 0,
        input: charter_store::execute_calldata(&a.seal_ops(vec![op])?),
    };
    Ok(epochs(&tx).next().flatten())
}

/// The `ag.ep` of each row `tx` creates, as sent (before any opening).
fn epochs(tx: &MinedTx) -> impl Iterator<Item = Option<String>> {
    tx_rows(tx).into_iter().map(|r: EntityData| {
        r.attributes
            .into_iter()
            .flatten()
            .find(|at| at.name == "ag.ep")
            .map(|at| at.value.to_string())
    })
}

/// Records where each piece of this upload landed, for a retry of the same push.
pub(crate) fn record(outgoing: &Path, pieces: &[&[u8]], at: &[(u64, u32)]) -> Result<()> {
    // Added to what this push already recorded: a push may upload a pack and a ref table.
    let mut r: Record = read_json(&path(outgoing))
        .ok()
        .flatten()
        .unwrap_or_default();
    r.extend(pieces.iter().map(|p| sha_hex(p)).zip(at.iter().copied()));
    write_json(&path(outgoing), &r)
}

/// The push landed: nothing is left to reuse.
pub(crate) fn clear(outgoing: &Path) {
    let _ = std::fs::remove_file(path(outgoing));
}

#[cfg(test)]
mod tests {
    use charter_store::{
        AttributeValue, B256, MinedTx, Operation, Query, QueryOpts, QueryPage, Receipt, SignedTx,
        StoreError, TxHash, TxInfo, U256, create_op_flags,
    };

    use super::*;

    const RID: Rid = [4; 32];
    const BULK: Address = Address::repeat_byte(7);

    /// A piece op as a sealing backend sends it: tagged with epoch `ep` (`None`: unsealed).
    fn piece(bytes: &[u8], ep: Option<u64>) -> Operation {
        let mut attrs = vec![
            ("ag.kind", AttributeValue::Str("piece".into()), true),
            ("ag.rid", AttributeValue::Bytes32(RID), true),
            ("ag.v", AttributeValue::U64(1), false),
            ("ag.body", AttributeValue::Bytes(bytes.to_vec()), false),
        ];
        attrs.extend(ep.map(|e| ("ag.ep", AttributeValue::U64(e), true)));
        create_op_flags(&attrs, 20, 1, 1).expect("op")
    }

    /// A backend that seals under epoch `ep` and holds one landed piece tx, `landed`.
    struct Backend {
        ep: Option<u64>,
        landed: MinedTx,
    }

    impl Arkiv for Backend {
        async fn chain_id(&self) -> Result<u64, StoreError> {
            unreachable!()
        }
        async fn head(&self) -> Result<u64, StoreError> {
            unreachable!()
        }
        async fn base_fee(&self) -> Result<u128, StoreError> {
            unreachable!()
        }
        async fn query(&self, _: &Query, _: &QueryOpts) -> Result<QueryPage, StoreError> {
            unreachable!()
        }
        async fn entity_nonce(&self, _: Address) -> Result<u64, StoreError> {
            unreachable!()
        }
        async fn balance(&self, _: Address) -> Result<U256, StoreError> {
            unreachable!()
        }
        async fn tx_count(&self, _: Address) -> Result<u64, StoreError> {
            unreachable!()
        }
        async fn tx_count_mined(&self, _: Address) -> Result<u64, StoreError> {
            unreachable!()
        }
        async fn estimate(&self, _: Address, _: &[Operation]) -> Result<u64, StoreError> {
            unreachable!()
        }
        async fn send(&self, _: SignedTx) -> Result<TxHash, StoreError> {
            unreachable!()
        }
        async fn receipt(&self, _: TxHash) -> Result<Option<Receipt>, StoreError> {
            unreachable!()
        }
        async fn transaction(&self, _: TxHash) -> Result<Option<TxInfo>, StoreError> {
            unreachable!()
        }
        async fn mined_tx(&self, _: TxHash) -> Result<Option<MinedTx>, StoreError> {
            unreachable!()
        }
        async fn block_txs(&self, _: u64) -> Result<Vec<MinedTx>, StoreError> {
            Ok(vec![self.landed.clone()])
        }
        fn seal_ops(&self, _: Vec<Operation>) -> Result<Vec<Operation>, StoreError> {
            Ok(vec![piece(&[], self.ep)])
        }
    }

    fn landed(bytes: &[u8], ep: Option<u64>) -> MinedTx {
        MinedTx {
            hash: B256::ZERO,
            from: BULK,
            to: Some(charter_store::ARKIV_ADDRESS),
            block: 9,
            index: 0,
            nonce: 0,
            input: charter_store::execute_calldata(&[piece(bytes, ep)]),
        }
    }

    #[tokio::test]
    async fn a_piece_sealed_under_an_older_epoch_is_uploaded_again() {
        let out = tempfile::tempdir().expect("tmp");
        let bytes: &[u8] = b"pack piece";
        record(out.path(), &[bytes], &[(9, 0)]).expect("record");
        let reuse = |sealed_now: Option<u64>, sealed_then: Option<u64>| {
            let b = Backend {
                ep: sealed_now,
                landed: landed(bytes, sealed_then),
            };
            let dir = out.path().to_path_buf();
            async move {
                reusable(&b, &dir, &RID, BULK, &[bytes])
                    .await
                    .expect("reusable")
            }
        };
        assert_eq!(
            reuse(None, None).await,
            vec![Some((9, 0))],
            "public: reused"
        );
        assert_eq!(
            reuse(Some(3), Some(3)).await,
            vec![Some((9, 0))],
            "same epoch"
        );
        assert_eq!(reuse(Some(4), Some(3)).await, vec![None], "the key changed");
    }
}
