//! Keeping data alive (design 5) and `status`: the keep set is computed from verified state
//! only (so spam carrying the repo's `ag.rid` is never extended), then every entity expiring
//! within `renew_below` is extended to `horizon`, control plane first, in batches of at most
//! 100 ops and 15M predicted gas, through the paced pipeline.

use std::collections::BTreeSet;

use alloy_signer_local::PrivateKeySigner;
use charter_core::{CHUNK_SIZE, Hash32, Kind, Nid, Observed, Plane};
use charter_store::pipeline::{BatchLimits, batch};
use charter_store::{Arkiv, B256, U256, extend_op};

use crate::observe::{all, by_hashes, creators, kind};
use crate::repo::{BLOCKS_PER_DAY, Config, Repo};
use crate::state::State;
use crate::view::{self, View, Want};
use crate::{Result, packs, write};

/// Storage units of an entity beyond its payload (MemArkiv's calibration, design 2.4).
const SYSTEM_UNITS: u64 = 1_872;
const OP_GAS: u64 = 21_400;

/// One entity of the keep set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kept {
    pub key: Hash32,
    pub expires_at: u64,
    /// Payload bytes, for the gas prediction.
    pub size: u64,
    pub plane: Plane,
    pub peer: Nid,
}

fn kept(o: &Observed, plane: Plane, peer: Nid) -> Kept {
    let size = match o.attr("ag.body") {
        Some(charter_core::AttrValue::Bytes(b)) => b.len() as u64,
        _ => 0,
    };
    Kept {
        key: o.key,
        expires_at: o.expires_at,
        size,
        plane,
        peer,
    }
}

/// The keep set of design 5 step 2, from a verified view.
pub async fn keep_set<A: Arkiv>(a: &A, view: &View) -> Result<Vec<Kept>> {
    let mut out = vec![];
    for (nid, pv) in &view.peers {
        let control = pv.info.binds.control_set();
        let bulk = pv.info.binds.bulk_set();
        if !control.is_empty() {
            for o in all(a, creators(&control)).await? {
                out.push(kept(&o, Plane::Control, *nid));
            }
        }
        if let Some(v2) = &pv.v2 {
            out.extend(keep_v2(a, &view.rid, *nid, v2).await?);
        }
        if pv.head.is_none() {
            continue;
        }
        if let Some(r) = pv.refs.iter().find(|o| bulk.contains(&o.creator)) {
            out.push(kept(r, Plane::Bulk, *nid));
        }
        let named: Vec<Hash32> = pv
            .chain
            .links
            .iter()
            .flat_map(|l| l.body.manifests.clone())
            .collect();
        let found = by_hashes(a, kind(Kind::Manifest), &named).await?;
        let mut seen = BTreeSet::new();
        for m in found.iter().filter(|o| bulk.contains(&o.creator)) {
            let Ok(e) = charter_core::open::<charter_core::ManifestBody>(m) else {
                continue;
            };
            if !seen.insert(e.h) || e.envelope.signer != *nid {
                continue;
            }
            out.push(kept(m, Plane::Bulk, *nid));
            for c in packs::landed(a, &view.rid, &e.body.pack_id, &bulk)
                .await?
                .values()
            {
                out.push(Kept {
                    key: c.key,
                    expires_at: c.expires_at,
                    size: CHUNK_SIZE as u64,
                    plane: Plane::Bulk,
                    peer: *nid,
                });
            }
        }
    }
    for (nid, o) in crate::program::fold_v1_refs(a, view).await? {
        out.push(kept(&o, Plane::Bulk, nid));
    }
    out.sort_by_key(|k| (k.plane == Plane::Bulk, k.size >= CHUNK_SIZE as u64, k.key));
    out.dedup_by_key(|k| k.key);
    Ok(out)
}

