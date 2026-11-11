//! The verified control-plane view of a repo (design 4.3 steps 1-4): identity chain, the
//! bind and anchor chains of every synced peer, and each peer's accepted head (refs and
//! manifests checked, no pack bytes yet).

use std::collections::{BTreeMap, BTreeSet};

use charter_core::{
    AcceptedHead, AnchorChain, CompletenessObs, Equivocation, HeadInputs, IdentityChain, Kind,
    ManifestBody, Nid, Observed, PeerInfo, RefsBody, Rid, Start, accept_head, anchor_chain,
    bind_chain, check_control_set, check_requires, identity_chain, is_device_row,
};
use charter_store::{Arkiv, Pred};
use futures_util::future::{join_all, try_join, try_join_all};

use crate::observe::{self, all, b32, by_hashes, by_seq, creators, kind};
use crate::state::State;
use crate::{Result, SyncError, warn};

/// One synced peer.
#[derive(Debug, Clone)]
pub struct PeerView {
    pub info: PeerInfo,
    pub chain: AnchorChain,
    pub head: Option<AcceptedHead>,
    pub completeness: Vec<CompletenessObs>,
    /// The anchor entities as observed (keepalive and status read their keys and expiry).
    pub anchors: Vec<Observed>,
    /// The head refs entity and the manifests the head check saw.
    pub refs: Vec<Observed>,
    pub manifests: Vec<Observed>,
    /// Format v2 (devices and heads); when set, `head` is `None` and the v1 chain is history.
    pub v2: Option<crate::heads::V2>,
}

impl PeerView {
    /// The peer's verified refs: the merged v2 heads once any landed, else the v1 head's.
    pub fn refs_body(&self) -> Option<&RefsBody> {
        match &self.v2 {
            Some(v) if v.heads().next().is_some() => Some(&v.refs),
            _ => self.head.as_ref().map(|h| &h.refs),
        }
    }
}

#[derive(Debug, Clone)]
pub struct View {
    pub rid: Rid,
    pub chain_id: u64,
    pub head_block: u64,
    pub identity: IdentityChain,
    /// Every resolvable signer of identity entities, plus synced peers.
    pub resolved: BTreeMap<Nid, PeerInfo>,
    pub peers: BTreeMap<Nid, PeerView>,
    pub equivocations: Vec<Equivocation>,
    /// The pinned repo program folded over the repo; set by fetch ([`crate::fetch::sync`]).
    pub program: Option<crate::program::Enforced>,
}

impl View {
    pub fn delegates(&self) -> &[Nid] {
        &self.identity.head().body.delegates
    }
}

/// Which peers to sync and which must verify (delegates always must).
#[derive(Debug, Clone, Default)]
pub struct Want {
    pub extra: BTreeSet<Nid>,
    pub must: BTreeSet<Nid>,
}

/// Peer key from its self-signed documents, then its bind chain for `rid`.
pub async fn resolve_peer<A: Arkiv>(a: &A, rid: &Rid, nid: &Nid) -> Result<Option<PeerInfo>> {
    let filter = Pred::And(vec![
        kind(Kind::Bind),
        b32("ag.rid", rid),
        b32("ag.nid", nid),
    ]);
    // Independent queries, sent together; the bind chain is only checked once the key is known.
    let (pk, binds) =
        try_join(crate::cob::peer_key_of(a, nid), by_seq(a, filter, "ag.seq")).await?;
    let Some(pk) = pk else {
        return Ok(None);
    };
    let binds = bind_chain(rid, &pk, &binds)?;
    Ok(Some(PeerInfo { pk, binds }))
}

fn signer_nids(observed: &[Observed]) -> BTreeSet<Nid> {
    observed
        .iter()
        .filter_map(|o| match o.attr("ag.nid") {
            Some(charter_core::AttrValue::Bytes32(n)) => Some(*n),
            _ => None,
        })
        .collect()
}

/// The `id` and `idacc` entities of `rid`.
pub fn identity_filter(rid: &Rid) -> Pred {
    let kinds = Pred::Or(vec![kind(Kind::Id), kind(Kind::IdAcc)]);
    Pred::And(vec![b32("ag.rid", rid), kinds])
}

/// Identity rows another step of this command just read (the private-repo check).
static PREFETCHED: std::sync::Mutex<Option<(Rid, std::time::Instant, Vec<Observed>)>> =
    std::sync::Mutex::new(None);
/// How long prefetched identity rows stand in for a fresh read.
const PREFETCH_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// Hands the rows of [`identity_filter`] to the next [`identity`] of `rid`, which uses them
/// once (within [`PREFETCH_TTL`]) instead of querying again.
pub fn prefetched_identity(rid: &Rid, rows: Vec<Observed>) {
    if let Ok(mut slot) = PREFETCHED.lock() {
        *slot = Some((*rid, std::time::Instant::now(), rows));
    }
}

