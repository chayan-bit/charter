//! [`Sealed`]: an `Arkiv` adapter for one private repo. Writes are sealed in
//! `seal_ops` (the pipeline calls it before pricing and signing), queries are blinded,
//! and rows are opened before the sync layer sees them. Push, clone and every core rule
//! run unchanged on top of it.

use charter_store::{
    Address, Arkiv, Attrs, Operation, Query, QueryOpts, QueryPage, Receipt, SignedTx, StoreError,
    TxHash, TxInfo, U256,
};

use crate::entity::{
    EPOCH, HIDDEN, PACK, blind_pred, is_stopper, late_head, open_row, open_row_unchecked, seal_op,
};
use charter_store::EntityData;
use charter_sync::heads::{LANDING_WINDOW, calldata_rows};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::keyring::{Keyring, write_private};

/// Opens `rows` of `ring`'s repo as [`Sealed`] does: the migration stopper keeps its place
/// (paging counts rows) but loses every attribute, so core sees no entity in it.
pub fn open_all(
    ring: &Keyring,
    rows: Vec<charter_store::EntityData>,
) -> Vec<charter_store::EntityData> {
    rows.into_iter()
        .map(|r| match is_stopper(&r) {
            true => charter_store::EntityData {
                attributes: Some(vec![]),
                ..r
            },
            false => open_row(ring, r),
        })
        .collect()
}

