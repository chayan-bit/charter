//! Push, part 2 (design 4.2 steps 6-16): price and fund, first-write `peer` + `bind`,
//! chunks with landing verification, manifests, refs, the anchor gate, the anchor, the
//! post-verify, and the local commit. Every step is idempotent against chain state (4.4).

use std::collections::{BTreeMap, BTreeSet};

use charter_core::{
    AnchorBody, BindBody, Body, CHUNK_SIZE, ErrorCode, Hash32, Kind, ManifestBody, PeerBody,
    PeerMark, RefsBody, ZERO, chunk_count, content_hash,
};
use charter_store::{Arkiv, Operation, Pred, U256};
use futures_util::future::{try_join, try_join_all};

use crate::fetch::set_namespace;
use crate::ids::{hex32, unhex32};
use crate::observe::{all, b32, creators, kind};
use crate::push::Push;
use crate::repo::staging;
use crate::state::{PendingPack, State, unhex};
use crate::write::{self, chunk_op, entity_op, sign};
use crate::{Result, SyncError, packs, view, warn};

/// Resend rounds after the first upload before `E_UPLOAD_INCOMPLETE` (design 4.2 step 9).
const RESEND_ROUNDS: usize = 3;

/// Outcome of the anchor gate (step 12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// The own head is still the expected prev: send the anchor.
    Clear,
    /// The pending anchor already landed (a resumed push): only commit remains.
    Anchored,
}

/// Predicted spend of a push, per sending address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub control_gas: u64,
    pub bulk_gas: u64,
    pub base_fee: u128,
    /// Predicted spend: all estimated gas at the current base fee plus the tip.
    pub spend: U256,
    /// Max-fee reserve of one window (up to `window` of this push's txs) of the largest
    /// predicted size.
    pub reserve: U256,
}

impl Cost {
    /// What the wallet and the repo addresses must hold before the push starts.
    pub fn required(&self) -> U256 {
        self.spend + self.reserve
    }
}