fn take_prefetched(rid: &Rid) -> Option<Vec<Observed>> {
    let mut slot = PREFETCHED.lock().ok()?;
    match slot.take() {
        Some((r, at, rows)) if r == *rid && at.elapsed() < PREFETCH_TTL => Some(rows),
        _ => None,
    }
}

/// The identity chain of `rid` and every signer the identity entities name.
pub async fn identity<A: Arkiv>(
    a: &A,
    rid: &Rid,
    state: &State,
) -> Result<(IdentityChain, BTreeMap<Nid, PeerInfo>)> {
    let observed = match take_prefetched(rid) {
        Some(rows) => rows,
        None => all(a, identity_filter(rid)).await?,
    };
    let nids: Vec<Nid> = signer_nids(&observed).into_iter().collect();
    let infos = try_join_all(nids.iter().map(|n| resolve_peer(a, rid, n))).await?;
    let peers: BTreeMap<Nid, PeerInfo> = nids
        .into_iter()
        .zip(infos)
        .filter_map(|(n, p)| p.map(|p| (n, p)))
        .collect();
    let chain = identity_chain(rid, &peers, &observed, state.id_mark()?.as_ref())?;
    Ok((chain, peers))
}

/// Anchor entities of `nid` created by its control addresses, in seq order.
pub async fn anchors<A: Arkiv>(
    a: &A,
    rid: &Rid,
    nid: &Nid,
    info: &PeerInfo,
) -> Result<Vec<Observed>> {
    let control = info.binds.control_set();
    if control.is_empty() {
        return Ok(vec![]);
    }
    let filter = Pred::And(vec![
        kind(Kind::Anchor),
        b32("ag.rid", rid),
        b32("ag.nid", nid),
        creators(&control),
    ]);
    by_seq(a, filter, "ag.seq").await
}

/// The anchor chain of one peer from genesis (the high-water mark is checked on accept).
pub async fn chain_of<A: Arkiv>(
    a: &A,
    rid: &Rid,
    nid: &Nid,
    info: &PeerInfo,
    identity: &IdentityChain,
) -> Result<(AnchorChain, Vec<Observed>)> {
    let observed = anchors(a, rid, nid, info).await?;
    let v1: Vec<Observed> = observed
        .iter()
        .filter(|o| !is_device_row(o))
        .cloned()
        .collect();
    let chain = anchor_chain(rid, nid, info, identity, &v1, Start::Genesis)?;
    Ok((chain, observed))
}

/// Bind chain, completeness, anchor chain and head acceptance for one peer (design 3.6).
pub async fn verify_peer<A: Arkiv>(
    a: &A,
    rid: &Rid,
    nid: &Nid,
    info: PeerInfo,
    identity: &IdentityChain,
    state: &State,
) -> Result<PeerView> {
    // Each completeness observation stays sequential inside (nonce, count, nonce); the
    // observations and the anchor query are independent, so they run together.
    let control: Vec<_> = info.binds.control_set().into_iter().collect();
    let (completeness, (chain, anchors)) = try_join(
        try_join_all(control.iter().map(|c| observe::completeness(a, c))),
        chain_of(a, rid, nid, &info, identity),
    )
    .await?;
    let device_rows: Vec<Observed> = anchors
        .iter()
        .filter(|o| is_device_row(o))
        .cloned()
        .collect();
    if !device_rows.is_empty() {
        check_control_set(nid, &info.binds.control_set(), &completeness)?;
        let v2 = Box::pin(crate::heads::build(
            a,
            rid,
            nid,
            &info,
            identity,
            &device_rows,
            state,
        ))
        .await?;
        // Before any device head landed (a half-done migration) the v1 chain still holds
        // the refs, so it is verified as before (head-model 5.1).
        if v2.heads().next().is_some() {
            return Ok(PeerView {
                info,
                chain,
                head: None,
                completeness,
                anchors,
                refs: vec![],
                manifests: vec![],
                v2: Some(v2),
            });
        }
        let mut pv = v1_head(a, rid, nid, info, chain, completeness, anchors, state).await?;
        pv.v2 = Some(v2);
        return Ok(pv);
    }
    v1_head(a, rid, nid, info, chain, completeness, anchors, state).await
}

/// Refs, manifests and head acceptance of a v1 anchor chain.
#[allow(clippy::too_many_arguments)]
async fn v1_head<A: Arkiv>(
    a: &A,
    rid: &Rid,
    nid: &Nid,
    info: PeerInfo,
    chain: AnchorChain,
    completeness: Vec<CompletenessObs>,
    anchors: Vec<Observed>,
    state: &State,
) -> Result<PeerView> {
    let applied = state.applied_manifest_set()?;
    let (refs, manifests) = match chain.head() {
        None => (vec![], vec![]),
        Some(h) => {
            let wanted: Vec<_> = chain
                .links
                .iter()
                .flat_map(|l| l.body.manifests.iter().copied())
                .filter(|m| !applied.contains(m))
                .collect();
            try_join(
                all(
                    a,
                    Pred::And(vec![kind(Kind::Refs), b32("ag.h", &h.body.refs)]),
                ),
                by_hashes(a, kind(Kind::Manifest), &wanted),
            )
            .await?
        }
    };
    let mark = state.peer_mark(nid)?;
    let inputs = HeadInputs {
        completeness: &completeness,
        mark: mark.as_ref(),
        refs: &refs,
        manifests: &manifests,
        applied: &applied,
    };
    let head = accept_head(rid, &info, &chain, &inputs)?;
    Ok(PeerView {
        info,
        chain,
        head,
        completeness,
        anchors,
        refs,
        manifests,
        v2: None,
    })
}

