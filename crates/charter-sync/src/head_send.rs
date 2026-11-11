//! Format v2 push, part 2 (docs/p5/head-model.md 7.1): the identity's first write and
//! device delegation, the snapshot or bulk pieces, the head tx, the re-check after the
//! receipt (`E_LOST_RACE`) and the local commit. A push returns once its bytes are on chain.

use std::collections::BTreeSet;
use std::time::Duration;

use charter_core::{
    Body, DeviceBody, ErrorCode, Hash32, HeadBody, HeadPack, ManifestBody, PieceRef, ZERO,
    chunk_count, entity_attributes, resolve_stamps,
};
use charter_store::{Arkiv, Operation, U256, create_op_flags, patch_op, random_salt};

use crate::head_push::{HeadPush, INLINE_MAX, Plan};
use crate::heads::{LANDING_WINDOW, V2};
use crate::ids::hex32;
use crate::pieces::{PIECE_SIZE, piece_op};
use crate::push_send::{Cost, first_write_ops};
use crate::repo::staging;
use crate::state::{LandingJson, State};
use crate::write::{self, chunk_op, entity_op, sign, wire};
use crate::{Result, SyncError, packs, warn};

/// Creation flags of a head: `permissionlessExtension`, not readonly, so `C_d` can patch it.
const HEAD_FLAGS: u8 = 2;
/// Waits for this device's earlier txs before signing (a resumed push after a crash).
const IDLE_POLLS: usize = 60;
const RESEND_ROUNDS: usize = 3;
/// Gas a head tx with a full 16 KiB inline piece stays under (MEASURED about 0.32M with
/// 2.7 KiB inline on Rabdi, 2026-11-11).
const HEAD_GAS: u64 = 1_200_000;
/// Head txs one top-up of `C_d` pays for.
const PREFUND_HEADS: u64 = 4;

/// Pack bytes this push sends, in head form.
struct Content {
    snapshot: Vec<Hash32>,
    packs: Vec<HeadPack>,
    /// Pieces sent in the head tx (at least the marker piece).
    inline: Vec<Vec<u8>>,
    /// Local pack files to index on commit, and their pack ids.
    files: Vec<(std::path::PathBuf, Hash32)>,
}

fn sha256(b: &[u8]) -> Hash32 {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).into()
}

