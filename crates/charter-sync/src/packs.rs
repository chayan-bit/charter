//! Pack bytes on Arkiv: chunk presence by metadata, chunk download in pages (design 4.3
//! step 5: 16 per page, 8 pages at a time, halving on 413 / 5xx), reassembly and checks.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use charter_core::{
    Address, CHUNK_SIZE, ErrorCode, Hash32, Kind, ManifestBody, Rid, chunk_count, verify_chunk,
};
use charter_crypto::sha256;
use charter_store::paging::keyset;
use charter_store::{
    Arkiv, Attrs, B256, EntityData, Pred, Query, QueryOpts, RowExt, Select, StoreError, attr,
};
use futures_util::future::join_all;

use crate::observe::{b32, creators, kind};
use crate::{Result, SyncError};

const PAGE: u64 = 16;
const PARALLEL_PAGES: usize = 8;

/// A landed chunk's entity key and expiry, by index (first created per index wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkMeta {
    pub key: Hash32,
    pub expires_at: u64,
}

fn chunk_filter(rid: &Rid, pack_id: &Hash32, bulk: &BTreeSet<Address>) -> Pred {
    Pred::And(vec![
        kind(Kind::Chunk),
        b32("ag.rid", rid),
        b32("ag.pack", pack_id),
        creators(bulk),
    ])
}

/// Index → first chunk entity created by a bulk address (metadata only, no bodies).
pub async fn landed<A: Arkiv>(
    a: &A,
    rid: &Rid,
    pack_id: &Hash32,
    bulk: &BTreeSet<Address>,
) -> Result<BTreeMap<u64, ChunkMeta>> {
    if bulk.is_empty() {
        return Ok(BTreeMap::new());
    }
    let select = Select {
        expires_at: true,
        attrs: Attrs::Named(vec!["ag.i".into()]),
        ..Select::default()
    };
    let rows = keyset(a, &chunk_filter(rid, pack_id, bulk), "ag.i", 0, select).await?;
    let mut out = BTreeMap::new();
    for r in rows {
        let (Some(i), Some(key), Some(exp)) = (r.attr_u64("ag.i"), r.key, r.expires_at) else {
            continue;
        };
        out.entry(i).or_insert(ChunkMeta {
            key: key.0,
            expires_at: exp,
        });
    }
    Ok(out)
}

fn not_live(m: &ManifestBody, i: u64) -> SyncError {
    SyncError::coded(
        ErrorCode::NotLive,
        format!(
            "pack {} chunk {i} of {} is not live on Arkiv (expired or deleted). \
             This repo's keepalive lapsed; the data cannot be recovered through queries. \
             Peers that cloned earlier still hold it locally and can re-push it.",
            hex::encode(m.pack_id),
            chunk_count(m.size)
        ),
    )
}

/// Chunk rows for indices `start..=end`, halving the page on 413 / 5xx.
async fn page<A: Arkiv>(a: &A, filter: &Pred, start: u64, end: u64) -> Result<Vec<EntityData>> {
    let mut out = vec![];
    let mut ranges = vec![(start, end)];
    while let Some((s, e)) = ranges.pop() {
        let q = Query::from(Pred::And(vec![
            filter.clone(),
            Pred::ge(attr("ag.i"), s),
            Pred::le(attr("ag.i"), e),
        ]))
        .order_by_u64("ag.i");
        let n = e - s + 1;
        let opts = QueryOpts {
            limit: u32::try_from(n.min(100)).unwrap_or(100),
            ..QueryOpts::with_attrs(&["ag.i", "ag.body"])
        };
        match a.query(&q, &opts).await {
            Err(StoreError::Http { status, .. }) if (status == 413 || status >= 500) && n > 1 => {
                let mid = s + n / 2;
                ranges.push((mid, e));
                ranges.push((s, mid - 1));
            }
            other => out.extend(other?.rows),
        }
    }
    Ok(out)
}

/// The first row per index whose bytes match the manifest (retry duplicates are expected).
fn pick(m: &ManifestBody, start: u64, end: u64, rows: Vec<EntityData>) -> Result<Vec<Vec<u8>>> {
    let mut found: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    for r in rows {
        let (Some(i), Some(body)) = (r.attr_u64("ag.i"), r.attr_bytes("ag.body")) else {
            continue;
        };
        if (start..=end).contains(&i) && !found.contains_key(&i) && verify_chunk(m, i, &body) {
            found.insert(i, body);
        }
    }
    (start..=end)
        .map(|i| found.remove(&i).ok_or_else(|| not_live(m, i)))
        .collect()
}

