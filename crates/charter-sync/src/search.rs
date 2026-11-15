//! Search (design 6): annotation queries give candidate rids (hints from permissionless
//! data); each is reduced to its verified identity head, and hits whose head no longer
//! matches the predicate (renamed repo, changed licence) are dropped.

use std::collections::{BTreeMap, BTreeSet};

use charter_core::{AttrValue, IdBody, Kind, Nid, Observed, Rid};
use charter_store::{Arkiv, OrderBy, OrderType, Pred, Query, QueryOpts, attr};

use crate::observe::{b32, full_select, kind, to_observed};
use crate::repo::BLOCKS_PER_DAY;
use crate::state::State;
use crate::{Result, view};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery {
    pub name: Option<String>,
    pub prefix: Option<String>,
    pub lang: Option<String>,
    /// SPDX ids; any of them matches.
    pub licenses: Vec<String>,
    pub peer: Option<Nid>,
    pub recent_days: Option<u64>,
    /// At most 100 rows per underlying query.
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub rid: Rid,
    pub head: IdBody,
    /// Newest `ag.at` seen for the repo (recency queries only).
    pub at: Option<u64>,
}

fn rid_of(o: &Observed) -> Option<Rid> {
    match o.attr("ag.rid") {
        Some(AttrValue::Bytes32(r)) => Some(*r),
        _ => None,
    }
}

fn at_of(o: &Observed) -> Option<u64> {
    match o.attr("ag.at") {
        Some(AttrValue::U64(a)) => Some(*a),
        _ => None,
    }
}

async fn page<A: Arkiv>(a: &A, q: Query, limit: u32) -> Result<Vec<Observed>> {
    let opts = QueryOpts {
        limit: limit.clamp(1, 100),
        offset: 0,
        select: full_select(),
    };
    Ok(to_observed(a.query(&q, &opts).await?.rows))
}

fn id_filter(q: &SearchQuery) -> Option<Pred> {
    let mut terms = vec![];
    if let Some(n) = &q.name {
        terms.push(Pred::eq(attr("ag.name"), n.as_str()));
    }
    if let Some(p) = &q.prefix {
        terms.push(Pred::StartsWith(attr("ag.name"), p.clone()));
    }
    if let Some(l) = &q.lang {
        terms.push(Pred::eq(attr("ag.lang"), l.as_str()));
    }
    if !q.licenses.is_empty() {
        let any = q
            .licenses
            .iter()
            .map(|l| Pred::eq(attr("ag.lic"), l.as_str()));
        terms.push(Pred::Or(any.collect()));
    }
    (!terms.is_empty()).then(|| Pred::And([vec![kind(Kind::Id)], terms].concat()))
}

/// Candidate rids from each requested query, intersected; `at` from recency hits.
async fn candidates<A: Arkiv>(a: &A, q: &SearchQuery) -> Result<BTreeMap<Rid, Option<u64>>> {
    let mut sets: Vec<BTreeMap<Rid, Option<u64>>> = vec![];
    if let Some(f) = id_filter(q) {
        let rows = page(a, Query::from(f), q.limit).await?;
        sets.push(rows.iter().filter_map(rid_of).map(|r| (r, None)).collect());
    }
    if let Some(n) = &q.peer {
        let f = Pred::And(vec![kind(Kind::Bind), b32("ag.nid", n)]);
        let rows = page(a, Query::from(f), q.limit).await?;
        sets.push(rows.iter().filter_map(rid_of).map(|r| (r, None)).collect());
    }
    if let Some(days) = q.recent_days {
        let since = a.head().await?.saturating_sub(days * BLOCKS_PER_DAY);
        let f = Pred::And(vec![kind(Kind::Anchor), Pred::ge(attr("ag.at"), since)]);
        let query = Query {
            filter: f,
            order: Some(OrderBy {
                field: attr("ag.at"),
                ty: OrderType::U64,
                desc: true,
            }),
        };
        let mut m: BTreeMap<Rid, Option<u64>> = BTreeMap::new();
        for o in page(a, query, q.limit).await? {
            if let Some(r) = rid_of(&o) {
                let e = m.entry(r).or_default();
                *e = (*e).max(at_of(&o));
            }
        }
        sets.push(m);
    }
    let mut it = sets.into_iter();
    let first = it.next().unwrap_or_default();
    Ok(it.fold(first, |acc, s| {
        acc.into_iter()
            .filter_map(|(r, at)| s.get(&r).map(|at2| (r, at.max(*at2))))
            .collect()
    }))
}

fn matches(q: &SearchQuery, head: &IdBody) -> bool {
    q.name.as_ref().is_none_or(|n| *n == head.name)
        && q.prefix
            .as_ref()
            .is_none_or(|p| head.name.starts_with(p.as_str()))
        && q.lang
            .as_ref()
            .is_none_or(|l| head.lang.as_ref() == Some(l))
        && (q.licenses.is_empty()
            || head
                .license
                .as_ref()
                .is_some_and(|l| q.licenses.contains(l)))
}

/// Verified search hits, newest first for recency queries, else by name.
pub async fn search<A: Arkiv>(a: &A, q: &SearchQuery) -> Result<Vec<Hit>> {
    Ok(search_until(a, q, || false).await?.0)
}

/// [`search`] that asks `stop` before each candidate's verification (3 queries for a
/// one-maintainer repo) and, once it says so, verifies no more; the flag is whether it did.
pub async fn search_until<A: Arkiv>(
    a: &A,
    q: &SearchQuery,
    stop: impl Fn() -> bool,
) -> Result<(Vec<Hit>, bool)> {
    let mut hits = vec![];
    let mut seen = BTreeSet::new();
    let mut stopped = false;
    for (rid, at) in candidates(a, q).await? {
        if !seen.insert(rid) {
            continue;
        }
        if stop() {
            stopped = true;
            break;
        }
        let Ok((chain, _)) = view::identity(a, &rid, &State::default()).await else {
            continue;
        };
        let head = chain.head().body.clone();
        let delegate_ok = q.peer.is_none_or(|n| head.delegates.contains(&n));
        if matches(q, &head) && delegate_ok {
            hits.push(Hit { rid, head, at });
        }
    }
    hits.sort_by(|x, y| y.at.cmp(&x.at).then_with(|| x.head.name.cmp(&y.head.name)));
    Ok((hits, stopped))
}
