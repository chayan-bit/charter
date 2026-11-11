//! The head tx (docs/p5/head-model.md 7.1 and 13.2): the create or patch op, the send with
//! its re-check after the receipt (`E_LOST_RACE`), and the local commit.

use std::collections::BTreeMap;

use charter_core::{
    Body, ErrorCode, Hash32, HeadBodyV3, HeadBodyV4, HeadRef, ManifestBody, RefTable, ZERO,
    entity_attributes, head_patch_attributes, resolve_stamps,
};
use charter_store::{Arkiv, Operation, create_op_flags, patch_op, random_salt};

use crate::head_push::{HeadPush, Plan, here_at, migrates};
use crate::heads::{LANDING_WINDOW, V2};
use crate::ids::hex32;
use crate::pieces::{marker_op, piece_op};
use crate::repo::staging;
use crate::state::{LandingJson, State};
use crate::write::{self, entity_op, sign, wire};
use crate::{Result, SyncError, warn};

use super::{Content, Pre, head_gas};

/// Creation flags of a head: `permissionlessExtension`, not readonly, so `C_d` can patch it.
const HEAD_FLAGS: u8 = 2;
/// A head patch with less calldata than this is priced by [`super::head_gas`], not estimated.
const UNESTIMATED_HEAD_BYTES: usize = 32 * 1024;

impl<A: Arkiv> HeadPush<'_, A> {
    /// The head create, or a patch: with every attribute when `full` (the migrating patch,
    /// and every patch of a sealing backend, which seals only ops naming `ag.rid` and
    /// `ag.kind`), else only `ag.body` and `ag.h` (head-model 13.2).
    fn head_op(
        &self,
        body: &HeadBodyV3,
        prev_key: Option<Hash32>,
        full: bool,
    ) -> Result<(Operation, Hash32)> {
        // A body naming a ref table is signed and stored as v4 (head-model 14.2).
        let (s, attrs) = match body.refs_table.is_some() {
            true => head_attrs(self.dev.sk(), &HeadBodyV4(body.clone()), prev_key, full)?,
            false => head_attrs(self.dev.sk(), body, prev_key, full)?,
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
    pub(super) async fn send_heads(
        &self,
        plan: &Plan,
        content: &Content,
        (mut refs, mut table): (Vec<HeadRef>, Option<RefTable>),
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
        // A move between v3 and v4 changes `ag.v`: that patch sets every attribute (14.3).
        let was_v4 = plan
            .prev
            .as_ref()
            .is_some_and(|p| p.head.refs_table.is_some());
        let mut full = migrates(plan.prev.as_ref()) || self.a.seals() || was_v4 != table.is_some();
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
                refs_table: table.clone(),
            };
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
            table = table.map(|t| t.resolved(landing.block));
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
    pub(super) fn recheck(&self, v2: &V2) -> Result<()> {
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

    pub(super) fn commit(
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

/// `body` signed, and the attributes of its create or full patch, or of a minimal patch.
fn head_attrs<B: Body>(
    sk: &charter_crypto::SecretKey,
    body: &B,
    prev_key: Option<Hash32>,
    full: bool,
) -> Result<(crate::write::Signed, Vec<charter_core::Attribute>)> {
    body.validate()?;
    let s = sign(sk, body)?;
    let attrs = match (prev_key, full) {
        (Some(_), false) => head_patch_attributes(B::KIND, &s.bytes),
        _ => {
            let env = charter_core::Envelope::decode(&s.bytes)?;
            entity_attributes(&env, &s.bytes, body)
        }
    };
    Ok((s, attrs))
}