/// Downloads, checks and writes the pack of `m` to `out` (chunk hashes, `pack_id`, trailer).
pub async fn download<A: Arkiv>(
    a: &A,
    rid: &Rid,
    m: &ManifestBody,
    bulk: &BTreeSet<Address>,
    out: &Path,
) -> Result<()> {
    let filter = chunk_filter(rid, &m.pack_id, bulk);
    let n = chunk_count(m.size);
    let mut file = std::fs::File::create(out).map_err(|e| SyncError::io(out.display(), e))?;
    let starts: Vec<u64> = (0..n).step_by(PAGE as usize).collect();
    for window in starts.chunks(PARALLEL_PAGES) {
        let pages = window.iter().map(|&s| {
            let e = (s + PAGE - 1).min(n - 1);
            let filter = &filter;
            async move { (s, e, page(a, filter, s, e).await) }
        });
        for (s, e, rows) in join_all(pages).await {
            for chunk in pick(m, s, e, rows?)? {
                file.write_all(&chunk)
                    .map_err(|e| SyncError::io(out.display(), e))?;
            }
        }
    }
    file.sync_all()
        .map_err(|e| SyncError::io(out.display(), e))?;
    check_pack(m, out)?;
    match m.pack_kind {
        BROTLI_PACK => decode_in_place(m, out),
        _ => Ok(()),
    }
}

/// `pack_kind` of a brotli stream of a self-contained pack (head-model 13.1).
pub const BROTLI_PACK: u64 = 3;

fn bad(m: &ManifestBody, what: &str) -> SyncError {
    SyncError::coded(
        ErrorCode::BadPack,
        format!("pack {}: {what}", hex::encode(m.pack_id)),
    )
}

/// The assembled bytes hash to `pack_id` and have the declared size; a raw pack (kind 1)
/// also ends with the trailer, a brotli one is checked after [`decode_in_place`].
pub fn check_pack(m: &ManifestBody, path: &Path) -> Result<()> {
    let (pack_id, size, trailer, _) = describe(path)?;
    if pack_id != m.pack_id || size != m.size {
        return Err(bad(m, "assembled bytes do not hash to pack_id"));
    }
    if m.pack_kind != BROTLI_PACK && !trailer.ends_with(&m.trailer) {
        return Err(bad(m, "trailer does not match the manifest"));
    }
    Ok(())
}

/// Replaces the checked brotli stream at `path` with the git pack inside, under the 100 MiB
/// cap, and checks its trailer.
fn decode_in_place(m: &ManifestBody, path: &Path) -> Result<()> {
    let err = |e: std::io::Error| SyncError::io(path.display(), e);
    let stored = std::fs::read(path).map_err(err)?;
    let pack = charter_core::codec::decode(
        charter_core::codec::Codec::Brotli,
        &stored,
        charter_core::MAX_PACK_SIZE,
    )
    .map_err(|e| bad(m, &e.to_string()))?;
    if !pack.ends_with(&m.trailer) {
        return Err(bad(m, "trailer does not match the manifest"));
    }
    std::fs::write(path, &pack).map_err(err)
}

/// Reads chunk `i` of a pack file (for uploads; files can be 100 MiB, so no full read).
pub fn read_chunk(file: &mut std::fs::File, i: u64, size: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let start = i * CHUNK_SIZE as u64;
    let len = size.saturating_sub(start).min(CHUNK_SIZE as u64);
    let mut buf = vec![0u8; usize::try_from(len).unwrap_or(CHUNK_SIZE)];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// `(pack_id, size, trailer, chunk hashes)` of a pack file, streamed chunk by chunk.
/// The trailer is the last 20 bytes (SHA-1 repos, the only object format P1 writes).
pub fn describe(path: &Path) -> Result<(Hash32, u64, Vec<u8>, Vec<u8>)> {
    use sha2::{Digest, Sha256};
    let err = |e: std::io::Error| SyncError::io(path.display(), e);
    let mut f = std::fs::File::open(path).map_err(err)?;
    let size = f.metadata().map_err(err)?.len();
    let mut whole = Sha256::new();
    let mut hashes = Vec::with_capacity(32 * chunk_count(size) as usize);
    let mut tail: Vec<u8> = vec![];
    for i in 0..chunk_count(size) {
        let c = read_chunk(&mut f, i, size).map_err(err)?;
        whole.update(&c);
        hashes.extend_from_slice(&sha256(&c));
        // The trailer may straddle the last chunk boundary, so keep a rolling 20-byte tail.
        tail.extend_from_slice(&c[c.len().saturating_sub(20)..]);
        tail.drain(..tail.len().saturating_sub(20));
    }
    Ok((whole.finalize().into(), size, tail, hashes))
}

pub fn b256(h: &Hash32) -> B256 {
    B256::from(*h)
}
