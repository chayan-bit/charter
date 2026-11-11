//! Format v2 ref events for the program fold (docs/p5/head-model.md 8): each delegate's
//! device bodies, walked back along `prev_tx` from calldata, merged per landing block with
//! the same rule as the head merge, so fold, merge and the push re-check agree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use std::sync::Arc;

use charter_core::{
    Body, Device, DeviceHead, Hash32, HeadBody, Merged, Nid, RefTable, Rid, ZERO, decode_table,
    encode_table, merge,
};
use charter_store::Arkiv;
use serde::{Deserialize, Serialize};

use crate::heads::{LANDING_WINDOW, V2, landing, raw_body_at};
use crate::ids::hex32;
use crate::state::{LandingJson, read_json, write_json};
use crate::{Result, SyncError};

/// One device body and where it landed.
#[derive(Debug, Clone)]
pub struct BodyAt {
    pub device: Nid,
    pub body: HeadBody,
    pub landing: LandingJson,
}

/// Verified bodies by landing tx, cached next to the git cache (calldata never changes).
/// A v4 body is kept with its field 7 only, its ref table once by `table_id` (head-model
/// 14.4), so a long walk of a large device stores each table once.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BodyCache {
    bodies: BTreeMap<String, (String, LandingJson)>,
    /// Landing of a current head, by head `h`.
    current: BTreeMap<String, LandingJson>,
    /// The ref table of each cached v4 body (CBOR hex), by landing tx.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    table_of: BTreeMap<String, String>,
    /// Decoded tables (canonical CBOR hex), by `table_id`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tables: BTreeMap<String, String>,
}

impl BodyCache {
    /// The cache file, its tables handed to the process's table cache.
    pub fn load(path: &Path) -> Self {
        let c: Self = read_json(path).ok().flatten().unwrap_or_default();
        for (id, t) in &c.tables {
            let id = hex::decode(id)
                .ok()
                .and_then(|b| <Hash32>::try_from(b).ok());
            let refs = hex::decode(t).ok().and_then(|b| decode_table(&b).ok());
            if let (Some(id), Some(refs)) = (id, refs) {
                crate::ref_table::remember(id, Arc::new(refs));
            }
        }
        c
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }

    /// Adds what another walk (on a clone of this cache) learned.
    pub fn absorb(&mut self, other: Self) {
        self.bodies.extend(other.bodies);
        self.current.extend(other.current);
        self.table_of.extend(other.table_of);
        self.tables.extend(other.tables);
    }

    fn get(&self, tx: &Hash32) -> Option<(HeadBody, Option<RefTable>, LandingJson)> {
        let (hex_body, l) = self.bodies.get(&hex32(tx))?;
        let body = HeadBody::decode(&hex::decode(hex_body).ok()?).ok()?;
        let table = match self.table_of.get(&hex32(tx)) {
            Some(t) => Some(minicbor::decode(&hex::decode(t).ok()?).ok()?),
            None => None,
        };
        Some((body, table, *l))
    }

    fn put(&mut self, body: &HeadBody, table: Option<&RefTable>, l: LandingJson) {
        self.bodies
            .insert(hex32(&l.tx), (hex::encode(body.encode()), l));
        let Some(t) = table else { return };
        let Ok(t_bytes) = minicbor::to_vec(t) else {
            return;
        };
        self.table_of.insert(hex32(&l.tx), hex::encode(t_bytes));
        if let Some(refs) = crate::ref_table::cached(&t.table_id) {
            self.tables
                .insert(hex32(&t.table_id), hex::encode(encode_table(&refs)));
        }
    }
}

/// `dev`'s bodies from the current one back to the first that landed before block `stop`
/// (the seed), or to seq 0. Each step must be the previous seq of the same device.
pub async fn walk<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    head: &DeviceHead,
    stop: u64,
    cache: &mut BodyCache,
) -> Result<Vec<BodyAt>> {
    let here = match cache.current.get(&hex32(&head.h)) {
        Some(l) => *l,
        None => landing(a, dev, head, LANDING_WINDOW)
            .await?
            .ok_or_else(|| broken(dev, "current body not found in calldata"))?,
    };
    cache.current.insert(hex32(&head.h), here);
    let mut out = vec![BodyAt {
        device: dev.nid,
        body: head.body.clone(),
        landing: here,
    }];
    loop {
        let last = &out[out.len() - 1];
        if last.landing.block < stop || last.body.prev_tx == ZERO {
            break;
        }
        let tx = last.body.prev_tx;
        let (raw, table, l) = match cache.get(&tx) {
            Some(x) => x,
            None => raw_body_at(a, rid, dev, &tx)
                .await?
                .ok_or_else(|| broken(dev, "prev_tx does not carry a body of this device"))?,
        };
        let body = match &table {
            Some(t) => crate::ref_table::effective(a, rid, dev, &raw, t).await?,
            None => raw.clone(),
        };
        if body.seq + 1 != last.body.seq
            || body.nid != last.body.nid
            || l.block >= last.landing.block
        {
            return Err(broken(dev, "prev_tx chain is not consecutive"));
        }
        cache.put(&raw, table.as_ref(), l);
        out.push(BodyAt {
            device: dev.nid,
            body,
            landing: l,
        });
    }
    out.reverse();
    Ok(out)
}

