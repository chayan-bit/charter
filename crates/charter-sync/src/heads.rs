//! The format v2 view of one identity (docs/p5/head-model.md sections 5 and 6): its device
//! delegations, each device's head under the `entityNonce` bracket, and the merge of the
//! heads into the identity's refs. Plus reading head bodies back from calldata.

use std::collections::BTreeMap;

use charter_core::{
    AttrValue, Device, DeviceHead, ErrorCode, Hash32, HeadBody, HeadNonce, IdentityChain, Kind,
    Merged, Nid, Observed, PeerInfo, RefTable, RefsBody, Rid, accept_device_head,
    accept_own_device_head, devices, merge, merged_refs, open_head_calldata, verify_entity,
};
use charter_store::{Arkiv, MinedTx, Pred, tx_rows};
use futures_util::future::try_join_all;

use crate::observe::{all, b32, creators, kind, to_observed};
use crate::state::{LandingJson, State};
use crate::{Result, SyncError};

/// Blocks after `at` a body may land in and still be found (head-model 5.4).
pub const LANDING_WINDOW: u64 = 64;
/// Retries of the full nonce bracket while `C_d` is mid-push.
const NONCE_RETRIES: usize = 4;

/// One device of an identity and its accepted head (none before its first push).
#[derive(Debug, Clone)]
pub struct DeviceView {
    pub device: Device,
    pub head: Option<DeviceHead>,
    pub nonce: HeadNonce,
    /// The head entity rows as observed (keepalive reads keys and expiry).
    pub rows: Vec<Observed>,
}

/// The v2 state of one identity.
#[derive(Debug, Clone)]
pub struct V2 {
    pub devices: Vec<DeviceView>,
    pub merged: Merged,
    /// The live refs of the merge.
    pub refs: RefsBody,
    /// Landing of each current head body this view resolved, by head `h`.
    pub landings: BTreeMap<Hash32, LandingJson>,
}

impl V2 {
    pub fn device(&self, nid: &Nid) -> Option<&DeviceView> {
        self.devices.iter().find(|d| d.device.nid == *nid)
    }

    pub fn heads(&self) -> impl Iterator<Item = (&Device, &DeviceHead)> {
        self.devices
            .iter()
            .filter_map(|d| d.head.as_ref().map(|h| (&d.device, h)))
    }
}

fn addr(a: &[u8; 20]) -> charter_store::Address {
    charter_store::Address::from(*a)
}

fn head_filter(rid: &Rid, dev: &Device) -> Pred {
    Pred::And(vec![
        kind(Kind::Head),
        b32("ag.rid", rid),
        b32("ag.dev", &dev.nid),
        creators(&[dev.control].into()),
    ])
}

/// Head rows of `dev`, read between two `entityNonce(C_d)` reads that agree.
async fn bracket<A: Arkiv>(a: &A, rid: &Rid, dev: &Device) -> Result<(Vec<Observed>, HeadNonce)> {
    let filter = head_filter(rid, dev);
    let mut last = None;
    for _ in 0..=NONCE_RETRIES {
        let before = a.entity_nonce(addr(&dev.control)).await?;
        let rows = all(a, filter.clone()).await?;
        let after = a.entity_nonce(addr(&dev.control)).await?;
        let nonce = HeadNonce {
            address: dev.control,
            before,
            after,
        };
        if before == after {
            return Ok((rows, nonce));
        }
        last = Some((rows, nonce));
    }
    last.ok_or_else(|| SyncError::coded(ErrorCode::InconsistentView, "no nonce observation"))
}

/// One device's head rows and the `entityNonce(C_d)` reads around them, already made.
#[derive(Debug, Clone)]
pub struct DeviceRead {
    pub device: Device,
    pub rows: Vec<Observed>,
    pub nonce: HeadNonce,
}

impl DeviceRead {
    /// The head rows of `dev`, then one `entityNonce(C_d)` read (head-model 6, short form).
    pub async fn quick<A: Arkiv>(a: &A, rid: &Rid, dev: &Device) -> Result<Self> {
        let rows = all(a, head_filter(rid, dev)).await?;
        let n = a.entity_nonce(addr(&dev.control)).await?;
        let nonce = HeadNonce {
            address: dev.control,
            before: n,
            after: n,
        };
        Ok(Self {
            device: dev.clone(),
            rows,
            nonce,
        })
    }

    /// The full `entityNonce` bracket around the head rows of `dev`.
    pub async fn bracketed<A: Arkiv>(a: &A, rid: &Rid, dev: &Device) -> Result<Self> {
        let (rows, nonce) = bracket(a, rid, dev).await?;
        Ok(Self {
            device: dev.clone(),
            rows,
            nonce,
        })
    }
}