impl<A: Arkiv> Push<'_, A> {
    fn rid(&self) -> Hash32 {
        self.repo.rid
    }

    fn has_bind(&self) -> bool {
        self.view
            .resolved
            .get(&self.nid())
            .is_some_and(|p| !p.binds.links.is_empty())
    }

    /// `[peer, bind_0]` for this peer's first write to the repo (design 4.2 step 7).
    pub(crate) fn first_write_ops(&self) -> Result<Vec<Operation>> {
        first_write_ops(self.keys, &self.rid(), self.cfg)
    }

    fn manifests(&self) -> Result<Vec<Vec<u8>>> {
        self.pending.manifests.iter().map(|m| unhex(m)).collect()
    }

    fn refs_bytes(&self) -> Result<Vec<u8>> {
        unhex(&self.pending.refs)
    }

    /// Gas per address from `eth_estimateGas` on one op of each shape (step 6). The
    /// estimates are independent, so they are requested together.
    pub async fn cost(&self) -> Result<Cost> {
        let (control, bulk) = (
            self.keys.control_address(&self.rid()),
            self.keys.bulk_address(&self.rid()),
        );
        let mut control_ops = vec![entity_op::<AnchorBody>(
            &self.provisional_anchor(0)?.bytes,
            self.cfg.control_lifetime,
        )?];
        if !self.has_bind() {
            control_ops = [self.first_write_ops()?, control_ops].concat();
        }
        let refs_op = entity_op::<RefsBody>(&self.refs_bytes()?, self.cfg.data_lifetime)?;
        let manifest_ops = self
            .manifests()?
            .iter()
            .map(|m| entity_op::<ManifestBody>(m, self.cfg.data_lifetime))
            .collect::<Result<Vec<_>>>()?;
        // Count chunks by length: full chunks plus each pack's shorter tail, so a small
        // push is priced and reserved for its real sizes, not for full 64 KiB chunks.
        let mut lens: BTreeMap<usize, u64> = BTreeMap::new();
        for size in self.pack_sizes()? {
            let tail = (size % CHUNK_SIZE as u64) as usize;
            *lens.entry(CHUNK_SIZE).or_default() += chunk_count(size) - u64::from(tail > 0);
            *lens.entry(tail).or_default() += u64::from(tail > 0);
        }
        let lens: Vec<(usize, u64)> = lens.into_iter().filter(|(l, n)| *l > 0 && *n > 0).collect();
        let chunk_ops = lens
            .iter()
            .map(|(len, _)| {
                chunk_op(
                    &self.rid(),
                    &ZERO,
                    0,
                    &vec![0xa5; *len],
                    self.cfg.data_lifetime,
                )
            })
            .collect::<Result<Vec<_>>>()?;
        let est = |from, op: &Operation| {
            let op = op.clone();
            async move { write::estimate(self.a, from, &[op]).await }
        };
        let (control_gas_each, (refs_gas, (manifest_gas, (chunk_gas, base_fee)))) = try_join(
            try_join_all(control_ops.iter().map(|op| est(control, op))),
            try_join(
                est(bulk, &refs_op),
                try_join(
                    try_join_all(manifest_ops.iter().map(|op| est(bulk, op))),
                    try_join(
                        try_join_all(chunk_ops.iter().map(|op| est(bulk, op))),
                        async { self.a.base_fee().await.map_err(SyncError::from) },
                    ),
                ),
            ),
        )
        .await?;
        // The first write goes as one tx, so the control total bounds the largest tx.
        let control_gas: u64 = control_gas_each.iter().sum();
        let mut largest = control_gas.max(refs_gas);
        // Txs: the anchor, the first write if any, the refs; manifests and chunks below.
        let mut txs = 1 + u64::from(!self.has_bind()) + 1;
        let mut bulk_gas = refs_gas;
        for gas in manifest_gas {
            largest = largest.max(gas);
            bulk_gas += gas;
            txs += 1;
        }
        for ((_, chunks), gas) in lens.iter().zip(chunk_gas) {
            largest = largest.max(gas);
            bulk_gas += chunks * gas;
            txs += chunks;
        }
        let pc = &self.cfg.pipeline;
        Ok(Cost {
            control_gas,
            bulk_gas,
            base_fee,
            spend: U256::from(control_gas + bulk_gas) * U256::from(base_fee + pc.tip),
            reserve: pc.reserve(txs, largest, base_fee),
        })
    }

    fn pack_sizes(&self) -> Result<Vec<u64>> {
        self.pending
            .packs
            .iter()
            .map(|p| {
                let f = self.repo.outgoing().join(&p.file);
                std::fs::metadata(&f)
                    .map(|m| m.len())
                    .map_err(|e| SyncError::io(f.display(), e))
            })
            .collect()
    }

    /// Step 6: the one up-front check; the sends then top up per window from the wallet.
    async fn fund(&self) -> Result<()> {
        let c = self.cost().await?;
        let spenders = [
            self.keys.control_address(&self.rid()),
            self.keys.bulk_address(&self.rid()),
        ];
        write::check_funds(
            self.a,
            self.keys.wallet().address(),
            &spenders,
            c.required(),
        )
        .await
    }

    /// Sends `jobs` from `signer`, topped up from the wallet per window. The bulk address
    /// only sends independent data txs (chunks, manifests, refs), so it keeps up to two
    /// blocks of gas in flight ([`charter_store::PipelineConfig::bulk`]).
    async fn send_from(
        &self,
        signer: &alloy_signer_local::PrivateKeySigner,
        jobs: Vec<Vec<Operation>>,
    ) -> Result<()> {
        let cfg = match signer.address() == self.keys.bulk_address(&self.rid()) {
            true => self.cfg.pipeline.bulk(),
            false => self.cfg.pipeline.clone(),
        };
        write::send_funded(self.a, signer, &self.keys.wallet(), &cfg, jobs).await?;
        Ok(())
    }

    /// Steps 6-11: fund, first write, chunks, manifests, refs.
    pub async fn upload(&self) -> Result<()> {
        self.fund().await?;
        if !self.has_bind() {
            let ops = self.first_write_ops()?;
            self.send_from(&self.keys.control(&self.rid()), vec![ops])
                .await?;
        }
        self.send_small_push().await?;
        for pk in &self.pending.packs {
            self.upload_pack(pk).await?;
        }
        let jobs = self.missing_entity_jobs().await?;
        self.send_from(&self.keys.bulk(&self.rid()), jobs).await
    }

    /// When every missing chunk fits in one batch, sends the chunks, manifests and refs
    /// together so they share blocks instead of each waiting for the previous receipt.
    /// The per-pack landing check and the entity check that follow still verify every
    /// piece and resend what did not land, so this only changes how sends are grouped.
    async fn send_small_push(&self) -> Result<()> {
        let mut jobs = vec![];
        for pk in &self.pending.packs {
            let (path, pack_id, size) = self.pack_file(pk)?;
            let missing = self.missing_chunks(&pack_id, size).await?;
            if jobs.len() + missing.len() > self.cfg.chunk_batch.max(1) {
                return Ok(());
            }
            jobs.extend(self.chunk_jobs(&path, &pack_id, size, &missing)?);
        }
        jobs.extend(self.missing_entity_jobs().await?);
        match self.send_from(&self.keys.bulk(&self.rid()), jobs).await {
            Err(SyncError::Store(e)) if e.code() == "E_RPC" => {
                warn(&format!("grouped upload: {e}; checking what landed"));
                Ok(())
            }
            other => other,
        }
    }

    fn pack_file(&self, pk: &PendingPack) -> Result<(std::path::PathBuf, Hash32, u64)> {
        let pack_id = unhex32(&pk.pack_id)?;
        let path = self.repo.outgoing().join(&pk.file);
        let size = std::fs::metadata(&path)
            .map_err(|e| SyncError::io(path.display(), e))?
            .len();
        Ok((path, pack_id, size))
    }

    async fn missing_chunks(&self, pack_id: &Hash32, size: u64) -> Result<Vec<u64>> {
        let bulk = BTreeSet::from([self.keys.bulk_address(&self.rid()).into_array()]);
        let have = packs::landed(self.a, &self.rid(), pack_id, &bulk).await?;
        Ok((0..chunk_count(size))
            .filter(|i| !have.contains_key(i))
            .collect())
    }

    fn chunk_jobs(
        &self,
        path: &std::path::Path,
        pack_id: &Hash32,
        size: u64,
        idx: &[u64],
    ) -> Result<Vec<Vec<Operation>>> {
        let mut f = std::fs::File::open(path).map_err(|e| SyncError::io(path.display(), e))?;
        idx.iter()
            .map(|&i| {
                let bytes = packs::read_chunk(&mut f, i, size)
                    .map_err(|e| SyncError::io(path.display(), e))?;
                Ok(vec![chunk_op(
                    &self.rid(),
                    pack_id,
                    i,
                    &bytes,
                    self.cfg.data_lifetime,
                )?])
            })
            .collect()
    }

    async fn upload_pack(&self, pk: &PendingPack) -> Result<()> {
        let (path, pack_id, size) = self.pack_file(pk)?;
        for round in 0..=RESEND_ROUNDS + 1 {
            let missing = self.missing_chunks(&pack_id, size).await?;
            if missing.is_empty() {
                return Ok(());
            }
            if round > RESEND_ROUNDS {
                break;
            }
            match self.send_chunks(&path, &pack_id, size, &missing).await {
                Err(SyncError::Store(e)) if e.code() == "E_RPC" => {
                    warn(&format!("chunk upload round {round}: {e}"))
                }
                other => other?,
            }
        }
        Err(SyncError::coded(
            ErrorCode::UploadIncomplete,
            format!(
                "pack {}: chunks still missing after {RESEND_ROUNDS} resend rounds; run git push again",
                pk.pack_id
            ),
        ))
    }

    async fn send_chunks(
        &self,
        path: &std::path::Path,
        pack_id: &Hash32,
        size: u64,
        idx: &[u64],
    ) -> Result<()> {
        let signer = self.keys.bulk(&self.rid());
        let (mut done, started) = (0, std::time::Instant::now());
        for batch in idx.chunks(self.cfg.chunk_batch.max(1)) {
            let jobs = self.chunk_jobs(path, pack_id, size, batch)?;
            self.send_from(&signer, jobs).await?;
            done += batch.len();
            let mb = (done * CHUNK_SIZE) as f64 / 1e6;
            eprintln!(
                "chart: pack {}: {done}/{} chunks sent ({:.2} MB/s)",
                &hex32(pack_id)[..12],
                idx.len(),
                mb / started.elapsed().as_secs_f64().max(1e-3)
            );
        }
        Ok(())
    }

    /// Create ops for the manifests and refs not yet live from our bulk address; the
    /// existence checks are independent, so they are requested together.
    async fn missing_entity_jobs(&self) -> Result<Vec<Vec<Operation>>> {
        let mut wanted: Vec<(Kind, Vec<u8>)> = self
            .manifests()?
            .into_iter()
            .map(|m| (ManifestBody::KIND, m))
            .collect();
        wanted.push((RefsBody::KIND, self.refs_bytes()?));
        let bulk = BTreeSet::from([self.keys.bulk_address(&self.rid()).into_array()]);
        let live = try_join_all(wanted.iter().map(|(k, bytes)| {
            let filter = Pred::And(vec![
                kind(*k),
                b32("ag.h", &content_hash(*k, bytes)),
                creators(&bulk),
            ]);
            all(self.a, filter)
        }))
        .await?;
        let mut jobs = vec![];
        for ((k, bytes), found) in wanted.iter().zip(live) {
            if found.is_empty() {
                let op = match *k {
                    Kind::Manifest => entity_op::<ManifestBody>(bytes, self.cfg.data_lifetime)?,
                    _ => entity_op::<RefsBody>(bytes, self.cfg.data_lifetime)?,
                };
                jobs.push(vec![op]);
            }
        }
        Ok(jobs)
    }

    async fn own_head(&self) -> Result<Option<Hash32>> {
        let info = view::resolve_peer(self.a, &self.rid(), &self.nid())
            .await?
            .ok_or_else(|| SyncError::coded(ErrorCode::Internal, "own peer document not found"))?;
        let (chain, _) =
            view::chain_of(self.a, &self.rid(), &self.nid(), &info, &self.view.identity).await?;
        Ok(chain.head().map(|l| l.h))
    }

    fn anchor_h(&self) -> Result<Option<Hash32>> {
        self.pending
            .anchor
            .as_ref()
            .map(|b| Ok(content_hash(Kind::Anchor, &unhex(b)?)))
            .transpose()
    }

    /// Step 12: the own head must still be the expected prev.
    pub async fn gate(&self) -> Result<Gate> {
        let head = self.own_head().await?;
        if head.is_some() && head == self.anchor_h()? {
            return Ok(Gate::Anchored);
        }
        let expected = (self.pending.seq > 0)
            .then(|| unhex32(&self.pending.prev))
            .transpose()?;
        if head == expected {
            return Ok(Gate::Clear);
        }
        Err(SyncError::coded(
            ErrorCode::ConcurrentPush,
            "this peer's anchor head moved during the push (another push with the same key?); \
             fetch and push again",
        ))
    }

    fn provisional_anchor(&self, at: u64) -> Result<write::Signed> {
        let manifests = self
            .manifests()?
            .iter()
            .map(|m| content_hash(Kind::Manifest, m))
            .collect();
        let body = AnchorBody {
            rid: self.rid(),
            seq: self.pending.seq,
            prev: unhex32(&self.pending.prev)?,
            refs: content_hash(Kind::Refs, &self.refs_bytes()?),
            manifests,
            id_head: self.view.identity.head().h,
            at,
        };
        sign(self.keys.sk(), &body)
    }

    /// Step 13: build (once, recorded in `pending.json`) and send the anchor.
    pub async fn send_anchor(&mut self) -> Result<()> {
        let bytes = match &self.pending.anchor {
            Some(b) => unhex(b)?,
            None => {
                let s = self.provisional_anchor(self.a.head().await?)?;
                self.pending.anchor = Some(hex::encode(&s.bytes));
                self.pending.save(&self.repo.pending_path())?;
                s.bytes
            }
        };
        let op = entity_op::<AnchorBody>(&bytes, self.cfg.control_lifetime)?;
        self.send_from(&self.keys.control(&self.rid()), vec![vec![op]])
            .await
    }

    /// Step 14: our anchor must be the head (first-created wins at every seq).
    pub async fn verify(&self) -> Result<()> {
        if self.own_head().await? == self.anchor_h()? {
            return Ok(());
        }
        Err(SyncError::coded(
            ErrorCode::LostRace,
            "another anchor with this peer's key took the same seq first; fetch and push again",
        ))
    }

    /// Step 15: index our packs, advance the mark, move staging refs into the namespace.
    pub fn commit(self) -> Result<()> {
        let anchor = self
            .pending
            .anchor
            .as_ref()
            .map(|b| unhex(b))
            .transpose()?
            .ok_or_else(|| SyncError::coded(ErrorCode::Internal, "commit before anchor"))?;
        let mut state = State::load(&self.repo.state_path())?;
        for pk in &self.pending.packs {
            if !state.applied_packs.contains(&pk.pack_id) {
                self.repo
                    .git()
                    .index_pack(&self.repo.outgoing().join(&pk.file))?;
            }
        }
        let new = PeerMark {
            seq: self.pending.seq,
            hash: content_hash(Kind::Anchor, &anchor),
            block: self.view.head_block,
        };
        let mark = match state.peer_mark(&self.nid())? {
            Some(old) => old.advance(&new)?,
            None => new,
        };
        state.set_peer_mark(&self.nid(), &mark);
        state
            .applied_packs
            .extend(self.pending.packs.iter().map(|p| p.pack_id.clone()));
        for m in self.manifests()? {
            state
                .applied_manifests
                .insert(hex32(&content_hash(Kind::Manifest, &m)));
        }
        state.save(&self.repo.state_path())?;
        let refs = RefsBody::decode(&charter_core::Envelope::decode(&self.refs_bytes()?)?.body)?;
        set_namespace(self.repo, &self.nid(), &refs)?;
        let staged = self.repo.git().refs(&staging(&self.nid()))?;
        let deletes: Vec<_> = staged
            .into_iter()
            .map(|(name, old)| crate::git::RefOp::Delete { name, old })
            .collect();
        self.repo.git().update_refs(&deletes)?;
        self.discard(&self.pending);
        Ok(())
    }

    /// Steps 6-15 end to end.
    pub async fn run(mut self) -> Result<()> {
        self.upload().await?;
        if self.gate().await? == Gate::Clear {
            self.send_anchor().await?;
            self.verify().await?;
        }
        self.commit()
    }
}

/// `[peer, bind_0]` ops for `keys`' first write to `rid`.
pub fn first_write_ops(
    keys: &crate::keys::Keys,
    rid: &Hash32,
    cfg: &crate::repo::Config,
) -> Result<Vec<Operation>> {
    let pk = keys.pk();
    let peer = PeerBody {
        alg: pk.alg().id(),
        pk: pk.as_bytes().to_vec(),
        alias: cfg.alias.clone(),
    };
    let bind = BindBody {
        rid: *rid,
        seq: 0,
        prev: ZERO,
        control: keys.control_address(rid).into_array(),
        bulk: keys.bulk_address(rid).into_array(),
    };
    Ok(vec![
        entity_op::<PeerBody>(&sign(keys.sk(), &peer)?.bytes, cfg.control_lifetime)?,
        entity_op::<BindBody>(&sign(keys.sk(), &bind)?.bytes, cfg.control_lifetime)?,
    ])
}