/// Builds the verified view: identity, then delegates, wanted peers and the transitive
/// closure of `requires`. A failing delegate or `must` peer aborts; others are skipped.
pub async fn build<A: Arkiv>(a: &A, rid: &Rid, state: &State, want: &Want) -> Result<View> {
    let chain_id = a.chain_id().await?;
    if let Some(expected) = state.chain_id.filter(|c| *c != chain_id) {
        return Err(charter_store::StoreError::WrongChain {
            expected,
            got: chain_id,
        }
        .into());
    }
    let (head_block, (identity, mut resolved)) = try_join(
        async { a.head().await.map_err(SyncError::from) },
        identity(a, rid, state),
    )
    .await?;
    let delegates: BTreeSet<Nid> = identity.head().body.delegates.iter().copied().collect();
    let must: BTreeSet<Nid> = delegates.union(&want.must).copied().collect();
    let mut queue: BTreeSet<Nid> = must.iter().chain(&want.extra).copied().collect();
    let mut peers = BTreeMap::new();
    // Rounds: every queued peer is resolved and verified concurrently; `requires` of the
    // verified ones form the next round. Outcomes are handled in nid order, as before.
    while !queue.is_empty() {
        let round: Vec<Nid> = std::mem::take(&mut queue)
            .into_iter()
            .filter(|n| !peers.contains_key(n))
            .collect();
        let infos = try_join_all(round.iter().map(|n| {
            let known = resolved.get(n).cloned();
            async move {
                match known {
                    Some(p) => Ok(Some(p)),
                    None => resolve_peer(a, rid, n).await,
                }
            }
        }))
        .await?;
        let found: Vec<(Nid, PeerInfo)> = round
            .into_iter()
            .zip(infos)
            .filter_map(|(n, p)| p.map(|p| (n, p)))
            .collect();
        resolved.extend(found.iter().map(|(n, p)| (*n, p.clone())));
        let verified = join_all(
            found
                .iter()
                .map(|(n, p)| verify_peer(a, rid, n, p.clone(), &identity, state)),
        )
        .await;
        for ((nid, _), r) in found.into_iter().zip(verified) {
            match r {
                Ok(pv) => {
                    queue.extend(
                        requires_of(&pv)
                            .into_iter()
                            .filter(|n| !peers.contains_key(n)),
                    );
                    peers.insert(nid, pv);
                }
                Err(e) if must.contains(&nid) => return Err(e),
                Err(e) => warn(&format!("skipping peer {}: {e}", crate::ids::to_text(&nid))),
            }
        }
    }
    check_all_requires(&peers)?;
    let equivocations = collect_equivocations(&peers);
    equivocations.iter().for_each(|e| {
        warn(&format!(
            "E_EQUIVOCATION peer {} seq {}: entities {}, {}",
            crate::ids::to_text(&e.nid),
            e.seq,
            hex::encode(e.keys[0]),
            hex::encode(e.keys[1])
        ))
    });
    Ok(View {
        rid: *rid,
        chain_id,
        head_block,
        identity,
        resolved,
        peers,
        equivocations,
        program: None,
    })
}

fn requires_of(pv: &PeerView) -> Vec<Nid> {
    pv.head
        .iter()
        .flat_map(|h| &h.manifests)
        .flat_map(|(_, m)| m.requires.iter().map(|r| r.nid))
        .collect()
}

fn check_all_requires(peers: &BTreeMap<Nid, PeerView>) -> Result<()> {
    let manifests: Vec<ManifestBody> = peers
        .values()
        .flat_map(|p| p.head.iter().flat_map(|h| h.manifests.iter()))
        .map(|(_, m)| m.clone())
        .collect();
    let chains: BTreeMap<Nid, AnchorChain> =
        peers.iter().map(|(n, p)| (*n, p.chain.clone())).collect();
    check_requires(&manifests, &chains).map_err(SyncError::from)
}

fn collect_equivocations(peers: &BTreeMap<Nid, PeerView>) -> Vec<Equivocation> {
    let mut v: Vec<Equivocation> = peers
        .values()
        .flat_map(|p| {
            p.chain
                .equivocations
                .iter()
                .chain(&p.info.binds.equivocations)
                .cloned()
        })
        .collect();
    v.sort();
    v.dedup();
    v
}