fn accept(
    rid: &Rid,
    nid: &Nid,
    identity: &IdentityChain,
    state: &State,
    r: &DeviceRead,
) -> Result<Option<DeviceHead>> {
    let (dev, rows, nonce) = (&r.device, &r.rows, &r.nonce);
    let mark = state.device_mark(&dev.nid)?;
    let m = mark.as_ref();
    Ok(match state.sent_head(&dev.nid)? {
        Some(sent) => accept_own_device_head(rid, nid, dev, identity, rows, nonce, m, &sent)?,
        None => accept_device_head(rid, nid, dev, identity, rows, nonce, m)?,
    })
}

/// The head accounts for exactly what `C_d` created by the nonce read.
fn exact(head: Option<&DeviceHead>, nonce: &HeadNonce) -> bool {
    nonce.before == nonce.after
        && nonce.before == head.map_or(0, |h| h.body.pieces_created.saturating_add(1))
}

fn device_view(r: DeviceRead, head: Option<DeviceHead>) -> DeviceView {
    if !exact(head.as_ref(), &r.nonce) && head.is_some() {
        crate::warn(
            "this device's head accounts for fewer entities than its control address created \
             (a copy of this device seed pushed at the same time); the next push repairs it",
        );
    }
    DeviceView {
        device: r.device,
        head,
        nonce: r.nonce,
        rows: r.rows,
    }
}

/// The head of `dev`: rows then one nonce read, taken when the head accounts for exactly
/// that nonce; anything else (a push in between, a repair, a failure) reads again inside
/// the full bracket and decides there.
///
/// Why one nonce after the rows suffices (head-model 6): along a device's body chain
/// `pieces_created` strictly increases, and once a body lands `entityNonce(C_d)` is at least
/// its `pieces_created + 1` forever. A nonce read after the rows that equals the rows' head
/// `pieces_created + 1` is therefore below every newer body's, so no newer body had landed.
async fn read_device<A: Arkiv>(
    a: &A,
    rid: &Rid,
    nid: &Nid,
    dev: Device,
    identity: &IdentityChain,
    state: &State,
) -> Result<DeviceView> {
    let quick = DeviceRead::quick(a, rid, &dev).await?;
    if let Ok(head) = accept(rid, nid, identity, state, &quick)
        && exact(head.as_ref(), &quick.nonce)
    {
        let head = Box::pin(with_table(a, rid, &dev, head)).await?;
        return Ok(device_view(quick, head));
    }
    let full = DeviceRead::bracketed(a, rid, &dev).await?;
    let head = accept(rid, nid, identity, state, &full)?;
    let head = Box::pin(with_table(a, rid, &dev, head)).await?;
    Ok(device_view(full, head))
}

/// A v4 head with its ref table overlaid (head-model 14.4); other heads as they are.
async fn with_table<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    head: Option<DeviceHead>,
) -> Result<Option<DeviceHead>> {
    let Some(mut h) = head else {
        return Ok(None);
    };
    crate::ref_table::resolve(a, rid, dev, &mut h).await?;
    Ok(Some(h))
}

/// The v2 view of identity `nid` from its device delegation rows (anchor kind, `ag.v = 2`).
pub async fn build<A: Arkiv>(
    a: &A,
    rid: &Rid,
    nid: &Nid,
    info: &PeerInfo,
    identity: &IdentityChain,
    device_rows: &[Observed],
    state: &State,
) -> Result<V2> {
    let devs = devices(rid, info, device_rows);
    // Boxed: these futures are large, and tests poll them on 2 MiB threads.
    let views = try_join_all(
        devs.into_iter()
            .map(|d| Box::pin(read_device(a, rid, nid, d, identity, state))),
    )
    .await?;
    merged(a, rid, views, state).await
}

async fn merged<A: Arkiv>(a: &A, rid: &Rid, views: Vec<DeviceView>, state: &State) -> Result<V2> {
    let mut landings: BTreeMap<Hash32, LandingJson> = views
        .iter()
        .filter_map(|v| v.head.as_ref())
        .filter_map(|h| state.landing(&h.h).map(|l| (h.h, l)))
        .collect();
    let merged = loop {
        let blocks: BTreeMap<Nid, u64> = views
            .iter()
            .filter_map(|v| v.head.as_ref().map(|h| (v.device.nid, h)))
            .filter_map(|(d, h)| landings.get(&h.h).map(|l| (d, l.block)))
            .collect();
        let heads: Vec<(Nid, &HeadBody)> = views
            .iter()
            .filter_map(|v| v.head.as_ref().map(|h| (v.device.nid, &h.body)))
            .collect();
        match merge(&heads, &blocks) {
            Ok(m) => break m,
            Err(need) => {
                for v in views.iter().filter(|v| need.contains(&v.device.nid)) {
                    let Some(h) = &v.head else { continue };
                    // Not found within the window: never first-landed (deterministic for all).
                    let l =
                        landing(a, &v.device, h, LANDING_WINDOW)
                            .await?
                            .unwrap_or(LandingJson {
                                tx: [0; 32],
                                block: u64::MAX,
                                index: u32::MAX,
                            });
                    landings.insert(h.h, l);
                }
            }
        }
    };
    Ok(V2 {
        devices: views,
        refs: merged_refs(rid, &merged),
        merged,
        landings,
    })
}

