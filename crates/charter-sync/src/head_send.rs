//! Head push, part 2 (docs/p5/head-model.md 7.1 and 13): the identity's first write and
//! device delegation, the snapshot or bulk pieces, the format v3 head tx, the re-check
//! after the receipt (`E_LOST_RACE`) and the local commit. A push returns once its bytes
//! are on chain.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use charter_core::codec::{self, Codec};
use charter_core::{
    Body, DeviceBody, ErrorCode, Hash32, HeadBodyV3, MAX_PACK_SIZE, ManifestBody, Pack, PackPieces,
    PieceRun, ZERO, chunk_count, entity_attributes, head_patch_attributes, resolve_stamps, runs,
};
use charter_store::{Arkiv, Operation, TxHash, U256, create_op_flags, patch_op, random_salt};
use futures_util::future::try_join_all;

use crate::head_push::{HeadPush, INLINE_MAX, Plan, here_at, migrates};
use crate::heads::{LANDING_WINDOW, V2};
use crate::ids::hex32;
use crate::pieces::{PIECE_SIZE, marker_op, piece_op};
use crate::push_send::{Cost, first_write_ops};
use crate::repo::{CONTROL_LIFETIME, staging};
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
/// A head patch with less calldata than this is priced by [`head_gas`], not estimated.
const UNESTIMATED_HEAD_BYTES: usize = 32 * 1024;
/// Snapshot manifests up to this many bytes in all are created by `C_d` in the head tx
/// (head-model 13.2): a block after their chunks, so GolemDB does not charge their index
/// terms as new twice, and with no marker; larger ones ride with the last chunks.
const MANIFESTS_IN_HEAD_MAX: usize = 32 * 1024;
/// Piece txs looked up by hash at once when a v2 head migrates.
const PARALLEL_LOOKUPS: usize = 8;

/// Calldata floor (EIP-7623, 10 per token, 4 tokens per non-zero byte).
const FLOOR_PER_BYTE: u64 = 40;
/// Rent of one storage unit for the longest life an entity can have left: 14 gas per
/// million unit-blocks (Arkiv `FeeSchedule`) times [`CONTROL_LIFETIME`] (Rabdi's maximum).
const RENT_PER_UNIT: u64 = (14 * CONTROL_LIFETIME).div_ceil(1_000_000);
/// Compute of each op after the first (measured on Rabdi).
const OP_GAS: u64 = 21_400;

/// Gas limit of a head tx of `calldata` bytes and `ops` ops sent without an estimate. A
/// patch pays rent on its net growth until the head expires, and a byte of calldata grows
/// the head by at most one storage unit (plus a few for an indexed attribute's cells,
/// which the slack covers); so the floor plus a full byte of rent per byte bounds the
/// worst case, a head that grows by its whole body. Never below [`HEAD_GAS`]. A limit
/// only reserves balance; the tx pays for the gas it uses.
fn head_gas(calldata: usize, ops: usize) -> u64 {
    const PER_BYTE: u64 = FLOOR_PER_BYTE + RENT_PER_UNIT + 9;
    let extra_ops = OP_GAS * ops.saturating_sub(1) as u64;
    HEAD_GAS.max(21_000 + extra_ops + PER_BYTE * calldata as u64)
}

/// What `setup` read on the way that the head send reuses instead of reading again.
#[derive(Debug, Clone, Copy)]
pub struct Ready {
    /// Nothing was sent or landed since the view's sync (the plan may reuse its heads).
    pub current: bool,
    /// `C_d`'s confirmed tx count once idle: the head tx's nonce. Read before
    /// `entityNonce`, so a tx of `C_d` landing after it either moves `entityNonce` or takes
    /// this nonce, and the send fails with `E_CONCURRENT_PUSH` (H1).
    account: u64,
    /// The head block, its base fee and when they were read.
    head: u64,
    base_fee: u128,
    at: std::time::Instant,
    /// `C_d`'s balance after any top-up.
    balance: U256,
}

/// Chain state the head send starts from: what `setup` read, and `entityNonce(C_d)` read
/// after it (alongside the plan). Without them the head send reads both fresh.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pre {
    pub ready: Option<Ready>,
    pub nonce: Option<u64>,
}