impl<A: Arkiv> HeadPush<'_, A> {
    fn has_bind(&self) -> bool {
        self.view
            .resolved
            .get(&self.nid())
            .is_some_and(|p| !p.binds.links.is_empty())
    }

    fn has_device(&self) -> bool {
        let me = self.me();
        self.view
            .peers
            .get(&self.nid())
            .and_then(|p| p.v2.as_ref())
            .is_some_and(|v| v.device(&me).is_some())
    }

    fn device_op(&self) -> Result<Operation> {
        let rid = self.rid();
        let pk = self.dev.pk();
        let body = DeviceBody {
            rid,
            alg: pk.alg().id(),
            pk: pk.as_bytes().to_vec(),
            control: self.dev.control_address(&rid).into_array(),
            bulk: self.dev.bulk_address(&rid).into_array(),
        };
        entity_op::<DeviceBody>(
            &sign(self.keys.sk(), &body)?.bytes,
            self.cfg.control_lifetime,
        )
    }

    /// `[peer, bind_0]` if this identity never wrote to the repo, then this device's delegation.
    fn first_ops(&self) -> Result<Vec<Operation>> {
        let mut ops = vec![];
        if !self.has_bind() {
            ops = first_write_ops(self.keys, &self.rid(), self.cfg)?;
        }
        if !self.has_device() {
            ops.push(self.device_op()?);
        }
        Ok(ops)
    }

    /// Sends `jobs` from `signer`, topped up from the wallet: `B_d` sends independent data
    /// txs (pieces, chunks, manifests) with two blocks of gas in flight, every other
    /// address sends ordered runs.
    async fn send_from(
        &self,
        signer: &alloy_signer_local::PrivateKeySigner,
        jobs: Vec<Vec<Operation>>,
    ) -> Result<Vec<charter_store::pipeline::Landed>> {
        let cfg = match signer.address() == self.dev.bulk_address(&self.rid()) {
            true => self.cfg.pipeline.bulk(),
            false => self.cfg.pipeline.ordered(),
        };
        write::send_funded(self.a, signer, &self.keys.wallet(), &cfg, jobs).await
    }

    /// Refuses to sign while an earlier tx of `C_d` is still pending (head-model 7.2);
    /// `true` when it had to wait (so the view's heads may be stale).
    async fn ensure_idle(&self) -> Result<bool> {
        let c = self.dev.control_address(&self.rid());
        for round in 0..IDLE_POLLS {
            let (pending, mined) =
                futures_util::future::try_join(self.a.tx_count(c), self.a.tx_count_mined(c))
                    .await?;
            if pending == mined {
                return Ok(round > 0);
            }
            tokio_sleep(self.cfg.pipeline.poll * 2).await;
        }
        Err(SyncError::coded(
            ErrorCode::ConcurrentPush,
            "an earlier head tx of this device is still pending; push again once it lands",
        ))
    }

    /// Rough predicted spend: the head tx plus every piece, chunk and manifest at estimate.
    pub async fn cost(&self) -> Result<Cost> {
        let rid = self.rid();
        let bulk = self.dev.bulk_address(&rid);
        let base_fee = self.a.base_fee().await?;
        let mut control_ops = self.first_ops()?;
        control_ops.push(piece_op(&rid, &vec![0xa5; INLINE_MAX])?);
        let control_gas =
            write::estimate(self.a, self.keys.control_address(&rid), &control_ops).await?;
        let state = State::load(&self.repo.state_path())?;
        let plan = self.plan(&state).await?;
        let bytes = match plan.snapshot {
            true => plan.after.len() as u64 * PIECE_SIZE as u64,
            false => plan.thin.len() as u64,
        };
        let piece =
            write::estimate(self.a, bulk, &[piece_op(&rid, &vec![0xa5; PIECE_SIZE])?]).await?;
        let n = bytes.div_ceil(PIECE_SIZE as u64);
        let bulk_gas = n * piece;
        let pc = &self.cfg.pipeline;
        Ok(Cost {
            control_gas,
            bulk_gas,
            base_fee,
            spend: U256::from(control_gas + bulk_gas) * U256::from(base_fee + pc.tip),
            reserve: pc.reserve(n + 2, piece.max(control_gas), base_fee),
        })
    }

    /// Steps 1 to 6 end to end.
    pub async fn run(self) -> Result<()> {
        let current = self.setup().await?;
        let state = State::load(&self.repo.state_path())?;
        let plan = Box::pin(self.plan_from(&state, current)).await?;
        if !current {
            // The program check ran on the listing; run it again on the fresh heads.
            let rid = self.rid();
            let (before, after) = (
                crate::head_push::refs_body(rid, &plan.before),
                crate::head_push::refs_body(rid, &plan.after),
            );
            crate::program::check_refs_from(
                &self.view,
                &self.nid(),
                Some(&before),
                &after,
                self.repo.git(),
            )?;
        }
        Box::pin(self.finish(plan)).await
    }

    fn pending_head_path(&self) -> std::path::PathBuf {
        self.repo.outgoing().join("head-pending")
    }

    /// The first write and device delegation if missing, then waits for `C_d` to be idle
    /// (and so for a head tx an interrupted push left in flight) while topping it up.
    /// `true` when nothing was sent or landed since the view's sync.
    pub async fn setup(&self) -> Result<bool> {
        let first = self.first_ops()?;
        let sent_first = !first.is_empty();
        if sent_first {
            self.send_from(&self.keys.control(&self.rid()), vec![first])
                .await?;
        }
        let (waited, ()) =
            futures_util::future::try_join(self.ensure_idle(), self.prefund_control()).await?;
        let _ = std::fs::remove_file(self.pending_head_path());
        Ok(!sent_first && !waited)
    }

    /// Tops `C_d` up for several head txs at once when it cannot pay for one, so most
    /// pushes send no top-up before their head tx.
    async fn prefund_control(&self) -> Result<()> {
        let c = self.dev.control_address(&self.rid());
        let (fee, have) =
            futures_util::future::try_join(self.a.base_fee(), self.a.balance(c)).await?;
        let one = self.cfg.pipeline.tx_reserve(HEAD_GAS, U256::ZERO, fee);
        let Some(amount) = charter_store::PipelineConfig::top_up(have, one, PREFUND_HEADS) else {
            return Ok(());
        };
        let chain = self.a.chain_id().await?;
        let cfg = self.cfg.pipeline.clone();
        charter_store::TxPipeline::new(self.a, self.keys.wallet(), chain, cfg)
            .transfer(c, amount)
            .await?;
        Ok(())
    }

    /// Sends what `plan` says, re-checks and commits.
    pub async fn finish(self, plan: Plan) -> Result<()> {
        let mut state = State::load(&self.repo.state_path())?;
        // Boxed: these futures are large, and tests poll them on 2 MiB threads.
        let content = Box::pin(self.content(&plan)).await?;
        let landed = Box::pin(self.send_heads(&plan, &content, &mut state)).await?;
        let v2 = Box::pin(self.own_v2(&state))
            .await?
            .ok_or_else(|| SyncError::coded(ErrorCode::Internal, "own heads vanished"))?;
        self.recheck(&v2)?;
        self.commit(&v2, &content, landed, &mut state)
    }

    /// `refs` limited to what the cache holds: a ref another device moved to objects this
    /// cache has not fetched keeps its cached value until the next fetch brings them.
    fn cached_refs(&self, refs: &charter_core::RefsBody) -> Result<charter_core::RefsBody> {
        let git = self.repo.git();
        let oids: Vec<String> = refs.refs.iter().map(|e| hex::encode(&e.oid)).collect();
        let have: BTreeSet<String> = git.present(&oids)?.into_iter().collect();
        let ns = crate::repo::namespace(&self.nid());
        let cached: std::collections::BTreeMap<String, String> =
            git.refs(&ns)?.into_iter().collect();
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

    async fn content(&self, plan: &Plan) -> Result<Content> {
        if plan.snapshot {
            return self.snapshot(plan).await;
        }
        let mut packs = plan.carried.clone();
        let mut files = vec![];
        let mut inline = vec![];
        if !plan.thin.is_empty() {
            let pack_id = sha256(&plan.thin);
            let file = self
                .repo
                .outgoing()
                .join(format!("v2-{}.pack", hex32(&pack_id)));
            std::fs::write(&file, &plan.thin).map_err(|e| SyncError::io(file.display(), e))?;
            files.push((file, pack_id));
            let pieces: Vec<&[u8]> = plan.thin.chunks(PIECE_SIZE).collect();
            let refs = match plan.thin.len() <= INLINE_MAX && inline_fits(plan) {
                true => {
                    inline.push(plan.thin.clone());
                    vec![PieceRef {
                        tx: ZERO,
                        sha256: sha256(&plan.thin),
                    }]
                }
                false => self.bulk_pieces(&pieces).await?,
            };
            packs.push(HeadPack {
                pack_id,
                pieces: refs,
            });
        }
        Ok(Content {
            snapshot: plan
                .prev
                .as_ref()
                .map(|p| p.head.body.snapshot.clone())
                .unwrap_or_default(),
            packs,
            inline,
            files,
        })
    }

    /// Bulk pieces from `B_d`, one tx each, before the head (crash-safe: they just expire).
    async fn bulk_pieces(&self, pieces: &[&[u8]]) -> Result<Vec<PieceRef>> {
        let rid = self.rid();
        let jobs = pieces
            .iter()
            .map(|p| Ok(vec![piece_op(&rid, p)?]))
            .collect::<Result<Vec<_>>>()?;
        let landed = self.send_from(&self.dev.bulk(&rid), jobs).await?;
        let mut tx = vec![ZERO; pieces.len()];
        for l in landed {
            tx[l.job] = l.receipt.tx_hash.0;
        }
        Ok(pieces
            .iter()
            .zip(tx)
            .map(|(p, tx)| PieceRef {
                tx,
                sha256: sha256(p),
            })
            .collect())
    }

    /// Full packs of the identity's refs after this push, as manifest plus chunks from `B_d`.
    async fn snapshot(&self, plan: &Plan) -> Result<Content> {
        let tips: Vec<String> = plan.after.values().map(hex::encode).collect();
        let files = match tips.is_empty() {
            true => vec![],
            false => self
                .repo
                .git()
                .full_packs_of(&tips, &self.repo.outgoing().join("snap"))?,
        };
        let mut hashes = vec![];
        let mut out_files = vec![];
        for f in files {
            let (pack_id, size, trailer, chunk_hashes) = packs::describe(&f)?;
            let m = ManifestBody {
                rid: self.rid(),
                pack_id,
                trailer,
                size,
                chunk_size: charter_core::CHUNK_SIZE as u64,
                chunk_hashes,
                requires: vec![],
                pack_kind: 1,
            };
            let signed = sign(self.dev.sk(), &m)?;
            self.upload_snapshot_pack(&f, &m, &signed.bytes).await?;
            hashes.push(signed.h);
            out_files.push((f, pack_id));
        }
        Ok(Content {
            snapshot: hashes,
            packs: vec![],
            inline: vec![],
            files: out_files,
        })
    }

    async fn upload_snapshot_pack(
        &self,
        f: &std::path::Path,
        m: &ManifestBody,
        manifest: &[u8],
    ) -> Result<()> {
        let rid = self.rid();
        let bulk = BTreeSet::from([self.dev.bulk_address(&rid).into_array()]);
        let mut sent_manifest = false;
        for _ in 0..=RESEND_ROUNDS {
            let have = packs::landed(self.a, &rid, &m.pack_id, &bulk).await?;
            let missing: Vec<u64> = (0..chunk_count(m.size))
                .filter(|i| !have.contains_key(i))
                .collect();
            if missing.is_empty() && sent_manifest {
                return Ok(());
            }
            let mut file = std::fs::File::open(f).map_err(|e| SyncError::io(f.display(), e))?;
            let mut jobs = vec![];
            for batch in missing.chunks(self.cfg.chunk_batch.max(1)) {
                jobs.clear();
                for &i in batch {
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
                if let Err(e) = self.send_from(&self.dev.bulk(&rid), jobs.clone()).await {
                    warn(&format!("snapshot upload: {e}; checking what landed"));
                }
            }
            if !sent_manifest {
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

    fn head_op(&self, body: &HeadBody, prev_key: Option<Hash32>) -> Result<(Operation, Hash32)> {
        let s = sign(self.dev.sk(), body)?;
        let env = charter_core::Envelope::decode(&s.bytes)?;
        let attrs = entity_attributes(&env, &s.bytes, body);
        let op = match prev_key {
            None => create_op_flags(
                &wire(&attrs),
                self.cfg.control_lifetime,
                random_salt(),
                HEAD_FLAGS,
            )?,
            Some(k) => patch_op(k.into(), &wire(&attrs))?,
        };
        Ok((op, s.h))
    }

    /// Sends the head tx, and a follow-up if the body landed past `at + LANDING_WINDOW`.
    async fn send_heads(
        &self,
        plan: &Plan,
        content: &Content,
        state: &mut State,
    ) -> Result<Vec<(Hash32, LandingJson)>> {
        let rid = self.rid();
        let (mut seq, mut pieces, mut prev_tx, mut key) = match &plan.prev {
            Some(p) => (
                p.head.body.seq + 1,
                p.head.body.pieces_created,
                p.landing.tx,
                Some(p.head.key),
            ),
            None => (0, 0, ZERO, None),
        };
        let mut refs = plan.refs.clone();
        let mut packs = content.packs.clone();
        let mut inline = content.inline.clone();
        let mut out = vec![];
        for _ in 0..3 {
            if inline.is_empty() {
                inline.push(vec![]);
            }
            let c_d = self.dev.control_address(&rid);
            // The confirmed tx count first: a tx of `C_d` landing after it either moves
            // `entityNonce` (caught below) or takes this nonce (the send fails).
            let account = self.a.tx_count_mined(c_d).await?;
            let (at, nonce) =
                futures_util::future::try_join(self.a.head(), self.a.entity_nonce(c_d)).await?;
            // Another writer of this device (a copied device seed) would fork the body
            // chain; the next head must account for exactly what `C_d` created so far.
            let expected = match key {
                Some(_) => pieces + 1,
                None => 0,
            };
            let sent = state.sent_head(&self.me())?;
            let own = plan.prev.as_ref().is_some_and(|p| Some(p.head.h) == sent);
            if nonce > expected && own && out.is_empty() && self.head_is(sent, nonce, state).await?
            {
                warn(
                    "repairing this device's head: it now accounts for every entity its control address created",
                );
                pieces = nonce - 1;
            } else if nonce != expected {
                return Err(SyncError::coded(
                    ErrorCode::ConcurrentPush,
                    format!(
                        "this device's control address created {nonce} entities, its head \
                         accounts for {expected}; another writer uses this device seed"
                    ),
                ));
            }
            pieces += inline.len() as u64;
            let body = HeadBody {
                rid,
                nid: self.nid(),
                seq,
                id_head: self.view.identity.head().h,
                at,
                pieces_created: pieces,
                refs: refs.clone(),
                snapshot: content.snapshot.clone(),
                packs: packs.clone(),
                prev_tx,
                object_hash: 1,
            };
            body.validate()?;
            let (head, h) = self.head_op(&body, key)?;
            let mut ops = inline
                .iter()
                .map(|p| piece_op(&rid, p))
                .collect::<Result<Vec<_>>>()?;
            ops.push(head);
            // Recorded before sending: a restart waits for this tx instead of forking the
            // body chain (head-model 7.2).
            let marker = self.pending_head_path();
            std::fs::write(&marker, hex32(&h)).map_err(|e| SyncError::io(marker.display(), e))?;
            state.set_sent_head(&self.me(), &h);
            state.save(&self.repo.state_path())?;
            let cfg = self.cfg.pipeline.ordered();
            let wallet = self.keys.wallet();
            let signer = self.dev.control(&rid);
            let landed =
                write::send_funded_at(self.a, &signer, &wallet, &cfg, account, vec![ops]).await?;
            let [l] = landed.as_slice() else {
                return Err(SyncError::coded(
                    ErrorCode::Internal,
                    "the head tx was split",
                ));
            };
            let index = self
                .a
                .mined_tx(l.receipt.tx_hash)
                .await?
                .map(|t| t.index)
                .unwrap_or(u32::MAX);
            let landing = LandingJson {
                tx: l.receipt.tx_hash.0,
                block: l.receipt.block_number,
                index,
            };
            state.landings.insert(hex32(&h), landing);
            state.save(&self.repo.state_path())?;
            out.push((h, landing));
            if landing.block <= at + LANDING_WINDOW {
                return Ok(out);
            }
            // Readers only scan `at + 1 ..= at + 64`: restate the body with concrete stamps.
            warn("the head tx landed late; sending a follow-up head");
            if key.is_none() {
                key = Some(self.created_head_key(&rid).await?);
            }
            refs = resolve_stamps(&refs, landing.block);
            packs = packs
                .into_iter()
                .map(|p| HeadPack {
                    pack_id: p.pack_id,
                    pieces: p
                        .pieces
                        .into_iter()
                        .map(|x| PieceRef {
                            tx: if x.tx == ZERO { landing.tx } else { x.tx },
                            sha256: x.sha256,
                        })
                        .collect(),
                })
                .collect();
            seq += 1;
            prev_tx = landing.tx;
            inline = vec![];
        }
        Err(SyncError::coded(
            ErrorCode::Rpc,
            "head txs keep landing late; push again",
        ))
    }

    /// Whether this device's head, read inside one `entityNonce` bracket at `nonce`, is
    /// still `sent`: a copied seed may have landed a consistent body since the plan.
    async fn head_is(&self, sent: Option<Hash32>, nonce: u64, state: &State) -> Result<bool> {
        let v2 = self.own_v2(state).await?;
        let dv = v2.as_ref().and_then(|v| v.device(&self.me()));
        Ok(dv.is_some_and(|d| {
            d.nonce.before == nonce
                && d.nonce.after == nonce
                && d.head.as_ref().map(|h| h.h) == sent
        }))
    }

    async fn created_head_key(&self, rid: &Hash32) -> Result<Hash32> {
        let state = State::load(&self.repo.state_path())?;
        let v2 = self.own_v2(&state).await?;
        v2.and_then(|v| {
            v.device(&self.me())
                .and_then(|d| d.head.as_ref().map(|h| h.key))
        })
        .ok_or_else(|| {
            SyncError::coded(
                ErrorCode::Internal,
                format!("head of {} not found", hex32(rid)),
            )
        })
    }

    /// After the receipt: every pushed ref must be held by this device's new entry.
    fn recheck(&self, v2: &V2) -> Result<()> {
        for (name, oid) in &self.planned {
            let w = v2.merged.get(name);
            let ok = match oid {
                Some(o) => {
                    w.is_some_and(|w| w.device == self.me() && hex::encode(&w.entry.oid) == *o)
                }
                None => w.is_none_or(|w| w.entry.is_tombstone()),
            };
            if !ok {
                return Err(SyncError::coded(
                    ErrorCode::LostRace,
                    format!(
                        "another device of this identity moved {name} concurrently; fetch and push again"
                    ),
                ));
            }
        }
        Ok(())
    }

    fn commit(
        &self,
        v2: &V2,
        content: &Content,
        landed: Vec<(Hash32, LandingJson)>,
        state: &mut State,
    ) -> Result<()> {
        for (file, pack_id) in &content.files {
            if !state.applied_packs.contains(&hex32(pack_id)) {
                self.repo.git().index_pack(file)?;
            }
            state.applied_packs.insert(hex32(pack_id));
            let _ = std::fs::remove_file(file);
        }
        state
            .applied_manifests
            .extend(content.snapshot.iter().map(hex32));
        let _ = landed;
        if let Some(mark) = v2
            .device(&self.me())
            .and_then(|d| d.head.as_ref())
            .map(|h| h.mark())
        {
            let mark = match state.device_mark(&self.me())? {
                Some(old) => old.advance(&mark)?,
                None => mark,
            };
            state.set_device_mark(&self.me(), &mark);
        }
        state.save(&self.repo.state_path())?;
        crate::fetch::set_namespace(self.repo, &self.nid(), &self.cached_refs(&v2.refs)?)?;
        let _ = std::fs::remove_file(self.pending_head_path());
        let staged = self.repo.git().refs(&staging(&self.nid()))?;
        let deletes: Vec<_> = staged
            .into_iter()
            .map(|(name, old)| crate::git::RefOp::Delete { name, old })
            .collect();
        self.repo.git().update_refs(&deletes)
    }
}

/// A plain thread sleep: the push future runs on the helper's runtime, and pending txs
/// resolve in seconds, so blocking briefly is fine and keeps tokio out of this crate.
async fn tokio_sleep(d: Duration) {
    std::thread::sleep(d);
}

/// Upper bound of the head body's bytes plus 16 KiB of inline pieces stays inside one tx
/// (head-model 4.2); otherwise the pack goes as bulk pieces.
fn inline_fits(plan: &Plan) -> bool {
    let refs: usize = plan.refs.iter().map(|e| e.name.len() + 2 * 32 + 32).sum();
    let pieces: usize = plan.carried.iter().map(|p| p.pieces.len() * 72 + 40).sum();
    let body = 512 + refs + pieces + charter_core::ENVELOPE_OVERHEAD + 3_373;
    // Attribute and ABI framing of the head and the piece ops, generously.
    body + body / 4 + INLINE_MAX + 8_192 <= charter_store::MAX_RAW_TX_BYTES
}