/// Whether the body `head` (an opened head row) carries landed by block `bound`: its
/// signed `at` starts the search, and a body lands within [`LANDING_WINDOW`] blocks after
/// it (docs/p5/head-model.md 5.4), so only `at + 1 ..= min(at + window, bound)` is read;
/// a backdated `at` finds nothing there. Costs those blocks' reads, only for a head an
/// extend moved past its epoch's end.
async fn landed_by<A: Arkiv>(
    a: &A,
    head: &charter_store::EntityData,
    bound: u64,
) -> Result<bool, StoreError> {
    let attr = |n: &str| head.attributes.iter().flatten().find(|x| x.name == n);
    let body = attr("ag.body").and_then(|x| x.value.as_str()?.strip_prefix("0x").map(hex::decode));
    let h = attr("ag.h").and_then(|x| x.value.as_str()?.strip_prefix("0x").map(hex::decode));
    let (Some(Ok(body)), Some(Ok(h)), Some(key), Some(creator), Some(upd)) =
        (body, h, head.key, head.creator, head.updated_at)
    else {
        return Ok(false);
    };
    let Some(at) = head_at(&body) else {
        return Ok(false);
    };
    let (Ok(h), key) = (<[u8; 32]>::try_from(h), key.0) else {
        return Ok(false);
    };
    let last = bound.min(at.saturating_add(LANDING_WINDOW)).min(upd);
    for b in at.saturating_add(1)..=last {
        for tx in a.block_txs(b).await? {
            let carries = |o: &charter_core::Observed| {
                (o.key == key || o.key == [0; 32])
                    && o.attr("ag.h") == Some(&charter_core::AttrValue::Bytes32(h))
            };
            let ours = tx.from == creator && calldata_rows(a, &tx).iter().any(carries);
            if ours && a.receipt(tx.hash).await?.is_some_and(|r| r.success) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// The signed `at` of a head envelope, v4, v3 or v2.
fn head_at(envelope: &[u8]) -> Option<u64> {
    use charter_core::{Body, HeadBody, HeadBodyV3, HeadBodyV4, Kind};
    let e = charter_core::Envelope::decode(envelope).ok()?;
    match e.kind {
        Kind::HeadV4 => HeadBodyV4::decode(&e.body).ok().map(|b| b.0.at),
        Kind::HeadV3 => HeadBodyV3::decode(&e.body).ok().map(|b| b.at),
        Kind::Head => HeadBody::decode(&e.body).ok().map(|b| b.at),
        _ => None,
    }
}

pub struct Sealed<'a, A> {
    pub inner: &'a A,
    pub ring: Keyring,
    /// Where [`landed_by`] verdicts are kept ([`Sealed::caching`]).
    landed: Option<PathBuf>,
}

impl<'a, A: Arkiv> Sealed<'a, A> {
    pub fn new(inner: &'a A, ring: Keyring) -> Self {
        Self {
            inner,
            ring,
            landed: None,
        }
    }

    /// Keeps each extended head's landing verdict under `home`, so its block reads are paid
    /// once: the verdict is final, as the epoch it was sealed in has ended.
    pub fn caching(mut self, home: &Path) -> Self {
        let file = format!("{}.landed.json", hex::encode(self.ring.rid));
        self.landed = Some(home.join("private").join(file));
        self
    }

    /// Replaces each extended head in `rows` whose body landed in time by its opened row.
    async fn admit_late_heads(&self, rows: &mut [EntityData]) -> Result<(), StoreError> {
        let mut seen: Option<BTreeMap<String, bool>> = None;
        let mut changed = false;
        for row in rows.iter_mut() {
            let Some(bound) = late_head(&self.ring, row) else {
                continue;
            };
            let opened = open_row_unchecked(&self.ring, row.clone());
            let id = verdict_id(&opened, bound);
            let cache = seen.get_or_insert_with(|| read_verdicts(self.landed.as_deref()));
            let landed = match cache.get(&id) {
                Some(v) => *v,
                None => {
                    let v = landed_by(self.inner, &opened, bound).await?;
                    changed |= cache.insert(id, v).is_none();
                    v
                }
            };
            if landed {
                *row = opened;
            }
        }
        if let (true, Some(path), Some(cache)) = (changed, &self.landed, seen) {
            write_verdicts(path, &cache);
        }
        Ok(())
    }
}

/// `entity key / ag.h / bound`: what a [`landed_by`] verdict depends on.
fn verdict_id(head: &EntityData, bound: u64) -> String {
    let h = head
        .attributes
        .iter()
        .flatten()
        .find(|x| x.name == "ag.h")
        .and_then(|x| x.value.as_str())
        .unwrap_or_default();
    let key = head.key.map(|k| hex::encode(k.0)).unwrap_or_default();
    format!("{key}/{h}/{bound}")
}

/// Cached verdicts; a missing or unreadable file is an empty cache (it only saves reads).
fn read_verdicts(path: Option<&Path>) -> BTreeMap<String, bool> {
    path.and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Saves the verdicts (mode 600). A cache that cannot be written only costs the reads again
/// next time, so that is not an error for the read that computed them.
fn write_verdicts(path: &Path, cache: &BTreeMap<String, bool>) {
    if let Ok(bytes) = serde_json::to_vec(cache) {
        let _ = write_private(path, &bytes);
    }
}

impl<A: Arkiv> Arkiv for Sealed<'_, A> {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        self.inner.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.inner.head().await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.inner.base_fee().await
    }
    async fn head_fee(&self) -> Result<(u64, u128), StoreError> {
        self.inner.head_fee().await
    }
    fn heads_ws(&self) -> Option<charter_store::heads::WsConfig> {
        self.inner.heads_ws()
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> Result<QueryPage, StoreError> {
        let q = Query {
            filter: blind_pred(&self.ring, &q.filter),
            order: q.order.clone(),
        };
        let mut o = o.clone();
        // A named projection must still return what opening and admitting a row needs.
        let opens = match &mut o.select.attrs {
            Attrs::All => true,
            Attrs::Named(names)
                if names
                    .iter()
                    .any(|n| HIDDEN.contains(&n.as_str()) || n == PACK) =>
            {
                names.extend(["ag.kind", "ag.rid", EPOCH, "ag.nid", "ag.h"].map(String::from));
                true
            }
            _ => false,
        };
        if opens {
            o.select.created_at = true;
            o.select.updated_at = true;
            o.select.creation_flags = true;
        }
        let mut page = self.inner.query(&q, &o).await?;
        page.rows = self.open_rows(std::mem::take(&mut page.rows));
        self.admit_late_heads(&mut page.rows).await?;
        Ok(page)
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        self.inner.entity_nonce(c).await
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.inner.balance(a).await
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.inner.tx_count(a).await
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.inner.tx_count_mined(a).await
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.inner.transaction(h).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.inner.mined_tx(h).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<charter_store::MinedTx>, StoreError> {
        self.inner.block_txs(n).await
    }
    async fn tx_at(&self, n: u64, i: u32) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.inner.tx_at(n, i).await
    }
    fn seals(&self) -> bool {
        true
    }
    fn incomplete_reason(&self) -> Option<&'static str> {
        self.ring.removed.then_some(
            "you were removed from this private repo, so what its members wrote since is \
             sealed under keys this machine does not hold; an admin can add you again",
        )
    }
    fn seals_for(&self, rid: &[u8; 32]) -> bool {
        self.ring.rid == *rid
    }
    fn open_rows(&self, rows: Vec<charter_store::EntityData>) -> Vec<charter_store::EntityData> {
        open_all(&self.ring, rows)
    }
    /// Estimates the sealed ops, so push cost predictions include padding.
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        let sealed = self.seal_ops(ops.to_vec())?;
        self.inner.estimate(from, &sealed).await
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        self.inner.send(tx).await
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        self.inner.receipt(h).await
    }
    fn seal_ops(&self, ops: Vec<Operation>) -> Result<Vec<Operation>, StoreError> {
        ops.into_iter()
            .map(|op| seal_op(&self.ring, op).map_err(|e| StoreError::InvalidOp(e.to_string())))
            .collect()
    }
}