/// Rows a mined tx created or patched, opened like query rows, at the tx's position.
pub fn calldata_rows<A: Arkiv>(a: &A, tx: &MinedTx) -> Vec<Observed> {
    let rows = a
        .open_rows(tx_rows(tx))
        .into_iter()
        .map(|mut r| {
            r.key.get_or_insert(charter_store::B256::ZERO);
            r.expires_at.get_or_insert(tx.block);
            r
        })
        .collect();
    to_observed(rows)
        .into_iter()
        .map(|o| Observed { tx: tx.index, ..o })
        .collect()
}

/// A row carrying the head body `h`: a create or full patch (`ag.kind = 'head'`), or a
/// format v3 minimal patch that sets only `ag.body` and `ag.h` (head-model 13.2).
fn is_head_row(o: &Observed, h: &Hash32) -> bool {
    let full = o.attr("ag.kind") == Some(&AttrValue::Str(Kind::Head.name().to_owned()));
    let minimal = o.attributes.len() == 2 && o.attr("ag.body").is_some();
    (full || minimal) && o.attr("ag.h") == Some(&AttrValue::Bytes32(*h))
}

fn landed(tx: &MinedTx) -> LandingJson {
    LandingJson {
        tx: tx.hash.0,
        block: tx.block,
        index: tx.index,
    }
}

/// The tx that carried `head`'s current body: from `C_d`, in blocks `at + 1` to
/// `min(updatedAt, at + LANDING_WINDOW)`.
pub async fn landing<A: Arkiv>(
    a: &A,
    dev: &Device,
    head: &DeviceHead,
    window: u64,
) -> Result<Option<LandingJson>> {
    let at = head.body.at;
    let last = head.updated_at.min(at.saturating_add(window));
    for b in at.saturating_add(1)..=last {
        for tx in a.block_txs(b).await? {
            // The patch must target this head (or be its create, in its creation block)
            // and the tx must have succeeded.
            let targets = |o: &Observed| {
                is_head_row(o, &head.h)
                    && (o.key == head.key || (o.key == [0; 32] && tx.block == head.created_at))
            };
            let ours = tx.from.0.0 == dev.control && calldata_rows(a, &tx).iter().any(targets);
            if ours && a.receipt(tx.hash).await?.is_some_and(|r| r.success) {
                return Ok(Some(landed(&tx)));
            }
        }
    }
    Ok(None)
}

/// A verified earlier body of `dev`'s head, read from the calldata of `tx`, with the ref
/// table of a v4 body (head-model 14) still to overlay: [`body_at`] does that.
pub async fn raw_body_at<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    tx: &Hash32,
) -> Result<Option<(HeadBody, Option<RefTable>, LandingJson)>> {
    let h = charter_store::TxHash::from(*tx);
    // The tx and its receipt together: one round trip per walked body.
    let (mined, receipt) = futures_util::future::try_join(a.mined_tx(h), a.receipt(h)).await?;
    let Some(mined) = mined else {
        return Ok(None);
    };
    let ok = receipt.is_some_and(|r| r.success && r.tx_hash == mined.hash);
    if mined.from.0.0 != dev.control || !ok {
        return Ok(None);
    }
    let body = calldata_rows(a, &mined)
        .into_iter()
        .find_map(|o| head_body(&o, dev).filter(|(b, _)| b.rid == *rid));
    Ok(body.map(|(b, t)| (b, t, landed(&mined))))
}

/// A verified earlier body of `dev`'s head, read from the calldata of `tx`, with effective
/// refs.
pub async fn body_at<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    tx: &Hash32,
) -> Result<Option<(HeadBody, LandingJson)>> {
    let Some((body, table, l)) = raw_body_at(a, rid, dev, tx).await? else {
        return Ok(None);
    };
    let body = match table {
        Some(t) => crate::ref_table::effective(a, rid, dev, &body, &t).await?,
        None => body,
    };
    Ok(Some((body, l)))
}

/// The head body a calldata row carries, signed by `dev`: a v2 body, or a v3 or v4 body
/// (full row or minimal patch) as its shared fields and the v4 ref table, which is all the
/// program fold reads.
fn head_body(o: &Observed, dev: &Device) -> Option<(HeadBody, Option<RefTable>)> {
    if let Ok(e) = verify_entity::<HeadBody>(o, &dev.pk) {
        return Some((e.body, None));
    }
    let (envelope, body, _) = open_head_calldata(o).ok()?;
    (envelope.signer == dev.nid && envelope.verify(&dev.pk).is_ok())
        .then(|| (body.shared(), body.refs_table.clone()))
}