/// Format v2 (head-model 4.4): every accepted head of the identity, and the snapshot
/// manifests and chunks each names. Pieces are never kept; their bytes live in calldata.
async fn keep_v2<A: Arkiv>(
    a: &A,
    rid: &Hash32,
    nid: Nid,
    v2: &crate::heads::V2,
) -> Result<Vec<Kept>> {
    let mut out = vec![];
    for d in &v2.devices {
        let Some(head) = &d.head else { continue };
        if let Some(o) = d.rows.iter().find(|o| o.key == head.key) {
            out.push(kept(o, Plane::Control, nid));
        }
        let bulk = BTreeSet::from([d.device.bulk]);
        let found = by_hashes(a, kind(Kind::Manifest), &head.body.snapshot).await?;
        for m in found.iter().filter(|o| o.creator == d.device.bulk) {
            let Ok(e) = charter_core::verify_entity::<charter_core::ManifestBody>(m, &d.device.pk)
            else {
                continue;
            };
            if !head.body.snapshot.contains(&e.h) {
                continue;
            }
            out.push(kept(m, Plane::Bulk, nid));
            for c in packs::landed(a, rid, &e.body.pack_id, &bulk)
                .await?
                .values()
            {
                out.push(Kept {
                    key: c.key,
                    expires_at: c.expires_at,
                    size: CHUNK_SIZE as u64,
                    plane: Plane::Bulk,
                    peer: nid,
                });
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeepOpts {
    /// Target remaining lifetime after extension, in blocks (default 90 days).
    pub horizon: u64,
    /// Extend entities with less than this many blocks left (default 30 days).
    pub renew_below: u64,
    pub max_spend: Option<U256>,
    pub dry_run: bool,
}

impl Default for KeepOpts {
    fn default() -> Self {
        Self {
            horizon: 90 * BLOCKS_PER_DAY,
            renew_below: 30 * BLOCKS_PER_DAY,
            max_spend: None,
            dry_run: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeepReport {
    pub kept: usize,
    pub extended: usize,
    pub txs: usize,
    pub gas: u64,
    pub spent: U256,
    /// Blocks until the earliest expiry in the keep set after this run.
    pub min_expiry_blocks: Option<u64>,
}

fn predicted_gas(k: &Kept, head: u64, horizon: u64) -> u64 {
    let added = (head + horizon).saturating_sub(k.expires_at);
    OP_GAS + (k.size + SYSTEM_UNITS) * added * 14 / 1_000_000
}

/// Extension jobs for the due part of `set`, cut off before `max_spend`; returns the jobs
/// and how many entities (a prefix of `due`) they extend.
fn plan(
    due: &[&Kept],
    head: u64,
    base_fee: u128,
    opts: &KeepOpts,
) -> (Vec<Vec<charter_store::Operation>>, usize) {
    let gases: Vec<u64> = due
        .iter()
        .map(|k| predicted_gas(k, head, opts.horizon))
        .collect();
    let items = due
        .iter()
        .zip(&gases)
        .map(|(k, g)| (extend_op(B256::from(k.key), opts.horizon), *g))
        .collect();
    let (mut at, mut spend, mut jobs) = (0, U256::ZERO, vec![]);
    for job in batch(items, BatchLimits::default()) {
        spend += write::price(gases[at..at + job.len()].iter().sum(), base_fee);
        if opts.max_spend.is_some_and(|m| spend > m) {
            break;
        }
        at += job.len();
        jobs.push(job);
    }
    (jobs, at)
}

/// The fetch sync for the control plane (no chunk bodies), then design 5 steps 2-6.
pub async fn keepalive<A: Arkiv>(
    a: &A,
    repo: &Repo,
    cfg: &Config,
    signer: &PrivateKeySigner,
    opts: &KeepOpts,
) -> Result<KeepReport> {
    let state = State::load(&repo.state_path())?;
    let want = Want {
        extra: cfg.extra_peers.iter().copied().collect(),
        ..Want::default()
    };
    let view = view::build(a, &repo.rid, &state, &want).await?;
    let set = keep_set(a, &view).await?;
    let head = view.head_block;
    let due: Vec<&Kept> = set
        .iter()
        .filter(|k| k.expires_at < head + opts.renew_below)
        .collect();
    let (jobs, extended) = plan(&due, head, a.base_fee().await?, opts);
    let mut report = KeepReport {
        kept: set.len(),
        extended,
        txs: jobs.len(),
        ..KeepReport::default()
    };
    if !opts.dry_run {
        for l in write::send(a, signer, &cfg.pipeline, jobs).await? {
            report.gas += l.receipt.gas_used;
            report.spent +=
                U256::from(l.receipt.gas_used) * U256::from(l.receipt.effective_gas_price);
        }
    }
    let renewed: BTreeSet<Hash32> = match opts.dry_run {
        true => BTreeSet::new(),
        false => due[..extended].iter().map(|k| k.key).collect(),
    };
    report.min_expiry_blocks = set
        .iter()
        .map(|k| {
            if renewed.contains(&k.key) {
                head + opts.horizon
            } else {
                k.expires_at
            }
        })
        .min()
        .map(|m| m.saturating_sub(head));
    Ok(report)
}

/// Per-peer line of `chart status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStatus {
    pub nid: Nid,
    pub delegate: bool,
    /// `(seq, block)` of the accepted head anchor.
    pub head: Option<(u64, u64)>,
    pub min_expiry_days: Option<u64>,
    pub expiring_14d: usize,
}

/// `chart status`: verified heads, expiry outlook, equivocation notes.
pub async fn status<A: Arkiv>(a: &A, repo: &Repo, cfg: &Config) -> Result<(View, Vec<PeerStatus>)> {
    let state = State::load(&repo.state_path())?;
    let want = Want {
        extra: cfg.extra_peers.iter().copied().collect(),
        ..Want::default()
    };
    let view = view::build(a, &repo.rid, &state, &want).await?;
    let set = keep_set(a, &view).await?;
    let head = view.head_block;
    let lines = view
        .peers
        .iter()
        .map(|(nid, pv)| {
            let mine: Vec<&Kept> = set.iter().filter(|k| k.peer == *nid).collect();
            PeerStatus {
                nid: *nid,
                delegate: view.delegates().contains(nid),
                // Format v2: the most recently updated device head.
                head: pv
                    .v2
                    .iter()
                    .flat_map(|v| v.heads())
                    .max_by_key(|(_, h)| h.updated_at)
                    .map(|(_, h)| (h.body.seq, h.updated_at))
                    .or_else(|| pv.chain.head().map(|l| (l.body.seq, l.position.block))),
                min_expiry_days: mine
                    .iter()
                    .map(|k| k.expires_at.saturating_sub(head) / BLOCKS_PER_DAY)
                    .min(),
                expiring_14d: mine
                    .iter()
                    .filter(|k| k.expires_at < head + 14 * BLOCKS_PER_DAY)
                    .count(),
            }
        })
        .collect();
    Ok((view, lines))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(i: u8) -> Kept {
        Kept {
            key: [i; 32],
            expires_at: 10,
            size: 100,
            plane: Plane::Bulk,
            peer: [0; 32],
        }
    }

    #[test]
    fn plans_batches_of_at_most_100_ops_and_stops_before_the_budget() {
        let set: Vec<Kept> = (0..=250u8).map(item).collect();
        let due: Vec<&Kept> = set.iter().collect();
        let opts = KeepOpts {
            horizon: 1_000,
            ..KeepOpts::default()
        };
        let (jobs, n) = plan(&due, 0, 1, &opts);
        assert_eq!(n, 251);
        assert!(jobs.iter().all(|j| j.len() <= 100) && jobs.len() == 3);
        let one = write::price(predicted_gas(&set[0], 0, 1_000) * 100, 1);
        let capped = KeepOpts {
            max_spend: Some(one),
            ..opts
        };
        let (jobs, n) = plan(&due, 0, 1, &capped);
        assert_eq!((jobs.len(), n), (1, 100));
        let (jobs, n) = plan(
            &due,
            0,
            1,
            &KeepOpts {
                max_spend: Some(U256::ZERO),
                ..capped
            },
        );
        assert!(jobs.is_empty() && n == 0);
    }
}
