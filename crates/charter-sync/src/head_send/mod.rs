//! Head push, part 2 (docs/p5/head-model.md 7.1 and 13): the identity's first write and
//! device delegation, the snapshot or bulk pieces, the format v3 head tx, the re-check
//! after the receipt (`E_LOST_RACE`) and the local commit. A push returns once its bytes
//! are on chain.

use std::path::PathBuf;
use std::time::Duration;

use charter_core::{DeviceBody, ErrorCode, Hash32, Pack};
use charter_store::{Arkiv, Operation, U256};

use crate::head_push::{HeadPush, INLINE_MAX, Plan};
use crate::pieces::{PIECE_SIZE, piece_op};
use crate::push_send::{Cost, first_write_ops};
use crate::repo::CONTROL_LIFETIME;
use crate::state::State;
use crate::write::{self, entity_op, sign};
use crate::{Result, SyncError};

mod content;
mod head_tx;

/// Waits for this device's earlier txs before signing (a resumed push after a crash).
const IDLE_POLLS: usize = 60;
/// Gas a head tx with a full 16 KiB inline piece stays under (MEASURED about 0.32M with
/// 2.7 KiB inline on Rabdi, 2026-11-11).
const HEAD_GAS: u64 = 1_200_000;
/// Head txs one top-up of `C_d` pays for.
const PREFUND_HEADS: u64 = 4;

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
        let choice = self.refs_choice(&plan);
        let mut content = Box::pin(self.content(&plan, choice.inline(&plan.refs))).await?;
        let table = Box::pin(self.upload_table(&choice)).await?;
        content.uploaded |= table.as_ref().is_some_and(|(_, new)| *new);
        drop(t);
        let t = span(|| {
            format!(
                "push.send_heads inline={}",
                content.inline.iter().map(Vec::len).sum::<usize>()
            )
        });
        let refs = choice.inline(&plan.refs).to_vec();
        let table = table.map(|(t, _)| t);
        let landed =
            Box::pin(self.send_heads(&plan, &content, (refs, table), pre, &mut state)).await?;
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
}

/// A plain thread sleep: the push future runs on the helper's runtime, and pending txs
/// resolve in seconds, so blocking briefly is fine and keeps tokio out of this crate.
async fn tokio_sleep(d: Duration) {
    std::thread::sleep(d);
}