/// Pack bytes this push sends, in head form.
struct Content {
    snapshot: Vec<Hash32>,
    packs: Vec<Pack>,
    /// Pieces sent in the head tx.
    inline: Vec<Vec<u8>>,
    /// Signed snapshot manifests `C_d` creates in the head tx.
    manifests: Vec<Vec<u8>>,
    /// Local pack files (git packs, not their stored form) to index on commit, and their
    /// pack ids.
    files: Vec<(PathBuf, Hash32)>,
    /// Pieces or chunks went out before the head tx, so the head read in setup is stale.
    uploaded: bool,
}

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
    /// whether it had to wait (so the view's heads may be stale), and the confirmed count.
    async fn ensure_idle(&self) -> Result<(bool, u64)> {
        let c = self.dev.control_address(&self.rid());
        for round in 0..IDLE_POLLS {
            let (pending, mined) =
                futures_util::future::try_join(self.a.tx_count(c), self.a.tx_count_mined(c))
                    .await?;
            if pending == mined {
                return Ok((round > 0, mined));
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
            false => plan.stored.len() as u64,
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
        use charter_store::trace::span;
        let t = span(|| "push.setup".into());
        let ready = self.setup().await?;
        drop(t);
        let current = ready.current;
        let state = State::load(&self.repo.state_path())?;
        let t = span(|| format!("push.plan reuse_view={current}"));
        // `entityNonce` after the count `setup` read (H1), in the plan's round trips.
        let c_d = self.dev.control_address(&self.rid());
        let (plan, nonce) =
            futures_util::future::try_join(Box::pin(self.plan_from(&state, current)), async {
                Ok(self.a.entity_nonce(c_d).await?)
            })
            .await?;
        drop(t);
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
        let pre = Pre {
            ready: Some(ready),
            nonce: Some(nonce),
        };
        Box::pin(self.finish_with(plan, pre)).await
    }

    fn pending_head_path(&self) -> std::path::PathBuf {
        self.repo.outgoing().join("head-pending")
    }

    /// The first write and device delegation if missing, then waits for `C_d` to be idle
    /// (and so for a head tx an interrupted push left in flight) while topping it up.
    /// What it read is reused by the head send.
    pub async fn setup(&self) -> Result<Ready> {
        let first = self.first_ops()?;
        let sent_first = !first.is_empty();
        // One wallet run tops up both the identity's control address (for `first`) and
        // `C_d`, so a first push waits for one top-up block, not two; then `first` goes
        // while `C_d` is checked idle (the wallet sends nothing else meanwhile).
        let control = self.keys.control(&self.rid());
        let funded = async {
            let funds = self.prefund(&first).await?;
            if sent_first {
                self.send_from(&control, vec![first.clone()]).await?;
            }
            Ok(funds)
        };
        let ((waited, account), (head, base_fee, at, balance)) =
            futures_util::future::try_join(self.ensure_idle(), funded).await?;
        let _ = std::fs::remove_file(self.pending_head_path());
        Ok(Ready {
            current: !sent_first && !waited,
            account,
            head,
            base_fee,
            at,
            balance,
        })
    }

    /// Tops `C_d` up for several head txs at once when it cannot pay for one, so most
    /// pushes send no top-up before their head tx, and the identity's control address
    /// for `first` (the first write and device delegation) if it holds too little. Returns
    /// the head, base fee and time it read, and `C_d`'s balance after the top-up.
    async fn prefund(&self, first: &[Operation]) -> Result<(u64, u128, std::time::Instant, U256)> {
        let rid = self.rid();
        let (c_d, c) = (
            self.dev.control_address(&rid),
            self.keys.control_address(&rid),
        );
        let pc = &self.cfg.pipeline;
        // The control address's needs only for `first`, read in the same round trip.
        let control = async {
            match first.is_empty() {
                true => Ok(None),
                false => {
                    futures_util::future::try_join(self.a.estimate(c, first), self.a.balance(c))
                        .await
                        .map(Some)
                }
            }
        };
        let ((head, fee), have, control) =
            futures_util::future::try_join3(self.a.head_fee(), self.a.balance(c_d), control)
                .await?;
        let at = std::time::Instant::now();
        let one = pc.tx_reserve(HEAD_GAS, U256::ZERO, fee);
        let mut sends = vec![];
        if let Some(amount) = charter_store::PipelineConfig::top_up(have, one, PREFUND_HEADS) {
            sends.push((c_d, amount));
        }
        // What the pipeline sending `first` would top up itself, one block later.
        if let Some((gas, have_c)) = control
            && let Some(amount) = charter_store::PipelineConfig::top_up(
                have_c,
                pc.tx_reserve(gas, U256::ZERO, fee),
                1,
            )
        {
            sends.push((c, amount));
        }
        if sends.is_empty() {
            return Ok((head, fee, at, have));
        }
        let to_c_d = sends.iter().find(|(a, _)| *a == c_d).map(|(_, v)| *v);
        let chain = self.a.chain_id().await?;
        charter_store::TxPipeline::new(self.a, self.keys.wallet(), chain, pc.clone())
            .transfers(sends)
            .await?;
        Ok((head, fee, at, have + to_c_d.unwrap_or_default()))
    }

    /// Sends what `plan` says, re-checks and commits, reading the head tx's nonce inputs
    /// fresh.
    pub async fn finish(self, plan: Plan) -> Result<()> {
        self.finish_with(plan, Pre::default()).await
    }

    /// [`Self::finish`] from chain state already read (see [`Pre`]).
    pub async fn finish_with(self, plan: Plan, pre: Pre) -> Result<()> {
        use charter_store::trace::span;
        let mut state = State::load(&self.repo.state_path())?;
        // Boxed: these futures are large, and tests poll them on 2 MiB threads.
        let t = span(|| {
            format!(
                "push.content snapshot={} thin={} stored={}",
                plan.snapshot,
                plan.thin.len(),
                plan.stored.len()
            )
        });
        let content = Box::pin(self.content(&plan)).await?;
        drop(t);
        let t = span(|| {
            format!(
                "push.send_heads inline={}",
                content.inline.iter().map(Vec::len).sum::<usize>()
            )
        });
        let landed = Box::pin(self.send_heads(&plan, &content, pre, &mut state)).await?;
        drop(t);
        let t = span(|| "push.recheck".into());
        let v2 = Box::pin(self.own_v2(&state))
            .await?
            .ok_or_else(|| SyncError::coded(ErrorCode::Internal, "own heads vanished"))?;
        self.recheck(&v2)?;
        drop(t);
        let _t = span(|| "push.commit".into());
        self.commit(&v2, &content, landed, &mut state)
    }

    /// `refs` limited to what the cache holds: a ref another device moved to objects this
    /// cache has not fetched keeps its cached value until the next fetch brings them.
    /// `cached` is the namespace as the cache holds it.
    fn cached_refs(
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

    async fn content(&self, plan: &Plan) -> Result<Content> {
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
            let pieces = match plan.stored.len() <= INLINE_MAX && inline_fits(plan) {
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

    /// The head create, or a patch: with every attribute when `full` (the migrating patch,
    /// and every patch of a sealing backend, which seals only ops naming `ag.rid` and
    /// `ag.kind`), else only `ag.body` and `ag.h` (head-model 13.2).
    fn head_op(
        &self,
        body: &HeadBodyV3,
        prev_key: Option<Hash32>,
        full: bool,
    ) -> Result<(Operation, Hash32)> {
        let s = sign(self.dev.sk(), body)?;
        let attrs = match (prev_key, full) {
            (Some(_), false) => head_patch_attributes(&s.bytes),
            _ => {
                let env = charter_core::Envelope::decode(&s.bytes)?;
                entity_attributes(&env, &s.bytes, body)
            }
        };
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
        mut pre: Pre,
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
        let mut packs = content
            .packs
            .iter()
            .map(|p| {
                p.to_v3()
                    .ok_or_else(|| SyncError::coded(ErrorCode::Internal, "a v2 pack to send"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut inline = content.inline.clone();
        let mut manifests = content.manifests.clone();
        let mut full = migrates(plan.prev.as_ref()) || self.a.seals();
        let mut out = vec![];
        for _ in 0..3 {
            let c_d = self.dev.control_address(&rid);
            // Only the first head tx starts from what was read before; a follow-up reads.
            let (ready, nonce) = (pre.ready.take(), pre.nonce.take());
            // The confirmed tx count first: a tx of `C_d` landing after it either moves
            // `entityNonce` (caught below) or takes this nonce (the send fails).
            let account = match ready {
                Some(r) => r.account,
                None => self.a.tx_count_mined(c_d).await?,
            };
            let (at, nonce) = futures_util::future::try_join(
                async {
                    // `at` must be recent: readers scan only `at + 1 ..= at + 64` (5.4), and a
                    // long upload outlasts the head read in setup.
                    match ready {
                        Some(r) if !content.uploaded => Ok(r.head),
                        _ => self.a.head().await,
                    }
                },
                async {
                    match nonce {
                        Some(n) => Ok(n),
                        None => self.a.entity_nonce(c_d).await,
                    }
                },
            )
            .await?;
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
            // Every create of `C_d` in this tx counts (the rollback rule, head-model 6); the
            // marker only when nothing else raises `entityNonce(C_d)`.
            let marker = inline.is_empty() && manifests.is_empty();
            pieces += (inline.len() + manifests.len() + usize::from(marker)) as u64;
            let body = HeadBodyV3 {
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
            let (head, h) = self.head_op(&body, key, full)?;
            let mut ops = inline
                .iter()
                .map(|p| piece_op(&rid, p))
                .collect::<Result<Vec<_>>>()?;
            for m in &manifests {
                ops.push(entity_op::<ManifestBody>(m, self.cfg.data_lifetime)?);
            }
            if marker {
                ops.push(marker_op()?);
            }
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
            let calldata = charter_store::execute_calldata(&ops).len();
            let seed = write::Seed {
                tip: ready.map(|r| (r.head, r.base_fee, r.at, r.balance)),
                // Manifests are stored for the data lifetime: estimate rather than bound.
                gas: (key.is_some() && manifests.is_empty() && calldata < UNESTIMATED_HEAD_BYTES)
                    .then(|| head_gas(calldata, ops.len())),
            };
            let landed =
                write::send_funded_at(self.a, &signer, &wallet, &cfg, account, seed, vec![ops])
                    .await?;
            let [l] = landed.as_slice() else {
                return Err(SyncError::coded(
                    ErrorCode::Internal,
                    "the head tx was split",
                ));
            };
            let landing = LandingJson {
                tx: l.receipt.tx_hash.0,
                block: l.receipt.block_number,
                index: l.receipt.index,
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
            for p in &mut packs {
                p.runs = here_at(&p.runs, &landing);
            }
            seq += 1;
            prev_tx = landing.tx;
            inline = vec![];
            manifests = vec![];
            // The late tx already wrote every attribute if it had to.
            full = self.a.seals();
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
        // One listing of the namespace and the staged refs, one ref transaction for both.
        let (ns, staged) = (crate::repo::namespace(&self.nid()), staging(&self.nid()));
        let git = self.repo.git();
        let (current, staged): (BTreeMap<_, _>, BTreeMap<_, _>) = git
            .refs_under(&[&ns, &staged])?
            .into_iter()
            .partition(|(name, _)| name.starts_with(&ns));
        let refs = self.cached_refs(&v2.refs, &current)?;
        let mut ops = crate::fetch::namespace_ops(&ns, &current, &refs);
        ops.extend(
            staged
                .into_iter()
                .map(|(name, old)| crate::git::RefOp::Delete { name, old }),
        );
        git.update_refs(&ops)?;
        let _ = std::fs::remove_file(self.pending_head_path());
        crate::piece_reuse::clear(&self.repo.outgoing());
        Ok(())
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
    let pieces: usize = plan
        .carried
        .iter()
        .map(|p| match &p.pieces {
            PackPieces::Txs(t) => t.len() * 72 + 40,
            PackPieces::Runs(r) => r.len() * 20 + 40,
        })
        .sum();
    let body = 512 + refs + pieces + charter_core::ENVELOPE_OVERHEAD + 3_373;
    // Attribute and ABI framing of the head and the piece ops, generously.
    body + body / 4 + INLINE_MAX + 8_192 <= charter_store::MAX_RAW_TX_BYTES
}
