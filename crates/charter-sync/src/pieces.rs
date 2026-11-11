//! Format v2 pack bytes (docs/p5/head-model.md 4.3 and 8): snapshot packs as manifest plus
//! chunks, and the packs since the snapshot as 20-block pieces that are read back from the
//! creating tx's calldata, live or not.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use charter_core::{
    Address, AttrValue, Device, DeviceHead, ErrorCode, Hash32, HeadPack, Kind, ManifestBody, Observed, Rid,
    ZERO, verify_entity,
};
use charter_store::{Arkiv, AttributeValue, Operation, TxHash, create_op_flags, random_salt};
use futures_util::future::{try_join, try_join_all};

use crate::heads::{V2, calldata_rows, landing};
use crate::observe::{by_hashes, kind};
use crate::state::LandingJson;
use crate::{Result, SyncError, packs};

/// `ag.kind` of a piece.
pub const PIECE: &str = "piece";
/// Blocks a piece entity lives; readers take its bytes from calldata (head-model 4.3).
pub const PIECE_LIFETIME: u64 = 20;
/// Largest piece: one chunk.
pub const PIECE_SIZE: usize = charter_core::CHUNK_SIZE;
/// Creation flags of a piece: plain (nobody needs to extend or patch it).
const PIECE_FLAGS: u8 = 1;

/// The create op of one piece of `rid` (empty bytes: the marker piece).
pub fn piece_op(rid: &Rid, bytes: &[u8]) -> Result<Operation> {
    let attrs = [
        ("ag.kind", AttributeValue::Str(PIECE.into()), true),
        ("ag.rid", AttributeValue::Bytes32(*rid), true),
        ("ag.v", AttributeValue::U64(1), false),
        ("ag.body", AttributeValue::Bytes(bytes.to_vec()), false),
    ];
    Ok(create_op_flags(
        &attrs,
        PIECE_LIFETIME,
        random_salt(),
        PIECE_FLAGS,
    )?)
}

fn sha256(b: &[u8]) -> Hash32 {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).into()
}

fn piece_bytes(o: &Observed, rid: &Rid) -> Option<Vec<u8>> {
    let is_piece = o.attr("ag.kind") == Some(&AttrValue::Str(PIECE.into()))
        && o.attr("ag.rid") == Some(&AttrValue::Bytes32(*rid));
    match o.attr("ag.body") {
        Some(AttrValue::Bytes(b)) if is_piece => Some(b.clone()),
        _ => None,
    }
}

fn bad_pack(pack_id: &Hash32, what: &str) -> SyncError {
    SyncError::coded(
        ErrorCode::BadPack,
        format!("pack {}: {what}", hex::encode(pack_id)),
    )
}

/// The landing of `head`'s body: from the view, else found by scanning.
pub async fn head_landing<A: Arkiv>(
    a: &A,
    v2: &V2,
    dev: &Device,
    head: &DeviceHead,
) -> Result<LandingJson> {
    if let Some(l) = v2.landings.get(&head.h).filter(|l| l.block != u64::MAX) {
        return Ok(*l);
    }
    landing(a, dev, head, crate::heads::LANDING_WINDOW)
        .await?
        .ok_or_else(|| {
            SyncError::coded(
                ErrorCode::NotLive,
                format!(
                    "head {} of device {}: body tx not found within {} blocks of its `at`",
                    hex::encode(head.h),
                    crate::ids::to_text(&dev.nid),
                    crate::heads::LANDING_WINDOW
                ),
            )
        })
}

/// Piece txs read at once within one pack (the pack's bytes are held in memory anyway).
const PARALLEL_TXS: usize = 8;

/// The piece bodies of `rid` in tx `tx`, if a device address of `dev` sent it.
async fn tx_pieces<A: Arkiv>(
    a: &A,
    rid: &Rid,
    senders: &BTreeSet<Address>,
    tx: Hash32,
) -> Result<(Hash32, Vec<Vec<u8>>)> {
    let mined = a.mined_tx(TxHash::from(tx)).await?;
    let found = mined
        .filter(|m| senders.contains(&m.from.0.0))
        .map(|m| {
            calldata_rows(a, &m)
                .iter()
                .filter_map(|o| piece_bytes(o, rid))
                .collect()
        })
        .unwrap_or_default();
    Ok((tx, found))
}

/// Reads `pack`'s pieces from calldata, checks every SHA-256 and the pack's, writes `out`.
/// `here` is the tx that carried the listing body (pieces with `tx = 0`).
pub async fn download_pieces<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    here: &Hash32,
    pack: &HeadPack,
    out: &Path,
) -> Result<()> {
    let senders = BTreeSet::from([dev.control, dev.bulk]);
    let tx_of = |p: &charter_core::PieceRef| if p.tx == ZERO { *here } else { p.tx };
    let txs: BTreeSet<Hash32> = pack.pieces.iter().map(tx_of).collect();
    let txs: Vec<Hash32> = txs.into_iter().collect();
    let mut by_tx: BTreeMap<Hash32, Vec<Vec<u8>>> = BTreeMap::new();
    for window in txs.chunks(PARALLEL_TXS) {
        let reads = window.iter().map(|tx| tx_pieces(a, rid, &senders, *tx));
        by_tx.extend(try_join_all(reads).await?);
    }
    let mut bytes = vec![];
    for p in &pack.pieces {
        let tx = tx_of(p);
        let piece = by_tx[&tx]
            .iter()
            .find(|b| sha256(b) == p.sha256)
            .ok_or_else(|| {
                bad_pack(
                    &pack.pack_id,
                    &format!("piece missing from tx {}", hex::encode(tx)),
                )
            })?;
        bytes.extend_from_slice(piece);
    }
    if sha256(&bytes) != pack.pack_id {
        return Err(bad_pack(&pack.pack_id, "pieces do not hash to pack_id"));
    }
    std::fs::write(out, &bytes).map_err(|e| SyncError::io(out.display(), e))
}