fn broken(dev: &Device, what: &str) -> SyncError {
    SyncError::coded(
        charter_core::ErrorCode::NotLive,
        format!("device {}: {what}", crate::ids::to_text(&dev.nid)),
    )
}

/// Every device's walked bodies of one identity; the devices are walked at the same time.
pub async fn identity_bodies<A: Arkiv>(
    a: &A,
    rid: &Rid,
    v2: &V2,
    stop: u64,
    cache: &mut BodyCache,
) -> Result<Vec<BodyAt>> {
    // A revoked device's cut body landed where its record says: no landing scan.
    for d in &v2.devices {
        if let (Some(h), Some(l)) = (&d.head, d.revoked.as_ref().and_then(|r| r.landing)) {
            cache.current.insert(hex32(&h.h), l);
        }
    }
    let walks = v2.heads().map(|(dev, head)| {
        let mut mine = cache.clone();
        async move {
            let bodies = walk(a, rid, dev, head, stop, &mut mine).await?;
            Ok::<_, SyncError>((bodies, mine))
        }
    });
    let mut all = vec![];
    for (bodies, theirs) in futures_util::future::try_join_all(walks).await? {
        cache.absorb(theirs);
        all.extend(bodies);
    }
    Ok(all)
}

/// The identity's merge after each landing block that carried any of `bodies`, in block
/// order. Bodies of one block are applied together, as the head merge sees them.
pub fn timeline(bodies: &[BodyAt]) -> Vec<(u64, Merged)> {
    let blocks: BTreeSet<u64> = bodies.iter().map(|b| b.landing.block).collect();
    let mut out = vec![];
    for block in blocks {
        let mut latest: BTreeMap<Nid, &BodyAt> = BTreeMap::new();
        for b in bodies.iter().filter(|b| b.landing.block <= block) {
            let keep = latest
                .get(&b.device)
                .is_none_or(|x| x.body.seq < b.body.seq);
            if keep {
                latest.insert(b.device, b);
            }
        }
        let heads: Vec<(Nid, &HeadBody)> = latest.iter().map(|(d, b)| (*d, &b.body)).collect();
        let landed: BTreeMap<Nid, u64> =
            latest.iter().map(|(d, b)| (*d, b.landing.block)).collect();
        // Every landing is known here, so the merge cannot ask for more.
        if let Ok(m) = merge(&heads, &landed) {
            out.push((block, m));
        }
    }
    out
}

/// Live refs of a merge and the signed `base` of each winning entry.
pub fn live_with_base(m: &Merged) -> BTreeMap<String, (Vec<u8>, Vec<u8>)> {
    m.iter()
        .filter(|(_, w)| !w.entry.is_tombstone())
        .map(|(n, w)| (n.clone(), (w.entry.oid.clone(), w.entry.base.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use charter_core::HeadRef;

    fn at(device: u8, block: u64, index: u32, seq: u64, oid: u8, counter: u64) -> BodyAt {
        BodyAt {
            device: [device; 32],
            body: HeadBody {
                rid: [0; 32],
                nid: [0; 32],
                seq,
                id_head: [0; 32],
                at: block - 1,
                pieces_created: seq + 1,
                refs: vec![HeadRef {
                    name: "refs/heads/main".into(),
                    oid: vec![oid; 20],
                    counter,
                    stamp: 0,
                    base: vec![],
                }],
                snapshot: vec![],
                packs: vec![],
                prev_tx: if seq == 0 { ZERO } else { [9; 32] },
                object_hash: 1,
            },
            landing: LandingJson {
                tx: [device; 32],
                block,
                index,
            },
        }
    }

    #[test]
    fn same_block_same_counter_folds_to_the_merge_winner() {
        // Device 9 lands first in the block (index 0), device 2 second: the merge rule
        // (block, then smaller device nid) picks device 2, and so must the fold.
        let bodies = vec![at(9, 50, 0, 0, 0xaa, 3), at(2, 50, 1, 0, 0xbb, 3)];
        let t = timeline(&bodies);
        assert_eq!(t.len(), 1, "one event per block");
        let heads: Vec<(Nid, &HeadBody)> = bodies.iter().map(|b| (b.device, &b.body)).collect();
        let landed = BTreeMap::from([([9; 32], 50), ([2; 32], 50)]);
        let m = merge(&heads, &landed).expect("merge");
        assert_eq!(
            t[0].1["refs/heads/main"].device,
            m["refs/heads/main"].device
        );
        assert_eq!(t[0].1["refs/heads/main"].device, [2; 32]);
    }

    #[test]
    fn an_earlier_block_wins_a_tie_and_a_later_counter_overrides() {
        let bodies = vec![
            at(2, 50, 0, 0, 0xaa, 3),
            at(1, 51, 0, 0, 0xbb, 3),
            at(1, 60, 0, 1, 0xcc, 4),
        ];
        let t = timeline(&bodies);
        let oid = |i: usize| live_with_base(&t[i].1)["refs/heads/main"].0[0];
        assert_eq!((oid(0), oid(1), oid(2)), (0xaa, 0xaa, 0xcc));
    }
}
