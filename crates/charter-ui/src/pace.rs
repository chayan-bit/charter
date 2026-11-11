//! How often the sync loop may spend RPC quota (docs/p6/ui.md 4.2, 4.5). Rabdi bills an
//! `arkiv_query` 10 units of a per-IP budget of 10,000 (docs/p0/arkiv-limits.md), and a full
//! round is 10 or more queries, so a round runs only when a one-query [`probe`] saw the repo
//! change:
//!
//! - the repo on screen is probed every [`ON_SCREEN_EVERY`] (with a live `newHeads` socket,
//!   at the first block after that), other repos every [`IDLE_EVERY`] while any page is open,
//!   and nothing at all with no page open; the first sync of an uncached `chart ui <rid>`
//!   repo runs at once;
//! - intervals stretch as the RPC reports its quota draining ([`slowdown`]), background work
//!   stops at [`PAUSE_AT`] percent used or on a 429 until the quota resets, and the pages
//!   say so.
//!
//! The probe covers every entity a round reads that carries the repo's `ag.rid`: identity,
//! bind, refs, anchors, heads, manifests, chunks and cob ops; creates and expiries by count
//! and newest row, head patches (a refs-only v3 push) by the heads' `$updatedAt`. A peer's own key entity carries no
//! rid; it changes nothing on screen until that peer writes a bind or an anchor, which the
//! probe sees. Sealed repos tag their entities with a blinded rid, so they skip the probe
//! and get a full round at the interval of an idle repo, on screen or not.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use charter_core::Rid;
use charter_store::{
    Arkiv, Attrs, B256, MAX_LIMIT, OrderBy, OrderType, Pred, Query, QueryOpts, Quota, RowExt,
    Select, StoreError, attr,
};
use charter_sync::{Config, SyncError};

use crate::UiError;
use crate::cache::{self, RepoView};
use crate::sync_loop::{Kind, Outcome, Shared, round};

pub const ON_SCREEN_EVERY: Duration = Duration::from_secs(10);
pub const IDLE_EVERY: Duration = Duration::from_secs(300);
/// Background rounds stop at this much of the RPC quota used.
pub const PAUSE_AT: f64 = 80.0;
/// A 429 that names no `Retry-After` pauses this long.
const REFUSED_PAUSE: Duration = Duration::from_secs(60);

/// The repo's live entity count and the last rows of its entities sorted by `ag.dev`.
///
/// Only heads carry `ag.dev`, and rows without the sort attribute come first in creation
/// order (Arkiv `docs/api-spec.md` "Sorting"), so the list ends with the newest entity that
/// is not a head, then every head. Each row keeps its `$updatedAt`, which a head patch
/// moves: a v3 push that only moves refs patches the head in place and creates nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub count: u64,
    pub tail: Vec<(B256, u64)>,
}

/// `(count, rows from offset)` of the repo's entities in `ag.dev` order: one `arkiv_query`.
async fn page<A: Arkiv>(
    a: &A,
    rid: &Rid,
    offset: u64,
    limit: u32,
) -> Result<(u64, Vec<(B256, u64, bool)>), StoreError> {
    let q = Query {
        filter: Pred::eq(attr("ag.rid"), B256::from(*rid)),
        order: Some(OrderBy {
            field: attr("ag.dev"),
            ty: OrderType::Bytes32,
            desc: false,
        }),
    };
    let select = Select {
        updated_at: true,
        count: true,
        attrs: Attrs::Named(vec!["ag.dev".into()]),
        ..Select::default()
    };
    let opts = QueryOpts {
        limit,
        offset,
        select,
    };
    let page = a.query(&q, &opts).await?;
    let count = page
        .count
        .ok_or_else(|| StoreError::Rpc("count missing".into()))?;
    let rows = page.rows.iter().map(|r| {
        let up = r.updated_at.or(r.created_at).unwrap_or(0);
        (
            r.key.unwrap_or_default(),
            up,
            r.attr_b256("ag.dev").is_some(),
        )
    });
    Ok((count, rows.collect()))
}

/// One query when nothing changed: the rows of the last probe's tail, at the same offset.
/// A create or an expiry moves the count (or, both at once, the newest non-head row), a head
/// patch a head's `$updatedAt`; then one more query reads the new tail.
pub async fn probe<A: Arkiv>(a: &A, rid: &Rid, prev: Option<&Probe>) -> Result<Probe, StoreError> {
    let keys = |rows: Vec<(B256, u64, bool)>| -> Vec<(B256, u64)> {
        rows.into_iter().map(|(k, up, _)| (k, up)).collect()
    };
    let count = match prev {
        Some(p) => {
            let n = p.tail.len() as u64;
            let (count, rows) = page(a, rid, p.count - n, n.max(1) as u32).await?;
            let now = Probe {
                count,
                tail: keys(rows),
            };
            if now == *p || count == 0 {
                return Ok(now);
            }
            count
        }
        None => page(a, rid, 0, 1).await?.0,
    };
    // shortcut: assumes fewer than 100 devices pushed to the repo; past that the tail is all
    // heads and a create among older rows shows only in the count.
    let (count, rows) = page(a, rid, count.saturating_sub(MAX_LIMIT.into()), MAX_LIMIT).await?;
    let from = rows.iter().rposition(|r| !r.2).unwrap_or(0);
    Ok(Probe {
        count,
        tail: keys(rows[from..].to_vec()),
    })
}

