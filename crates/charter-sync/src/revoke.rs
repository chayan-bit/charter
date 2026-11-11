//! Device revocation (docs/p5/head-model.md section 15): reading a revoked device's frozen
//! body, listing an identity's devices, and the identity's revoke record.

use charter_core::{
    Body, Device, DeviceHead, ErrorCode, HeadNonce, IdentityChain, Nid, Revocation, RevokeBody,
    Rid, ZERO, accept_cut_head,
};
use charter_store::{Arkiv, TxHash};

use crate::heads::{DeviceView, V2, calldata_rows};
use crate::ids::to_text;
use crate::state::LandingJson;
use crate::write::{self, entity_op, sign};
use crate::{Config, Keys, Result, SyncError, View};

/// A revoked device: its record and where its cut body landed (none without a cut body).
#[derive(Debug, Clone)]
pub struct Revoked {
    pub record: Revocation,
    pub landing: Option<LandingJson>,
}

fn bad(what: impl Into<String>) -> SyncError {
    SyncError::coded(ErrorCode::BadRefs, what)
}

/// The view of revoked device `dev`: its head is the body `r` names, read from calldata;
/// the head entity and `entityNonce(C_d)` are not read (head-model 15.2).
pub async fn read_revoked<A: Arkiv>(
    a: &A,
    rid: &Rid,
    nid: &Nid,
    dev: Device,
    identity: &IdentityChain,
    r: Revocation,
) -> Result<DeviceView> {
    let nonce = HeadNonce {
        address: dev.control,
        before: 0,
        after: 0,
    };
    if r.h == ZERO {
        return Ok(DeviceView {
            device: dev,
            head: None,
            nonce,
            rows: vec![],
            revoked: Some(Revoked {
                record: r,
                landing: None,
            }),
        });
    }
    let h = TxHash::from(r.tx);
    let (mined, receipt) = futures_util::future::try_join(a.mined_tx(h), a.receipt(h)).await?;
    let mined = mined.ok_or_else(|| {
        SyncError::coded(
            ErrorCode::NotLive,
            format!(
                "the cut body of revoked device {} is not in block history",
                to_text(&dev.nid)
            ),
        )
    })?;
    let ok = receipt.is_some_and(|x| x.success && x.tx_hash == mined.hash);
    if mined.from.0.0 != dev.control || !ok {
        return Err(bad(
            "a revoke names a tx that is not a landed head tx of its device",
        ));
    }
    let mut head: DeviceHead = calldata_rows(a, &mined)
        .iter()
        .find_map(|o| accept_cut_head(rid, nid, &dev, identity, &r, o, mined.block).ok())
        .ok_or_else(|| bad("a revoke names a body its tx does not carry"))?;
    crate::ref_table::resolve(a, rid, &dev, &mut head).await?;
    let landing = LandingJson {
        tx: mined.hash.0,
        block: mined.block,
        index: mined.index,
    };
    Ok(DeviceView {
        device: dev,
        head: Some(head),
        nonce,
        rows: vec![],
        revoked: Some(Revoked {
            record: r,
            landing: Some(landing),
        }),
    })
}

/// One delegated device of an identity, as `chart device list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub nid: Nid,
    pub control: [u8; 20],
    pub bulk: [u8; 20],
    /// Seq of its accepted head (the cut body for a revoked device).
    pub seq: Option<u64>,
    /// Seq of the last valid body when revoked (`Some(None)`: revoked before any body).
    pub revoked: Option<Option<u64>>,
}

/// Every delegated device of `nid` in `view`.
pub fn list(view: &View, nid: &Nid) -> Vec<DeviceInfo> {
    let Some(v2) = view.peers.get(nid).and_then(|p| p.v2.as_ref()) else {
        return vec![];
    };
    v2.devices
        .iter()
        .map(|d| DeviceInfo {
            nid: d.device.nid,
            control: d.device.control,
            bulk: d.device.bulk,
            seq: d.head.as_ref().map(|h| h.body.seq),
            revoked: d
                .revoked
                .as_ref()
                .map(|r| (r.record.h != ZERO).then_some(r.record.seq)),
        })
        .collect()
}

/// The record that freezes `dv` at its current head, as the revoker sees it now.
async fn record<A: Arkiv>(a: &A, rid: &Rid, v2: &V2, dv: &DeviceView) -> Result<RevokeBody> {
    let Some(head) = &dv.head else {
        return Ok(RevokeBody {
            rid: *rid,
            device: dv.device.nid,
            seq: 0,
            h: ZERO,
            tx: ZERO,
        });
    };
    let landing = crate::pieces::head_landing(a, v2, &dv.device, head).await?;
    Ok(RevokeBody {
        rid: *rid,
        device: dv.device.nid,
        seq: head.body.seq,
        h: head.h,
        tx: landing.tx,
    })
}