/// The snapshot manifests `hashes` of `dev`'s head, signed by the device and created by `B_d`.
pub async fn snapshot_manifests<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    hashes: &[Hash32],
) -> Result<Vec<(Hash32, ManifestBody)>> {
    if hashes.is_empty() {
        return Ok(vec![]);
    }
    let observed = by_hashes(a, kind(Kind::Manifest), hashes).await?;
    hashes
        .iter()
        .map(|h| {
            observed
                .iter()
                .filter(|o| o.creator == dev.bulk)
                .filter_map(|o| verify_entity::<ManifestBody>(o, &dev.pk).ok())
                .find(|e| e.h == *h && e.body.rid == *rid)
                .map(|e| (*h, e.body))
                .ok_or_else(|| {
                    SyncError::coded(
                        ErrorCode::NotLive,
                        format!("snapshot manifest {} not live", hex::encode(h)),
                    )
                })
        })
        .collect()
}

/// Downloads one snapshot pack from `B_d`'s chunks.
pub async fn download_snapshot<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    m: &ManifestBody,
    out: &Path,
) -> Result<()> {
    packs::download(a, rid, m, &BTreeSet::from([dev.bulk]), out).await
}

type Fetched = Vec<(Hash32, Hash32, std::path::PathBuf)>;

/// Downloads every pack of `v2` not `in_cache`: `(listing h, pack_id, file)` per pack.
/// Per device, the snapshot (manifests, then chunks) and the pieces since it download at
/// the same time.
pub async fn download_v2<A: Arkiv>(
    a: &A,
    repo: &crate::repo::Repo,
    rid: &Rid,
    v2: &V2,
    in_cache: &(dyn Fn(&Hash32) -> bool + Sync),
) -> Result<Fetched> {
    let mut out: Fetched = vec![];
    for (dev, head) in v2.heads() {
        let fresh = |id: &Hash32| !in_cache(id) && !out.iter().any(|d| d.1 == *id);
        let todo: Vec<&HeadPack> = head
            .body
            .packs
            .iter()
            .filter(|p| fresh(&p.pack_id))
            .collect();
        // A snapshot pack also listed as a piece pack comes from the pieces (one writer per file).
        let wanted: Vec<Hash32> = head
            .body
            .snapshot
            .iter()
            .filter(|h| !in_cache(h))
            .copied()
            .collect();
        let skip: BTreeSet<Hash32> = out
            .iter()
            .map(|d| d.1)
            .chain(todo.iter().map(|p| p.pack_id))
            .collect();
        let (snap, pieces) = try_join(
            snapshot_packs(a, repo, rid, dev, &wanted, &skip, in_cache),
            piece_packs(a, repo, rid, v2, (dev, head), &todo),
        )
        .await?;
        out.extend(snap);
        out.extend(pieces);
    }
    Ok(out)
}

async fn snapshot_packs<A: Arkiv>(
    a: &A,
    repo: &crate::repo::Repo,
    rid: &Rid,
    dev: &Device,
    wanted: &[Hash32],
    skip: &BTreeSet<Hash32>,
    in_cache: &(dyn Fn(&Hash32) -> bool + Sync),
) -> Result<Fetched> {
    let mut out: Fetched = vec![];
    for (h, m) in snapshot_manifests(a, rid, dev, wanted).await? {
        let seen = skip.contains(&m.pack_id) || out.iter().any(|d| d.1 == m.pack_id);
        if !seen && !in_cache(&m.pack_id) {
            let tmp = repo.tmp().join(format!("{}.pack", hex::encode(m.pack_id)));
            download_snapshot(a, rid, dev, &m, &tmp).await?;
            out.push((h, m.pack_id, tmp));
        }
    }
    Ok(out)
}

async fn piece_packs<A: Arkiv>(
    a: &A,
    repo: &crate::repo::Repo,
    rid: &Rid,
    v2: &V2,
    (dev, head): (&Device, &DeviceHead),
    todo: &[&HeadPack],
) -> Result<Fetched> {
    let mut out: Fetched = vec![];
    if todo.is_empty() {
        return Ok(out);
    }
    let inline = todo.iter().any(|p| p.pieces.iter().any(|x| x.tx == ZERO));
    let here = match inline {
        true => head_landing(a, v2, dev, head).await?.tx,
        false => ZERO,
    };
    for p in todo {
        let tmp = repo.tmp().join(format!("{}.pack", hex::encode(p.pack_id)));
        download_pieces(a, rid, dev, &here, p, &tmp).await?;
        out.push((p.pack_id, p.pack_id, tmp));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use charter_store::tx_rows;

    #[test]
    fn piece_ops_read_back_from_calldata() {
        let op = piece_op(&[4; 32], b"pack bytes").expect("op");
        let tx = charter_store::MinedTx {
            hash: TxHash::ZERO,
            from: charter_store::Address::ZERO,
            to: Some(charter_store::ARKIV_ADDRESS),
            block: 9,
            index: 0,
            input: charter_store::execute_calldata(&[op]),
        };
        let rows = crate::observe::to_observed(
            tx_rows(&tx)
                .into_iter()
                .map(|mut r| {
                    r.key = Some(charter_store::B256::ZERO);
                    r.expires_at = Some(9);
                    r
                })
                .collect(),
        );
        assert_eq!(
            piece_bytes(&rows[0], &[4; 32]),
            Some(b"pack bytes".to_vec())
        );
        assert_eq!(piece_bytes(&rows[0], &[5; 32]), None, "other repo");
    }
}
