//! Head push content (docs/p5/head-model.md 13 and 14): which refs go inline or in a ref
//! table, and the pack bytes a push sends as a snapshot, bulk pieces or inline pieces.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use charter_core::codec::{self, Codec};
use charter_core::{
    ErrorCode, Hash32, HeadRef, MAX_PACK_SIZE, MAX_TABLE_PIECES, ManifestBody, Pack, PackPieces,
    PieceRun, RefTable, chunk_count, max_body_len, runs,
};
use charter_store::{Arkiv, TxHash};
use futures_util::future::try_join_all;

use crate::head_push::{HeadPush, INLINE_MAX, Plan};
use crate::ids::hex32;
use crate::pieces::{PIECE_SIZE, piece_op};
use crate::write::{chunk_op, entity_op, sign};
use crate::{Result, SyncError, packs, ref_table, warn};

use super::Content;

const RESEND_ROUNDS: usize = 3;
/// Snapshot manifests up to this many bytes in all are created by `C_d` in the head tx
/// (head-model 13.2): a block after their chunks, so GolemDB does not charge their index
/// terms as new twice, and with no marker; larger ones ride with the last chunks.
const MANIFESTS_IN_HEAD_MAX: usize = 32 * 1024;
/// Piece txs looked up by hash at once when a v2 head migrates.
const PARALLEL_LOOKUPS: usize = 8;

/// One snapshot pack ready to send: the git pack, its stored form and its manifest.
struct Built {
    raw: PathBuf,
    stored: PathBuf,
    m: ManifestBody,
    signed: crate::write::Signed,
}

fn sha256(b: &[u8]) -> Hash32 {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).into()
}