/// Interval multiplier as the quota drains: x1 under 50 % used, x2 under 65 %, x4 under
/// [`PAUSE_AT`]; `None` (pause) from there.
pub fn slowdown(used: Option<f64>) -> Option<u32> {
    match used.unwrap_or(0.0) {
        u if u < 50.0 => Some(1),
        u if u < 65.0 => Some(2),
        u if u < PAUSE_AT => Some(4),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Wait,
    /// Probe; a full round only if it changed.
    Check,
    /// A full round regardless (first sync, sealed repo), probing first when possible.
    Full,
}

#[derive(Debug, Default)]
struct State {
    probe: Option<Probe>,
    checked: Option<Instant>,
    /// The repo changed and no round has caught up yet (failed or lock busy).
    dirty: bool,
}

/// The loop's pacing state; [`Pacer::tick`] does whatever is due at `now`.
#[derive(Debug, Default)]
pub struct Pacer {
    repos: BTreeMap<Rid, State>,
    /// The latest `newHeads` block and when it arrived; `None` while no socket is live.
    head: Option<(u64, Instant)>,
}

fn private(home: &Path, rid: &Rid) -> Option<bool> {
    let dir = home.join("repos").join(charter_sync::ids::hex32(rid));
    let v: Option<RepoView> = cache::read(&dir.join(cache::VIEW)).ok().flatten();
    v.map(|v| v.private)
}

fn retry_after(e: &UiError) -> Option<Option<Duration>> {
    match e {
        UiError::Sync(SyncError::Store(StoreError::RateLimited { retry_after })) => {
            Some(*retry_after)
        }
        _ => None,
    }
}

impl Pacer {
    /// A block from the `newHeads` socket, or `None` when it dropped.
    pub fn head(&mut self, n: Option<u64>, now: Instant) {
        self.head = match (n, self.head) {
            (None, _) => None,
            (Some(n), Some((m, t))) if n == m => Some((m, t)),
            (Some(n), _) => Some((n, now)),
        };
    }

    fn step(&self, shared: &Shared, home: &Path, rid: &Rid, now: Instant, slow: u32) -> Step {
        let st = self.repos.get(rid);
        let checked = st.and_then(|s| s.checked);
        if checked.is_none() && shared.wanted(rid) && private(home, rid).is_none() {
            return Step::Full;
        }
        if !shared.any_open(now) {
            return Step::Wait;
        }
        let sealed = private(home, rid) == Some(true);
        let on = shared.on_screen(rid, now) && !sealed;
        let every = slow * if on { ON_SCREEN_EVERY } else { IDLE_EVERY };
        let Some(t) = checked else {
            return Step::Check;
        };
        if now < t + every {
            return Step::Wait;
        }
        // On screen with a live socket: the first block after the interval, not the timer.
        let no_block = self.head.is_some_and(|(_, at)| at <= t);
        if on && no_block && now < t + 3 * every {
            return Step::Wait;
        }
        if sealed { Step::Full } else { Step::Check }
    }

    /// Runs what is due at `now` for every repo the loop knows.
    pub async fn tick<A: Arkiv>(&mut self, a: &A, cfg: &Config, shared: &Shared, now: Instant) {
        if shared.paused().is_some_and(|until| now < until) {
            return;
        }
        let quota = a.quota();
        let Some(slow) = slowdown(quota.as_ref().and_then(Quota::used)) else {
            return self.pause(shared, &cfg.home, quota.map(|q| q.until()));
        };
        self.pause(shared, &cfg.home, None);
        for rid in shared.repos(&cfg.home) {
            let step = self.step(shared, &cfg.home, &rid, now, slow);
            if step == Step::Wait {
                continue;
            }
            if let Err(e) = self.run(a, cfg, shared, rid, step, now).await {
                if let Some(wait) = retry_after(&e) {
                    let until = now + wait.unwrap_or(REFUSED_PAUSE);
                    return self.pause(shared, &cfg.home, Some(until));
                }
                charter_sync::warn(&format!("ui sync {}: {e}", charter_sync::ids::hex32(&rid)));
                shared.failed(rid, e.to_string());
                shared.emit(rid, Kind::Sync);
            }
        }
    }

    /// Sets or clears the pause; the pages hear of a change.
    fn pause(&self, shared: &Shared, home: &Path, until: Option<Instant>) {
        if shared.paused() == until {
            return;
        }
        shared.pause(until);
        shared
            .repos(home)
            .into_iter()
            .for_each(|r| shared.emit(r, Kind::Sync));
    }

    async fn run<A: Arkiv>(
        &mut self,
        a: &A,
        cfg: &Config,
        shared: &Shared,
        rid: Rid,
        step: Step,
        now: Instant,
    ) -> Result<(), UiError> {
        let st = self.repos.entry(rid).or_default();
        st.checked = Some(now);
        if private(&cfg.home, &rid) != Some(true) {
            let p = probe(a, &rid, st.probe.as_ref())
                .await
                .map_err(SyncError::from)?;
            st.dirty |= st.probe.as_ref() != Some(&p);
            st.probe = Some(p);
        }
        if step == Step::Check && !st.dirty {
            shared.checked(rid);
            shared.emit(rid, Kind::Sync);
            return Ok(());
        }
        let r = round(a, cfg, &rid).await;
        if r.as_ref().is_err_and(|e| retry_after(e).is_some()) {
            return r.map(|_| ());
        }
        st.dirty = !matches!(r, Ok(Outcome::Synced(_)));
        let kinds = Outcome::kinds(&r);
        shared.recorded(rid, r);
        kinds.into_iter().for_each(|k| shared.emit(rid, k));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_loop_slows_as_the_quota_drains_and_stops_at_eighty_percent() {
        let s = [
            None,
            Some(10.0),
            Some(55.0),
            Some(70.0),
            Some(80.0),
            Some(100.0),
        ];
        let got: Vec<_> = s.into_iter().map(slowdown).collect();
        assert_eq!(got, [Some(1), Some(1), Some(2), Some(4), None, None]);
    }
}
