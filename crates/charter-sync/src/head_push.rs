//! Format v2 push, part 1 (docs/p5/head-model.md 7.1): stage objects, read this identity's
//! fresh heads, compute the next body's refs and decide snapshot or pieces. Part 2
//! ([`crate::head_send`]) sends, re-checks and commits.

use std::collections::{BTreeMap, BTreeSet};

use charter_core::{
    DeviceHead, ErrorCode, Hash32, HeadPack, HeadRef, Merged, Nid, RefEntry, RefsBody, Winner,
    ZERO, next_entries, resolve_stamps,
};
use charter_store::Arkiv;

use crate::device::device_keys;
use crate::git::Git;
use crate::heads::{self, V2};
use crate::keys::Keys;
use crate::manifest_cache;
use crate::pieces::PIECE_SIZE;
use crate::push::{Update, stage};
use crate::repo::{Config, Repo};
use crate::state::{LandingJson, State};
use crate::view::{self, View};
use crate::{Result, SyncError};

/// Piece bytes since the snapshot that always allow another piece push (head-model 4.4).
pub const SNAPSHOT_MIN_BYTES: u64 = 1 << 20;
/// Packs since the snapshot after which the next push snapshots.
pub const SNAPSHOT_MAX_PUSHES: usize = 64;
/// Most snapshot layers one head lists; the refresh after that rewrites one snapshot.
pub const MAX_LAYERS: usize = 8;
/// Largest pack sent inline in the head tx (larger ones go as bulk pieces first).
pub const INLINE_MAX: usize = 16 * 1024;

/// A v2 push in progress.
pub struct HeadPush<'a, A: Arkiv> {
    pub(crate) a: &'a A,
    pub(crate) repo: &'a Repo,
    pub(crate) cfg: &'a Config,
    pub(crate) keys: &'a Keys,
    pub(crate) dev: Keys,
    pub view: View,
    /// `(dst, new oid hex)`; `None` deletes.
    pub planned: Vec<(String, Option<String>)>,
}

/// This device's current body and where it landed.
#[derive(Debug, Clone)]
pub struct Prev {
    pub head: DeviceHead,
    pub landing: LandingJson,
}

/// What the next body says, before its pack bytes are sent.
#[derive(Debug, Clone)]
pub struct Plan {
    pub prev: Option<Prev>,
    pub refs: Vec<HeadRef>,
    /// Live refs of the identity before and after this push.
    pub before: BTreeMap<String, Vec<u8>>,
    pub after: BTreeMap<String, Vec<u8>>,
    /// The thin pack of the pushed objects (empty for refs-only pushes).
    pub thin: Vec<u8>,
    pub snapshot: bool,
    /// On a snapshot push: the layers kept, by git pack trailer (empty rewrites them all).
    pub keep_layers: Vec<String>,
    /// This device's packs since its snapshot, with `tx = 0` made concrete.
    pub carried: Vec<HeadPack>,
}

fn oid_bytes(hex_oid: &str) -> Result<Vec<u8>> {
    hex::decode(hex_oid).map_err(|e| SyncError::io("oid", e))
}

fn refs_map(r: &RefsBody) -> BTreeMap<String, Vec<u8>> {
    r.refs
        .iter()
        .map(|e| (e.name.clone(), e.oid.clone()))
        .collect()
}

/// `refs` of `rid` as a refs body.
pub(crate) fn refs_body(rid: Hash32, refs: &BTreeMap<String, Vec<u8>>) -> RefsBody {
    RefsBody {
        rid,
        object_hash: 1,
        refs: refs
            .iter()
            .map(|(name, oid)| RefEntry {
                name: name.clone(),
                oid: oid.clone(),
            })
            .collect(),
    }
}

fn live(merged: &Merged) -> BTreeMap<String, Vec<u8>> {
    merged
        .iter()
        .filter(|(_, w)| !w.entry.is_tombstone())
        .map(|(n, w)| (n.clone(), w.entry.oid.clone()))
        .collect()
}

impl<'a, A: Arkiv> HeadPush<'a, A> {
    /// Stages `updates` from `work`. `view` must come from a fetch sync that included this
    /// identity (`Want::must`).
    pub fn prepare(
        a: &'a A,
        repo: &'a Repo,
        cfg: &'a Config,
        keys: &'a Keys,
        view: View,
        work: &Git,
        updates: &[Update],
    ) -> Result<Self> {
        let planned = stage(repo, &keys.nid(), work, updates)?;
        Ok(Self {
            a,
            repo,
            cfg,
            keys,
            dev: device_keys(&cfg.home)?,
            view,
            planned,
        })
    }