/// Revokes `device` of this identity (head-model 15.4): signed by the identity key and sent
/// from its control address. `this_device` is the machine's own device, which is refused.
pub async fn revoke<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    view: &View,
    device: &Nid,
    this_device: &Nid,
) -> Result<RevokeBody> {
    crate::guard::require_sealed(a, &cfg.home, &view.rid)?;
    let usage = |m: String| SyncError::coded(ErrorCode::BadFormat, m);
    if device == this_device {
        return Err(usage(
            "this is this machine's device: revoke it from another machine, or remove \
             device/seed first so this machine gets a new device key"
                .into(),
        ));
    }
    let v2 = view
        .peers
        .get(&keys.nid())
        .and_then(|p| p.v2.as_ref())
        .ok_or_else(|| usage("this identity has no devices in this repo".into()))?;
    let dv = v2
        .device(device)
        .ok_or_else(|| usage(format!("no delegated device {}", to_text(device))))?;
    if dv.revoked.is_some() {
        return Err(usage(format!(
            "device {} is already revoked",
            to_text(device)
        )));
    }
    let body = record(a, &view.rid, v2, dv).await?;
    body.validate()?;
    let op = entity_op::<RevokeBody>(&sign(keys.sk(), &body)?.bytes, cfg.control_lifetime)?;
    let pipeline = cfg.pipeline.ordered();
    write::send_funded(
        a,
        &keys.control(&view.rid),
        &keys.wallet(),
        &pipeline,
        vec![vec![op]],
    )
    .await?;
    Ok(body)
}

/// What `chart device revoke` sent.
#[derive(Debug, Clone)]
pub struct RevokeOutcome {
    pub record: RevokeBody,
    /// The sync failed (a device that breaks the rollback check, head-model 15.3), so the
    /// cut is the last head of the device this machine verified.
    pub from_mark: bool,
}

/// Syncs `repo` and revokes `device` at its current head; when the sync fails closed on a
/// device (a stolen `C_d` creating outside head txs), at the head this machine last
/// verified for it, which is the only body it can vouch for.
pub async fn revoke_device<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    repo: &crate::Repo,
    device: &Nid,
    this_device: &Nid,
) -> Result<RevokeOutcome> {
    // Before the sync, so an unsealed backend never reaches the mark path either.
    crate::guard::require_sealed(a, &cfg.home, &repo.rid)?;
    let want = crate::Want {
        must: [keys.nid()].into(),
        ..crate::Want::default()
    };
    match Box::pin(crate::sync(a, repo, &want)).await {
        Ok(view) => {
            let record = revoke(a, cfg, keys, &view, device, this_device).await?;
            Ok(RevokeOutcome {
                record,
                from_mark: false,
            })
        }
        Err(e) if e.code() == ErrorCode::ControlNotLive && device != this_device => {
            let record = Box::pin(revoke_at_mark(a, cfg, keys, repo, device)).await?;
            Ok(RevokeOutcome {
                record,
                from_mark: true,
            })
        }
        Err(e) => Err(e),
    }
}

/// Revokes `device` at this machine's mark of it, found where the body landed: the
/// recorded landing, else the block of the mark (the head's last update when verified).
async fn revoke_at_mark<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    repo: &crate::Repo,
    device: &Nid,
) -> Result<RevokeBody> {
    let rid = repo.rid;
    let no_cut = || {
        SyncError::coded(
            ErrorCode::ControlNotLive,
            format!(
                "device {} fails the rollback check and this machine has no verified head of \
                 it to keep; revoke it from a machine that synced it before",
                to_text(device)
            ),
        )
    };
    let state = crate::state::State::load(&repo.state_path())?;
    let mark = state.device_mark(device)?.ok_or_else(no_cut)?;
    let info = crate::view::resolve_peer(a, &rid, &keys.nid())
        .await?
        .ok_or_else(no_cut)?;
    let rows = crate::view::anchors(a, &rid, &keys.nid(), &info).await?;
    if charter_core::revocations(&rid, &info, &rows).contains_key(device) {
        return Err(SyncError::coded(
            ErrorCode::BadFormat,
            format!("device {} is already revoked", to_text(device)),
        ));
    }
    let dev = charter_core::devices(&rid, &info, &rows)
        .into_iter()
        .find(|d| d.nid == *device)
        .ok_or_else(no_cut)?;
    let tx = match state.landing(&mark.hash) {
        Some(l) => l.tx,
        None => landing_in(a, &dev, &mark.hash, mark.block)
            .await?
            .ok_or_else(no_cut)?,
    };
    let body = RevokeBody {
        rid,
        device: *device,
        seq: mark.seq,
        h: mark.hash,
        tx,
    };
    body.validate()?;
    let op = entity_op::<RevokeBody>(&sign(keys.sk(), &body)?.bytes, cfg.control_lifetime)?;
    let pipeline = cfg.pipeline.ordered();
    write::send_funded(
        a,
        &keys.control(&rid),
        &keys.wallet(),
        &pipeline,
        vec![vec![op]],
    )
    .await?;
    Ok(body)
}

/// The successful tx of `dev`'s `C_d` in `block` that carries head body `h`.
async fn landing_in<A: Arkiv>(
    a: &A,
    dev: &Device,
    h: &charter_core::Hash32,
    block: u64,
) -> Result<Option<charter_core::Hash32>> {
    let carries = |o: &charter_core::Observed| {
        o.attr("ag.h") == Some(&charter_core::AttrValue::Bytes32(*h)) && o.attr("ag.body").is_some()
    };
    for tx in a.block_txs(block).await? {
        let ours = tx.from.0.0 == dev.control && calldata_rows(a, &tx).iter().any(carries);
        if ours && a.receipt(tx.hash).await?.is_some_and(|r| r.success) {
            return Ok(Some(tx.hash.0));
        }
    }
    Ok(None)
}
