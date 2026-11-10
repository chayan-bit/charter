//! Program source distribution ([`publish`]) and the escrow hook ([`obligations`]).

use charter_core::{CREATE, CobOpBody, Hash32, Rid, ZERO};
use charter_store::Arkiv;
use serde_json::{Value, json};

use super::{PROGRAM_TYPE, PUBLISH};
use crate::keys::Keys;
use crate::repo::{Config, Repo};
use crate::view::Want;
use crate::{Result, cob, fetch};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Posts `{engine, source}` as a publish op on the repo's program object (created on first
/// use); returns the publish op id.
pub async fn publish<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
    engine: &str,
    source: &str,
) -> Result<Hash32> {
    let op = |object: Hash32, action: &str, payload: &Value| -> Result<CobOpBody> {
        Ok(CobOpBody {
            rid: *rid,
            type_name: PROGRAM_TYPE.into(),
            object,
            parents: if object == ZERO { vec![] } else { vec![object] },
            action: action.into(),
            payload: cob::json::to_cbor(payload)?,
            ts_ms: now_ms(),
        })
    };
    let existing = cob::list(a, rid, None, Some(PROGRAM_TYPE)).await?;
    let payload = json!({ "engine": engine, "source": source });
    if let Some(o) = existing.iter().find(|o| o.body.is_create()) {
        return cob::post(a, cfg, keys, &op(o.id, PUBLISH, &payload)?).await;
    }
    // A first publish: the create and the publish naming it go out together.
    let create = cob::sign_op(keys, &op(ZERO, CREATE, &json!({}))?)?;
    let publish = cob::sign_op(keys, &op(create.h, PUBLISH, &payload)?)?;
    let ids = cob::post_signed(a, cfg, keys, rid, &[create, publish]).await?;
    ids.last()
        .copied()
        .ok_or_else(|| crate::SyncError::coded(charter_core::ErrorCode::Internal, "no op posted"))
}

/// Obligations of the pinned program of `rid` (empty without a pin); the escrow hook. It
/// fetches into the cache under [`Config::default_home`], since fast-forwards need objects.
pub async fn obligations<A: Arkiv>(a: &A, rid: &Rid) -> Result<Vec<Value>> {
    obligations_in(a, &Repo::open(&Config::default_home(), rid)?).await
}

/// [`obligations`] with an explicit repo cache.
pub async fn obligations_in<A: Arkiv>(a: &A, repo: &Repo) -> Result<Vec<Value>> {
    let view = fetch::sync(a, repo, &Want::default()).await?;
    Ok(view
        .program
        .map(|e| e.outcome.obligations)
        .unwrap_or_default())
}
