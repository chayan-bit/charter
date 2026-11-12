//! The heads a round accepted, so a probe can check them by their writers alone when rows
//! that anyone can write fill the general probe's tail (docs/p6/ui.md 4.2, `pace`).

use std::collections::BTreeMap;

use charter_core::{Address, Hash32, Kind, Rid};
use charter_store::{Arkiv, Attrs, B256, MAX_LIMIT, Pred, Query, QueryOpts, Select, StoreError};
use charter_sync::View;
use charter_sync::observe::{b32, kind};

/// Most devices one head query names: two predicates each, under the node's 64.
pub const MAX_DEVICES: usize = 31;

/// Each live device's control address and nid, and every head row the round read for them,
/// by entity key, with its `$updatedAt`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Watch {
    devices: Vec<(Address, Hash32)>,
    rows: BTreeMap<B256, u64>,
}

impl Watch {
    /// The unrevoked v2 devices of every synced peer; `None` past [`MAX_DEVICES`].
    // shortcut: a repo with more devices keeps the general probe only, so rows that fill its
    // tail can hide a head patch there; split the query when such repos exist.
    pub fn of(view: &View) -> Option<Self> {
        let live = view
            .peers
            .values()
            .filter_map(|p| p.v2.as_ref())
            .flat_map(|v| v.devices.iter())
            .filter(|d| d.revoked.is_none());
        let mut w = Self::default();
        for d in live {
            w.devices.push((d.device.control, d.device.nid));
            w.rows
                .extend(d.rows.iter().map(|r| (B256::from(r.key), r.updated_at)));
        }
        (!w.devices.is_empty() && w.devices.len() <= MAX_DEVICES).then_some(w)
    }

    /// One query: true when a watched head row moved or expired. New rows are left to the
    /// general probe's count.
    pub async fn moved<A: Arkiv>(&self, a: &A, rid: &Rid) -> Result<bool, StoreError> {
        let mine = self.devices.iter().map(|(c, d)| {
            Pred::And(vec![
                Pred::eq(
                    charter_store::Field::Creator,
                    charter_store::Address::from(*c),
                ),
                b32("ag.dev", d),
            ])
        });
        let q = Query::from(Pred::And(vec![
            kind(Kind::Head),
            b32("ag.rid", rid),
            Pred::Or(mine.collect()),
        ]));
        let select = Select {
            updated_at: true,
            attrs: Attrs::Named(vec![]),
            ..Select::default()
        };
        let opts = QueryOpts {
            limit: MAX_LIMIT,
            offset: 0,
            select,
        };
        let page = a.query(&q, &opts).await?;
        let now: BTreeMap<B256, u64> = page
            .rows
            .iter()
            .filter_map(|r| Some((r.key?, r.updated_at.or(r.created_at)?)))
            .collect();
        Ok(self.rows.iter().any(|(k, up)| now.get(k) != Some(up)))
    }
}