    pub(crate) fn rid(&self) -> Hash32 {
        self.repo.rid
    }

    pub(crate) fn nid(&self) -> Nid {
        self.keys.nid()
    }

    pub(crate) fn me(&self) -> Nid {
        self.dev.nid()
    }

    /// The identity's refs after this push as the view sees them (program checks).
    pub fn refs_after(&self) -> Result<RefsBody> {
        let mut refs: BTreeMap<String, Vec<u8>> = self
            .view
            .peers
            .get(&self.nid())
            .and_then(|p| p.refs_body())
            .map(|r| {
                r.refs
                    .iter()
                    .map(|e| (e.name.clone(), e.oid.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (name, oid) in &self.planned {
            match oid {
                Some(o) => refs.insert(name.clone(), oid_bytes(o)?),
                None => refs.remove(name),
            };
        }
        Ok(RefsBody {
            rid: self.rid(),
            object_hash: 1,
            refs: refs
                .into_iter()
                .map(|(name, oid)| RefEntry { name, oid })
                .collect(),
        })
    }

    /// This identity's v2 view, read fresh (`None` before its first device delegation).
    pub(crate) async fn own_v2(&self, state: &State) -> Result<Option<V2>> {
        let Some(info) = view::resolve_peer(self.a, &self.rid(), &self.nid()).await? else {
            return Ok(None);
        };
        let rows = view::anchors(self.a, &self.rid(), &self.nid(), &info).await?;
        let device_rows: Vec<_> = rows
            .into_iter()
            .filter(charter_core::is_device_row)
            .collect();
        if device_rows.is_empty() {
            return Ok(None);
        }
        let v2 = Box::pin(heads::build(
            self.a,
            &self.rid(),
            &self.nid(),
            &info,
            &self.view.identity,
            &device_rows,
            state,
        ))
        .await?;
        Ok(Some(v2))
    }

    async fn prev(&self, v2: &V2, state: &State) -> Result<Option<Prev>> {
        let Some(d) = v2.device(&self.me()) else {
            return Ok(None);
        };
        let Some(head) = d.head.clone() else {
            return Ok(None);
        };
        let landing = match state.landing(&head.h) {
            Some(l) => Some(l),
            // Our own body: search up to its last update, past the readers' window.
            None => heads::landing(self.a, &d.device, &head, u64::MAX).await?,
        };
        let landing = landing.ok_or_else(|| {
            SyncError::coded(
                ErrorCode::NotLive,
                "this device's current head body was not found in calldata",
            )
        })?;
        Ok(Some(Prev { head, landing }))
    }

    /// The merge before this push: v2 heads, or a migrating identity's v1 refs at counter 1.
    fn merged_before(&self, v2: Option<&V2>) -> Merged {
        if let Some(v) = v2.filter(|v| v.heads().next().is_some()) {
            return v.merged.clone();
        }
        let v1 = self
            .view
            .peers
            .get(&self.nid())
            .and_then(|p| p.head.as_ref())
            .map(|h| h.refs.refs.clone())
            .unwrap_or_default();
        v1.into_iter()
            .map(|e| {
                let entry = HeadRef {
                    name: e.name.clone(),
                    oid: e.oid,
                    counter: 1,
                    stamp: 0,
                    base: vec![],
                };
                (
                    e.name,
                    Winner {
                        device: self.me(),
                        entry,
                    },
                )
            })
            .collect()
    }

    /// Computes the next body's refs and pack plan (no sends), from a fresh read of this
    /// identity's heads.
    pub async fn plan(&self, state: &State) -> Result<Plan> {
        self.plan_from(state, false).await
    }

    /// Like [`Self::plan`]; with `reuse_view` the heads verified by the `list for-push`
    /// sync are the base (head-model 7.1 step 1), saving a second read. Only valid while
    /// nothing of this identity was sent since that sync.
    pub(crate) async fn plan_from(&self, state: &State, reuse_view: bool) -> Result<Plan> {
        let mine = self.view.peers.get(&self.nid()).and_then(|p| p.v2.clone());
        let reread = !(reuse_view && mine.is_some());
        let v2 = match (reuse_view, mine) {
            (true, Some(v)) => Some(v),
            _ => Box::pin(self.own_v2(state)).await?,
        };
        let prev = match &v2 {
            Some(v) => self.prev(v, state).await?,
            None => None,
        };
        let merged = self.merged_before(v2.as_ref());
        if reread {
            self.check_listing(&merged)?;
        }
        let others: BTreeSet<String> = v2
            .iter()
            .flat_map(|v| v.heads())
            .filter(|(d, _)| d.nid != self.me())
            .flat_map(|(_, h)| h.body.refs.iter().map(|e| e.name.clone()))
            .collect();
        let prev_refs: Vec<HeadRef> = match &prev {
            Some(p) => resolve_stamps(&p.head.body.refs, p.landing.block),
            None => merged
                .values()
                .filter(|w| w.device == self.me())
                .map(|w| w.entry.clone())
                .collect(),
        };
        let updates = self
            .planned
            .iter()
            .map(|(n, o)| Ok((n.clone(), o.as_deref().map(oid_bytes).transpose()?)))
            .collect::<Result<Vec<_>>>()?;
        let with_base: Vec<charter_core::EntryUpdate> = updates
            .iter()
            .map(|(n, o)| (n.clone(), o.clone(), self.base(&merged, n, o.as_deref())))
            .collect();
        let refs = next_entries(&self.me(), &prev_refs, &merged, &others, &with_base);
        let before = live(&merged);
        let mut after = before.clone();
        for (n, o) in &updates {
            match o {
                Some(o) => after.insert(n.clone(), o.clone()),
                None => after.remove(n),
            };
        }
        let tips: Vec<String> = updates
            .iter()
            .filter_map(|(_, o)| o.as_ref().map(hex::encode))
            .collect();
        let exclude: Vec<String> = before.values().map(hex::encode).collect();
        let thin = match tips.is_empty() {
            true => vec![],
            false => self.repo.git().thin_pack_bytes(&tips, &exclude)?,
        };
        let carried = carried(prev.as_ref());
        let has_snapshot = v2
            .iter()
            .flat_map(|v| v.heads())
            .any(|(_, h)| !h.body.snapshot.is_empty());
        let layers = match (&prev, has_snapshot) {
            (Some(p), true) => {
                let dev = self.own_device(p)?;
                let hashes = &p.head.body.snapshot;
                manifest_cache::facts(self.a, self.repo, &self.rid(), &dev, hashes).await?
            }
            _ => vec![],
        };
        let snapshot = !has_snapshot || snapshot_due(&layers, &carried, thin.len());
        let keep_layers = match snapshot {
            true => layers_to_keep(&layers, |t| self.repo.git().has_pack(t)),
            false => vec![],
        };
        Ok(Plan {
            prev,
            refs,
            before,
            after,
            thin,
            snapshot,
            keep_layers,
            carried,
        })
    }

    /// git decided each update against the listing of the view; a pushed ref another
    /// device moved since then would be overwritten unseen, so it is a lost race.
    fn check_listing(&self, fresh: &Merged) -> Result<()> {
        let listed: BTreeMap<String, Vec<u8>> = self
            .view
            .peers
            .get(&self.nid())
            .and_then(|p| p.refs_body())
            .map(refs_map)
            .unwrap_or_default();
        let fresh = live(fresh);
        match self
            .planned
            .iter()
            .find(|(name, _)| listed.get(name) != fresh.get(name))
        {
            Some((name, _)) => Err(SyncError::coded(
                ErrorCode::LostRace,
                format!(
                    "another device of this identity moved {name} after git checked it; fetch and push again"
                ),
            )),
            None => Ok(()),
        }
    }

    /// The signed `base` of an update (head-model 8): the pinned program's accepted head
    /// if `new` fast-forwards it, else the merged oid if `new` fast-forwards that, by git
    /// ancestry in the cache (which holds both), else empty.
    fn base(&self, merged: &Merged, name: &str, new: Option<&[u8]>) -> Vec<u8> {
        let Some(new) = new else { return vec![] };
        let accepted = self
            .view
            .program
            .as_ref()
            .and_then(|e| e.outcome.heads.get(name));
        let winner = merged
            .get(name)
            .filter(|w| !w.entry.is_tombstone())
            .map(|w| &w.entry.oid);
        [accepted, winner]
            .into_iter()
            .flatten()
            .find(|old| crate::program::is_ancestor(self.repo.git(), old, new) == Some(true))
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn own_device(&self, p: &Prev) -> Result<charter_core::Device> {
        Ok(charter_core::Device {
            nid: self.me(),
            pk: self.dev.pk().clone(),
            control: self.dev.control_address(&self.rid()).into_array(),
            bulk: self.dev.bulk_address(&self.rid()).into_array(),
            h: ZERO,
            position: charter_core::Position {
                block: p.head.created_at,
                tx: 0,
                key: p.head.key,
            },
        })
    }
}

/// Head-model 4.4: piece bytes over max(1 MiB, 25% of the snapshot's layers), 64 packs,
/// or more pieces than one head may list.
fn snapshot_due(layers: &[manifest_cache::Facts], carried: &[HeadPack], new: usize) -> bool {
    let new_pieces = new.div_ceil(PIECE_SIZE);
    let pieces: usize = carried.iter().map(|p| p.pieces.len()).sum::<usize>() + new_pieces;
    if pieces > charter_core::MAX_HEAD_PIECES
        || carried.len() + usize::from(new > 0) > SNAPSHOT_MAX_PUSHES
    {
        return true;
    }
    let snap_bytes: u64 = layers.iter().map(|f| f.size).sum();
    // shortcut: counts every piece as full, so a refresh comes slightly early.
    let piece_bytes = (pieces * PIECE_SIZE) as u64;
    piece_bytes > SNAPSHOT_MIN_BYTES.max(snap_bytes / 4)
}

/// Layered refresh policy (speed-and-cost 2.8): keep every current layer and add one while
/// fewer than [`MAX_LAYERS`] exist and each layer's pack is in the cache (`cached(trailer)`);
/// otherwise keep none, so the refresh rewrites one compact snapshot. One rewrite in eight
/// refreshes bounds both the layer count and the bytes of deleted refs kept alive.
pub fn layers_to_keep(
    layers: &[manifest_cache::Facts],
    cached: impl Fn(&str) -> bool,
) -> Vec<String> {
    let ok = !layers.is_empty()
        && layers.len() < MAX_LAYERS
        && layers
            .iter()
            .all(|f| !f.trailer.is_empty() && cached(&f.trailer));
    match ok {
        true => layers.iter().map(|f| f.trailer.clone()).collect(),
        false => vec![],
    }
}

/// The previous body's packs with `tx = 0` replaced by the tx that carried it.
fn carried(prev: Option<&Prev>) -> Vec<HeadPack> {
    let Some(p) = prev else { return vec![] };
    p.head
        .body
        .packs
        .iter()
        .map(|pk| HeadPack {
            pack_id: pk.pack_id,
            pieces: pk
                .pieces
                .iter()
                .map(|x| charter_core::PieceRef {
                    tx: if x.tx == ZERO { p.landing.tx } else { x.tx },
                    sha256: x.sha256,
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifest_cache::Facts;

    fn f(size: u64, t: &str) -> Facts {
        Facts {
            size,
            trailer: t.into(),
        }
    }

    #[test]
    fn a_refresh_adds_a_layer_until_the_layer_cap() {
        let yes = |_: &str| true;
        assert!(layers_to_keep(&[], yes).is_empty(), "no snapshot: full");
        assert_eq!(layers_to_keep(&[f(100, "a")], yes), ["a"]);
        assert_eq!(layers_to_keep(&[f(100, "a"), f(400, "b")], yes), ["a", "b"]);
        let many: Vec<Facts> = (0..MAX_LAYERS as u64).map(|i| f(i + 1, "x")).collect();
        assert_eq!(layers_to_keep(&many[1..], yes).len(), MAX_LAYERS - 1);
        assert!(layers_to_keep(&many, yes).is_empty(), "layer cap: compact");
    }

    #[test]
    fn a_layer_whose_pack_left_the_cache_forces_a_full_snapshot() {
        let layers = [f(100, "a"), f(10, "b")];
        assert!(layers_to_keep(&layers, |t| t != "b").is_empty());
        assert!(
            layers_to_keep(&[f(100, "")], |_| true).is_empty(),
            "unknown trailer"
        );
    }

    #[test]
    fn snapshot_due_counts_every_layer() {
        let one = [f(8 << 20, "a")];
        let two = [f(4 << 20, "a"), f(4 << 20, "b")];
        let pieces = 2 << 20;
        assert!(!snapshot_due(&one, &[], pieces), "2 MiB under 25% of 8 MiB");
        assert!(!snapshot_due(&two, &[], pieces), "the same over two layers");
        assert!(snapshot_due(&[f(4 << 20, "a")], &[], pieces));
    }
}