impl<A: Arkiv> HeadPush<'_, A> {
    /// `refs` limited to what the cache holds: a ref another device moved to objects this
    /// cache has not fetched keeps its cached value until the next fetch brings them.
    /// `cached` is the namespace as the cache holds it.
    pub(super) fn cached_refs(
        &self,
        refs: &charter_core::RefsBody,
        cached: &BTreeMap<String, String>,
    ) -> Result<charter_core::RefsBody> {
        let git = self.repo.git();
        let oids: Vec<String> = refs.refs.iter().map(|e| hex::encode(&e.oid)).collect();
        let have: BTreeSet<String> = git.present(&oids)?.into_iter().collect();
        let ns = crate::repo::namespace(&self.nid());
        let mut out = refs.clone();
        out.refs = refs
            .refs
            .iter()
            .filter_map(|e| match have.contains(&hex::encode(&e.oid)) {
                true => Some(e.clone()),
                false => cached
                    .get(&format!("{ns}{}", e.name))
                    .and_then(|o| hex::decode(o).ok())
                    .map(|oid| charter_core::RefEntry {
                        name: e.name.clone(),
                        oid,
                    }),
            })
            .collect();
        Ok(out)
    }

    /// How the next body carries `plan.refs` (head-model 14.3): inline when a v3 body
    /// fits, else with the previous ref table or a new one.
    pub(super) fn refs_choice(&self, plan: &Plan) -> ref_table::Choice {
        let rows = plan.prev.as_ref().and_then(|p| {
            let t = p.head.refs_table.as_ref()?;
            Some((t, ref_table::cached(&t.table_id)?, p.landing.block))
        });
        let prev = rows.as_ref().map(|(t, r, l)| (*t, r.as_slice(), *l));
        let max = max_body_len(self.dev.pk().alg().sig_len());
        // The carried packs and one new pack, as `inline_fits` bounds them.
        let packs = packs_bound(plan) + 40 + 2 * 20;
        ref_table::choose(&plan.refs, prev, packs, max)
    }

    /// Sends a new ref table's pieces from `B_d` before the head tx (reusing pieces a
    /// refused attempt landed, issue #26); the table the body names and whether it is new.
    pub(super) async fn upload_table(
        &self,
        choice: &ref_table::Choice,
    ) -> Result<Option<(RefTable, bool)>> {
        let rows = match choice {
            ref_table::Choice::Inline => return Ok(None),
            ref_table::Choice::Reuse { table, .. } => return Ok(Some((table.clone(), false))),
            ref_table::Choice::New { table, .. } => table,
        };
        let (codec, stored, table_id) = ref_table::stored(rows);
        let pieces: Vec<&[u8]> = stored.chunks(PIECE_SIZE).collect();
        if pieces.len() > MAX_TABLE_PIECES {
            return Err(SyncError::coded(
                ErrorCode::RefsTooLarge,
                format!(
                    "this device's ref table needs {} pieces, at most {MAX_TABLE_PIECES} fit",
                    pieces.len()
                ),
            ));
        }
        let at = self.bulk_pieces(&pieces).await?;
        ref_table::remember(table_id, std::sync::Arc::new(rows.clone()));
        let t = RefTable {
            table_id,
            codec: codec as u8,
            runs: runs(&at),
            stamp: 0,
        };
        Ok(Some((t, true)))
    }

    pub(super) async fn content(&self, plan: &Plan, inline_refs: &[HeadRef]) -> Result<Content> {
        let kept: Vec<Hash32> = match &plan.prev {
            Some(p) if !plan.keep_layers.is_empty() && !plan.after.is_empty() => {
                p.head.body.snapshot.clone()
            }
            _ => vec![],
        };
        if plan.snapshot {
            // With kept layers, one more layer on them (speed-and-cost 2.8), not a rewrite.
            let tips: Vec<String> = plan.after.values().map(hex::encode).collect();
            let prefix = self.repo.outgoing().join("snap");
            let git = self.repo.git();
            let files = match tips.is_empty() {
                true => vec![],
                false => git.snapshot_packs(&tips, &plan.keep_layers, &prefix, MAX_PACK_SIZE)?,
            };
            let mut c = self.snapshot(files).await?;
            c.snapshot = [kept, c.snapshot].concat();
            return Ok(c);
        }
        let mut packs = self.as_runs(plan).await?;
        let mut files = vec![];
        let mut inline = vec![];
        if !plan.thin.is_empty() {
            let pack_id = sha256(&plan.stored);
            let file = self
                .repo
                .outgoing()
                .join(format!("v3-{}.pack", hex32(&pack_id)));
            std::fs::write(&file, &plan.thin).map_err(|e| SyncError::io(file.display(), e))?;
            files.push((file, pack_id));
            let pieces = match plan.stored.len() <= INLINE_MAX && inline_fits(plan, inline_refs) {
                true => {
                    inline.push(plan.stored.clone());
                    vec![PieceRun::HERE]
                }
                false => {
                    let pieces: Vec<&[u8]> = plan.stored.chunks(PIECE_SIZE).collect();
                    runs(&self.bulk_pieces(&pieces).await?)
                }
            };
            packs.push(Pack {
                pack_id,
                codec: plan.codec,
                pieces: PackPieces::Runs(pieces),
            });
        }
        Ok(Content {
            uploaded: inline.is_empty() && !files.is_empty(),
            snapshot: plan
                .prev
                .as_ref()
                .map(|p| p.head.body.snapshot.clone())
                .unwrap_or_default(),
            packs,
            inline,
            manifests: vec![],
            files,
        })
    }

    /// `plan.carried` with every pack listed by runs: on the push that migrates a v2 head,
    /// each piece tx's `(block, index)` comes from the landing already known or one
    /// `eth_getTransactionByHash` (head-model 13.4).
    async fn as_runs(&self, plan: &Plan) -> Result<Vec<Pack>> {
        let mut at: BTreeMap<Hash32, (u64, u32)> = BTreeMap::new();
        // v0.4.0 stored `index: u32::MAX` when it could not read the index: look it up.
        if let Some(p) = plan.prev.as_ref().filter(|p| p.landing.index != u32::MAX) {
            at.insert(p.landing.tx, (p.landing.block, p.landing.index));
        }
        let lookup: BTreeSet<Hash32> = plan
            .carried
            .iter()
            .flat_map(|p| match &p.pieces {
                PackPieces::Txs(t) => t.iter().map(|x| x.tx).collect(),
                PackPieces::Runs(_) => vec![],
            })
            .filter(|tx| !at.contains_key(tx))
            .collect();
        let lookup: Vec<Hash32> = lookup.into_iter().collect();
        for window in lookup.chunks(PARALLEL_LOOKUPS) {
            let reads = window.iter().map(|tx| async move {
                let m = self.a.mined_tx(TxHash::from(*tx)).await?.ok_or_else(|| {
                    SyncError::coded(
                        ErrorCode::NotLive,
                        format!("piece tx {} of this device's head not found", hex32(tx)),
                    )
                })?;
                Ok::<_, SyncError>((*tx, (m.block, m.index)))
            });
            at.extend(try_join_all(reads).await?);
        }
        Ok(plan
            .carried
            .iter()
            .map(|p| match &p.pieces {
                PackPieces::Runs(_) => p.clone(),
                PackPieces::Txs(t) => {
                    let mut positions: Vec<(u64, u32)> = t.iter().map(|x| at[&x.tx]).collect();
                    // A tx holding several pieces of the pack is named once (13.1).
                    positions.dedup();
                    Pack {
                        pieces: PackPieces::Runs(runs(&positions)),
                        ..p.clone()
                    }
                }
            })
            .collect())
    }

    /// Bulk pieces from `B_d`, one tx each, before the head (crash-safe: they just expire);
    /// where each landed. Pieces a refused attempt of this push landed and that read back
    /// intact are named again instead of sent (issue #26).
    async fn bulk_pieces(&self, pieces: &[&[u8]]) -> Result<Vec<(u64, u32)>> {
        let rid = self.rid();
        let out = self.repo.outgoing();
        let bulk = self.dev.bulk_address(&rid);
        let mut at = crate::piece_reuse::reusable(self.a, &out, &rid, bulk, pieces).await?;
        let todo: Vec<usize> = (0..pieces.len()).filter(|i| at[*i].is_none()).collect();
        let jobs = todo
            .iter()
            .map(|&i| Ok(vec![piece_op(&rid, pieces[i])?]))
            .collect::<Result<Vec<_>>>()?;
        let landed = self.send_from(&self.dev.bulk(&rid), jobs).await?;
        for l in landed {
            at[todo[l.job]] = Some((l.receipt.block_number, l.receipt.index));
        }
        let at = at
            .into_iter()
            .map(|a| a.ok_or_else(|| SyncError::coded(ErrorCode::Internal, "a piece did not land")))
            .collect::<Result<Vec<_>>>()?;
        crate::piece_reuse::record(&out, pieces, &at)?;
        Ok(at)
    }

    /// The git pack `raw` in its stored form (head-model 13.3) and its unsigned manifest:
    /// a brotli stream of it (`pack_kind = 3`, written next to it) when that is smaller,
    /// else the pack itself (`pack_kind = 1`).
    fn manifest_of(&self, raw: &Path) -> Result<(PathBuf, ManifestBody)> {
        let bytes = std::fs::read(raw).map_err(|e| SyncError::io(raw.display(), e))?;
        // Readers decode at most this much (head-model 13.3); never publish a pack past it.
        if bytes.len() as u64 > MAX_PACK_SIZE {
            return Err(SyncError::coded(
                ErrorCode::BadPack,
                format!("snapshot pack of {} B exceeds 100 MiB", bytes.len()),
            ));
        }
        let (codec, encoded) = codec::encode(&bytes);
        let (stored, pack_kind) = match codec {
            Codec::Brotli => {
                // `<prefix>-<trailer>.br`, so the manifest cache reads the same trailer.
                let p = raw.with_extension("br");
                std::fs::write(&p, &encoded).map_err(|e| SyncError::io(p.display(), e))?;
                (p, packs::BROTLI_PACK)
            }
            Codec::Raw => (raw.to_path_buf(), 1),
        };
        let (pack_id, size, _, chunk_hashes) = packs::describe(&stored)?;
        let m = ManifestBody {
            rid: self.rid(),
            pack_id,
            trailer: bytes[bytes.len().saturating_sub(20)..].to_vec(),
            size,
            chunk_size: charter_core::CHUNK_SIZE as u64,
            chunk_hashes,
            requires: vec![],
            pack_kind,
        };
        Ok((stored, m))
    }

    /// Snapshot packs `files` as manifest plus chunks from `B_d`; the manifests go in the
    /// head tx when they fit [`MANIFESTS_IN_HEAD_MAX`].
    async fn snapshot(&self, files: Vec<PathBuf>) -> Result<Content> {
        let mut built = vec![];
        for raw in files {
            let (stored, m) = self.manifest_of(&raw)?;
            let signed = sign(self.dev.sk(), &m)?;
            built.push(Built {
                raw,
                stored,
                m,
                signed,
            });
        }
        let manifest_bytes: usize = built.iter().map(|b| b.signed.bytes.len()).sum();
        let in_head = manifest_bytes <= MANIFESTS_IN_HEAD_MAX;
        for b in &built {
            self.upload_snapshot_pack(&b.stored, &b.m, &b.signed.bytes, in_head)
                .await?;
        }
        let hashes: Vec<Hash32> = built.iter().map(|b| b.signed.h).collect();
        let stored: Vec<(PathBuf, Hash32)> = built
            .iter()
            .map(|b| (b.stored.clone(), b.m.pack_id))
            .collect();
        crate::manifest_cache::record_files(self.repo, &hashes, &stored)?;
        for b in built.iter().filter(|b| b.stored != b.raw) {
            let _ = std::fs::remove_file(&b.stored);
        }
        Ok(Content {
            uploaded: !built.is_empty(),
            snapshot: hashes,
            packs: vec![],
            inline: vec![],
            manifests: match in_head {
                true => built.iter().map(|b| b.signed.bytes.clone()).collect(),
                false => vec![],
            },
            files: built.into_iter().map(|b| (b.raw, b.m.pack_id)).collect(),
        })
    }

    /// Chunks of `f` from `B_d` until all landed, and the manifest with the last ones
    /// unless it goes in the head tx (`in_head`).
    async fn upload_snapshot_pack(
        &self,
        f: &Path,
        m: &ManifestBody,
        manifest: &[u8],
        in_head: bool,
    ) -> Result<()> {
        let rid = self.rid();
        let bulk = BTreeSet::from([self.dev.bulk_address(&rid).into_array()]);
        let mut sent_manifest = in_head;
        for _ in 0..=RESEND_ROUNDS {
            let have = packs::landed(self.a, &rid, &m.pack_id, &bulk).await?;
            let missing: Vec<u64> = (0..chunk_count(m.size))
                .filter(|i| !have.contains_key(i))
                .collect();
            if missing.is_empty() && sent_manifest {
                return Ok(());
            }
            let mut file = std::fs::File::open(f).map_err(|e| SyncError::io(f.display(), e))?;
            let batches: Vec<&[u64]> = missing.chunks(self.cfg.chunk_batch.max(1)).collect();
            for (n, batch) in batches.iter().enumerate() {
                let mut jobs = vec![];
                for &i in *batch {
                    let bytes = packs::read_chunk(&mut file, i, m.size)
                        .map_err(|e| SyncError::io(f.display(), e))?;
                    jobs.push(vec![chunk_op(
                        &rid,
                        &m.pack_id,
                        i,
                        &bytes,
                        self.cfg.data_lifetime,
                    )?]);
                }
                // The manifest rides with the last chunks instead of a block of its own:
                // readers reach it only through the head, which lands after both.
                let with_manifest = !sent_manifest && n + 1 == batches.len();
                if with_manifest {
                    jobs.push(vec![entity_op::<ManifestBody>(
                        manifest,
                        self.cfg.data_lifetime,
                    )?]);
                }
                match self.send_from(&self.dev.bulk(&rid), jobs).await {
                    Ok(_) => sent_manifest |= with_manifest,
                    Err(e) => warn(&format!("snapshot upload: {e}; checking what landed")),
                }
            }
            if !sent_manifest && batches.is_empty() {
                let op = entity_op::<ManifestBody>(manifest, self.cfg.data_lifetime)?;
                self.send_from(&self.dev.bulk(&rid), vec![vec![op]]).await?;
                sent_manifest = true;
            }
        }
        Err(SyncError::coded(
            ErrorCode::UploadIncomplete,
            format!(
                "snapshot pack {}: chunks still missing; run git push again",
                hex32(&m.pack_id)
            ),
        ))
    }
}

/// Upper bound of the head body's bytes plus 16 KiB of inline pieces stays inside one tx
/// (head-model 4.2); otherwise the pack goes as bulk pieces.
fn inline_fits(plan: &Plan, inline_refs: &[HeadRef]) -> bool {
    let refs: usize = inline_refs.iter().map(|e| e.name.len() + 2 * 32 + 32).sum();
    let body = 512 + refs + packs_bound(plan) + charter_core::ENVELOPE_OVERHEAD + 3_373;
    // Attribute and ABI framing of the head and the piece ops, generously.
    body + body / 4 + INLINE_MAX + 8_192 <= charter_store::MAX_RAW_TX_BYTES
}

/// Upper bound of the body bytes of the packs `plan` carries.
fn packs_bound(plan: &Plan) -> usize {
    plan.carried
        .iter()
        .map(|p| match &p.pieces {
            PackPieces::Txs(t) => t.len() * 72 + 40,
            PackPieces::Runs(r) => r.len() * 20 + 40,
        })
        .sum()
}
