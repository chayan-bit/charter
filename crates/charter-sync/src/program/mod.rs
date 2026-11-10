//! Repo program enforcement (docs/p3/programs.md, "Enforcement"): the pinned source comes
//! from an `xyz.arkiv.program` publish op, verified ops and the delegates' signed-refs
//! updates become fold events in Arkiv order, and the fold starts from the canonical heads
//! at the pin. Fetch lists the program's accepted heads; the helper refuses a push the
//! program would reject.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use charter_core::{
    AttrValue, Body, Envelope, ErrorCode, Hash32, IdentityChain, Kind, Merged, Nid, ProgramPin,
    RefsBody, Revision, VerifiedOp, canonical_refs, find_refs, verify_ops,
};
use charter_program::{Checkpoint, Event, Outcome, Program, ProgramOp, RefUpdate, RepoInfo};
use charter_store::{Arkiv, Pred};
use serde_json::{Value, json};

use crate::git::Git;
use crate::observe::{all, b32, by_hashes, kind};
use crate::state::{PendingPush, unhex};
use crate::view::View;
use crate::{Result, SyncError, cob};

pub mod heads;
pub mod proofs;
mod publish;

pub use publish::{obligations, obligations_in, publish};

/// COB type of program source ops.
pub const PROGRAM_TYPE: &str = "xyz.arkiv.program";
/// Action of the op that carries `{engine, source}`.
pub const PUBLISH: &str = "publish";
/// Arkiv order of a would-be push: after everything on chain (scripts see orders as i64).
const PENDING_ORDER: u64 = i64::MAX as u64;

/// A pinned program folded over the repo; `outcome.heads` are the canonical heads.
#[derive(Debug, Clone)]
pub struct Enforced {
    /// The pinned engine tag and source (compiled per fold: a rhai AST is not `Send`, and
    /// views cross await points).
    pub engine: String,
    pub source: String,
    pub repo: RepoInfo,
    /// Canonical heads at the pin's identity revision (the fold's starting heads).
    pub seed: BTreeMap<String, Vec<u8>>,
    pub events: Vec<Event>,
    /// Block of each Arkiv order (rank), to turn an obligation's `due_order` into `since_block`.
    pub blocks: Vec<u64>,
    pub outcome: Outcome,
    /// The fold paused after `events`: a push check folds only its own updates from here.
    pub checkpoint: Option<Checkpoint>,
}

/// File next to the git cache holding the last fold's checkpoint (incremental folds).
const FOLD_CACHE: &str = "arkiv-fold.json";

fn missing(msg: impl Into<String>) -> SyncError {
    SyncError::coded(ErrorCode::ProgramMissing, msg)
}

/// Folds `e.events` then `extra`, resuming from `cache` when it is a checkpoint of a
/// prefix of exactly these events (charter-program checks that); returns the outcome and
/// the checkpoint after all of them.
fn fold(
    e: &Enforced,
    extra: Vec<RefUpdate>,
    git: &Git,
    cache: Option<&Checkpoint>,
) -> Result<(Outcome, Option<Checkpoint>)> {
    let events: Vec<Event> = e
        .events
        .iter()
        .cloned()
        .chain(extra.into_iter().map(Event::Push))
        .collect();
    let ancestry = |a: &[u8], b: &[u8]| is_ancestor(git, a, b);
    let (o, cp) = Program::load(&e.engine, &e.source)
        .map_err(|err| missing(err.to_string()))?
        .fold_cached(
            &e.source,
            e.repo.clone(),
            e.seed.clone(),
            &events,
            &ancestry,
            cache,
        )
        .map_err(|err| missing(format!("program fold failed: {err}")))?;
    let o = Outcome {
        obligations: o
            .obligations
            .into_iter()
            .map(|v| since_block(v, &e.blocks))
            .collect(),
        ..o
    };
    Ok((o, cp))
}

/// The checkpoint saved by the last fold of this cache, if readable.
fn load_checkpoint(git: &Git) -> Option<Checkpoint> {
    let b = std::fs::read(git.dir()?.join(FOLD_CACHE)).ok()?;
    serde_json::from_slice(&b).ok()
}

/// Keeps `cp` for the next fold; a cache that cannot be written only costs a full fold.
fn save_checkpoint(git: &Git, cp: Option<&Checkpoint>) {
    let Some(path) = git.dir().map(|d| d.join(FOLD_CACHE)) else {
        return;
    };
    let r = match cp {
        Some(cp) => crate::state::write_json(&path, cp),
        None => std::fs::remove_file(&path).or(Ok(())),
    };
    if let Err(e) = r {
        crate::warn(&format!("fold cache: {e}"));
    }
}

/// Adds `since_block` (the block of Arkiv order `due_order`) to an obligation that names
/// the event that made it due and has no `since_block` of its own.
fn since_block(mut v: Value, blocks: &[u64]) -> Value {
    let due = v.get("due_order").and_then(Value::as_u64);
    let block = due.and_then(|o| blocks.get(usize::try_from(o).ok()?).copied());
    if let (Some(b), Some(m)) = (block, v.as_object_mut()) {
        m.entry("since_block").or_insert(json!(b));
    }
    v
}

/// Git ancestry of commit `a` to commit `b` in `git`, or `None` when git cannot decide
/// (either commit, or any commit between them, is missing): a `None` keeps the fold from
/// being checkpointed, so the verdict is taken again once the objects arrive.
pub fn is_ancestor(git: &Git, a: &[u8], b: &[u8]) -> Option<bool> {
    let (a, b) = (hex::encode(a), hex::encode(b));
    match git.exit_code(&["merge-base", "--is-ancestor", &a, &b]) {
        Ok(Some(0)) => Some(true),
        Ok(Some(1)) => Some(false),
        _ => None,
    }
}

impl Enforced {
    /// Folds again with `updates` appended after everything on chain; the first of them the
    /// program rejects is `E_PROGRAM_REJECTED` with the program's reason.
    pub fn check(&self, updates: Vec<RefUpdate>, git: &Git) -> Result<()> {
        let (o, _) = fold(self, updates, git, self.checkpoint.as_ref())?;
        match o
            .rejected
            .iter()
            .find(|(u, _)| u.arkiv_order == PENDING_ORDER)
        {
            Some((_, why)) => Err(SyncError::coded(ErrorCode::ProgramRejected, why.clone())),
            None => Ok(()),
        }
    }
}

/// Ref moves of `author` from `old` to `new` refs, in ref-name order, at `order`.
pub fn ref_updates(
    author: Nid,
    old: Option<&RefsBody>,
    new: &RefsBody,
    order: u64,
) -> Vec<RefUpdate> {
    let map = |r: Option<&RefsBody>| -> BTreeMap<String, Vec<u8>> {
        r.iter()
            .flat_map(|r| &r.refs)
            .map(|e| (e.name.clone(), e.oid.clone()))
            .collect()
    };
    let (before, after) = (map(old), map(Some(new)));
    let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|n| before.get(*n) != after.get(*n))
        .map(|n| RefUpdate {
            author,
            name: n.clone(),
            old: before.get(n).cloned().unwrap_or_default(),
            new: after.get(n).cloned().unwrap_or_default(),
            arkiv_order: order,
            signed_base: false,
        })
        .collect()
}

/// The program view of a verified op at `order`.
pub fn program_op(o: &VerifiedOp, order: u64) -> ProgramOp {
    ProgramOp {
        id: o.id,
        author: o.author,
        rid: o.body.rid,
        object: o.object(),
        type_name: o.body.type_name.clone(),
        action: o.body.action.clone(),
        parents: o.body.parents.clone(),
        payload: o.body.payload.clone(),
        ts_ms: o.body.ts_ms,
        arkiv_order: order,
        proofs: vec![],
    }
}

/// Rank of `key` in the one creation-ordered query that holds every event.
fn order(rank: &HashMap<Hash32, u64>, key: &Hash32) -> Result<u64> {
    rank.get(key).copied().ok_or_else(|| {
        SyncError::coded(
            ErrorCode::InconsistentView,
            format!(
                "entity {} missing from the ordered event query",
                hex::encode(key)
            ),
        )
    })
}

/// Compiles the source of the first publish op whose `Program::hash` is the pinned hash.
fn source(pin: &ProgramPin, ops: &[VerifiedOp]) -> Result<String> {
    let src = ops
        .iter()
        .filter(|o| o.body.type_name == PROGRAM_TYPE && o.body.action == PUBLISH)
        .filter_map(|o| cob::json::to_json(&o.body.payload).ok())
        .find_map(|v| {
            let (engine, src) = (v["engine"].as_str()?, v["source"].as_str()?);
            (engine == pin.engine && Program::hash(src) == pin.hash).then(|| src.to_owned())
        })
        .ok_or_else(|| {
            missing(format!(
                "no {PROGRAM_TYPE} publish op carries the pinned program {} {}",
                pin.engine,
                hex::encode(pin.hash)
            ))
        })?;
    Program::load(&pin.engine, &src).map_err(|e| missing(e.to_string()))?;
    Ok(src)
}

/// The earliest revision of the run up to the head that carries the head's pin.
fn pin_revision(chain: &IdentityChain) -> &Revision {
    let pin = &chain.head().body.program;
    chain
        .revisions
        .iter()
        .rev()
        .take_while(|r| r.body.program == *pin)
        .last()
        .unwrap_or_else(|| chain.head())
}

/// One state of a delegate's refs: at Arkiv order `order`, with the signed `base` of each
/// ref for format v2 (`None` for v1 links, whose `old` is the previous refs).
struct RefState {
    order: u64,
    refs: BTreeMap<String, Vec<u8>>,
    bases: Option<BTreeMap<String, Vec<u8>>>,
}

fn refs_map(r: &RefsBody) -> BTreeMap<String, Vec<u8>> {
    r.refs
        .iter()
        .map(|e| (e.name.clone(), e.oid.clone()))
        .collect()
}

/// Ref moves from `before` to `after` by `author` at `after.order`; a v2 move's `old` is
/// the signed base, so `ff` never depends on objects a client may lack (head-model 8).
fn diff(author: Nid, before: Option<&RefState>, after: &RefState) -> Vec<RefUpdate> {
    let empty = BTreeMap::new();
    let b = before.map(|s| &s.refs).unwrap_or(&empty);
    let names: BTreeSet<&String> = b.keys().chain(after.refs.keys()).collect();
    names
        .into_iter()
        .filter(|n| b.get(*n) != after.refs.get(*n))
        .map(|n| {
            let new = after.refs.get(n).cloned().unwrap_or_default();
            let new_empty = new.is_empty();
            let old = match (&after.bases, new.is_empty()) {
                (Some(bases), false) => bases.get(n).cloned().unwrap_or_default(),
                _ => b.get(n).cloned().unwrap_or_default(),
            };
            RefUpdate {
                author,
                name: n.clone(),
                old,
                new,
                arkiv_order: after.order,
                signed_base: after.bases.is_some() && !new_empty,
            }
        })
        .collect()
}

/// Seed votes and post-pin ref updates of every delegate: v1 anchor links, then format v2
/// merges per landing block (`v2` holds each delegate's timeline with its orders).
async fn ref_events<A: Arkiv>(
    a: &A,
    view: &View,
    rank: &HashMap<Hash32, u64>,
    pin_order: u64,
    v2: &BTreeMap<Nid, Vec<(u64, Merged)>>,
) -> Result<(BTreeMap<Nid, RefsBody>, Vec<RefUpdate>)> {
    let (mut votes, mut updates) = (BTreeMap::new(), vec![]);
    for nid in view.delegates() {
        let Some(pv) = view.peers.get(nid) else {
            continue;
        };
        let mut states: Vec<RefState> = v2
            .get(nid)
            .into_iter()
            .flatten()
            .map(|(order, m)| {
                let live = heads::live_with_base(m);
                RefState {
                    order: *order,
                    refs: live
                        .iter()
                        .map(|(n, (o, _))| (n.clone(), o.clone()))
                        .collect(),
                    bases: Some(live.into_iter().map(|(n, (_, b))| (n, b)).collect()),
                }
            })
            .collect();
        let v2_seed = states.iter().any(|s| s.order < pin_order);
        let links = &pv.chain.links;
        let orders = links
            .iter()
            .map(|l| order(rank, &l.position.key))
            .collect::<Result<Vec<_>>>()?;
        // Only the last pre-pin push (the seed vote) and later ones need their refs.
        let from = match v2_seed {
            true => orders
                .iter()
                .position(|o| *o >= pin_order)
                .unwrap_or(orders.len()),
            false => orders.iter().rposition(|o| *o < pin_order).unwrap_or(0),
        };
        let hashes: Vec<Hash32> = links[from..].iter().map(|l| l.body.refs).collect();
        let observed = by_hashes(a, kind(Kind::Refs), &hashes).await?;
        for (link, ord) in links.iter().zip(&orders).skip(from) {
            let refs =
                find_refs(&view.rid, &pv.info, &observed, &link.body.refs).ok_or_else(|| {
                    let h = hex::encode(link.body.refs);
                    SyncError::coded(
                        ErrorCode::NotLive,
                        format!("refs {h} after the program pin"),
                    )
                })?;
            states.push(RefState {
                order: *ord,
                refs: refs_map(&refs),
                bases: None,
            });
        }
        states.sort_by_key(|s| s.order);
        let mut prev: Option<&RefState> = None;
        for s in &states {
            match s.order < pin_order {
                true => {
                    let refs = RefsBody {
                        rid: view.rid,
                        object_hash: 1,
                        refs: s
                            .refs
                            .iter()
                            .map(|(name, oid)| charter_core::RefEntry {
                                name: name.clone(),
                                oid: oid.clone(),
                            })
                            .collect(),
                    };
                    votes.insert(*nid, refs);
                }
                false => updates.extend(diff(*nid, prev, s)),
            }
            prev = Some(s);
        }
    }
    Ok((votes, updates))
}

/// The v1 refs entities a fold of the pinned program reads: each delegate's last anchor
/// link before the pin (the seed vote) and every later one. They outlive the peer's own
/// v1 head, also after it migrated to format v2, so keepalive keeps them (head-model 10).
pub async fn fold_v1_refs<A: Arkiv>(
    a: &A,
    view: &View,
) -> Result<Vec<(Nid, charter_core::Observed)>> {
    if view.identity.head().body.program.is_none() {
        return Ok(vec![]);
    }
    let pin = pin_revision(&view.identity).effective.block;
    let mut out = vec![];
    for nid in view.delegates() {
        let Some(pv) = view.peers.get(nid) else {
            continue;
        };
        let links = &pv.chain.links;
        // A link in the pin's own block may sit on either side of it: keep both.
        let from = links
            .iter()
            .rposition(|l| l.position.block < pin)
            .unwrap_or(0);
        let hashes: Vec<Hash32> = links[from..].iter().map(|l| l.body.refs).collect();
        let bulk = pv.info.binds.bulk_set();
        let found = by_hashes(a, kind(Kind::Refs), &hashes).await?;
        out.extend(
            found
                .into_iter()
                .filter(|o| bulk.contains(&o.creator))
                .map(|o| (*nid, o)),
        );
    }
    Ok(out)
}

/// Each delegate's v2 timeline (landing block, merge), bounded at the pin's block.
async fn v2_timelines<A: Arkiv>(
    a: &A,
    view: &View,
    stop: u64,
    git: &Git,
) -> Result<BTreeMap<Nid, Vec<(u64, Merged)>>> {
    let path = git.dir().map(|d| d.join("arkiv-head-bodies.json"));
    let mut cache = path
        .as_deref()
        .map(heads::BodyCache::load)
        .unwrap_or_default();
    let mut out = BTreeMap::new();
    for nid in view.delegates() {
        let Some(v2) = view.peers.get(nid).and_then(|p| p.v2.as_ref()) else {
            continue;
        };
        let bodies = heads::identity_bodies(a, &view.rid, v2, stop, &mut cache).await?;
        out.insert(*nid, heads::timeline(&bodies));
    }
    if let Some(p) = &path {
        cache.save(p)?;
    }
    Ok(out)
}

fn kind_is(o: &charter_core::Observed, k: Kind) -> bool {
    o.attr("ag.kind") == Some(&AttrValue::Str(k.name().to_owned()))
}

/// Folds the pinned program of `view`, or `None` when the identity pins none. A missing or
/// mismatched source is `E_PROGRAM_MISSING`: heads are never computed without the program.
/// `git` holds the pushed objects (fetch calls this after applying packs) for ancestry.
pub async fn enforce<A: Arkiv>(a: &A, view: &View, git: &Git) -> Result<Option<Enforced>> {
    let head = view.identity.head();
    let Some(pin) = head.body.program.clone() else {
        return Ok(None);
    };
    // One creation-ordered query, so ops, anchors and identity entities share one order.
    let kinds = [Kind::Cob, Kind::Anchor, Kind::Id, Kind::IdAcc].map(kind);
    let filter = Pred::And(vec![b32("ag.rid", &view.rid), Pred::Or(kinds.to_vec())]);
    let observed = all(a, filter).await?;
    let pin_rev = pin_revision(&view.identity);
    let timelines = Box::pin(v2_timelines(a, view, pin_rev.effective.block, git)).await?;
    // Arkiv order: entities in creation order; a block's v2 merges right after its entities.
    let v2_blocks: BTreeSet<u64> = timelines.values().flatten().map(|(b, _)| *b).collect();
    let mut keys_in_order: Vec<(u64, u8, usize)> = (0..observed.len())
        .map(|i| (observed[i].block, 0, i))
        .chain(v2_blocks.iter().map(|b| (*b, 1, 0)))
        .collect();
    keys_in_order.sort();
    let blocks: Vec<u64> = keys_in_order.iter().map(|k| k.0).collect();
    let mut rank: HashMap<Hash32, u64> = HashMap::new();
    let mut block_rank: BTreeMap<u64, u64> = BTreeMap::new();
    for (r, (b, tag, i)) in (0u64..).zip(&keys_in_order) {
        match tag {
            0 => _ = rank.insert(observed[*i].key, r),
            _ => _ = block_rank.insert(*b, r),
        }
    }
    let v2_orders: BTreeMap<Nid, Vec<(u64, Merged)>> = timelines
        .into_iter()
        .map(|(n, t)| (n, t.into_iter().map(|(b, m)| (block_rank[&b], m)).collect()))
        .collect();
    let cobs: Vec<_> = observed
        .into_iter()
        .filter(|o| kind_is(o, Kind::Cob))
        .collect();
    let pin_order = order(&rank, &pin_rev.effective.key)?;
    // Author keys and the delegates' refs are independent reads: fetch them together.
    let (keys, (votes, pushes)) = futures_util::future::try_join(
        cob::peer_keys_with(a, &cobs, &view.resolved),
        ref_events(a, view, &rank, pin_order, &v2_orders),
    )
    .await?;
    let ops = verify_ops(&view.rid, &keys, &cobs);
    let source = source(&pin, &ops)?;
    let seed = canonical_refs(&pin_rev.body, &votes)
        .refs
        .into_iter()
        .collect();
    let pops = ops
        .iter()
        .map(|o| Ok(program_op(o, order(&rank, &o.arkiv_order.key)?)))
        .collect::<Result<Vec<_>>>()?;
    // Proofs are verified once per receipt and cached next to the git cache.
    let cache_path = git.dir().map(|d| d.join("arkiv-proofs.json"));
    let mut cache = cache_path
        .as_deref()
        .map(proofs::ProofCache::load)
        .unwrap_or_default();
    let pops = proofs::attach(pops, &mut cache);
    if let Some(p) = &cache_path {
        cache.save(p)?;
    }
    let mut events: Vec<Event> = pops.into_iter().map(Event::Op).collect();
    events.extend(pushes.into_iter().map(Event::Push));
    events.sort_by_key(Event::order);
    let repo = RepoInfo {
        rid: view.rid,
        delegates: head.body.delegates.clone(),
        quorum: head.body.threshold,
        name: head.body.name.clone(),
    };
    let mut e = Enforced {
        engine: pin.engine.clone(),
        source,
        repo,
        seed,
        events,
        blocks,
        outcome: Outcome {
            state_json: String::new(),
            accepted: vec![],
            rejected: vec![],
            obligations: vec![],
            logs: vec![],
            heads: BTreeMap::new(),
        },
        checkpoint: None,
    };
    let (cached, started) = (load_checkpoint(git), std::time::Instant::now());
    (e.outcome, e.checkpoint) = fold(&e, vec![], git, cached.as_ref())?;
    if std::env::var_os("CHARTER_TRACE").is_some_and(|v| !v.is_empty()) {
        eprintln!(
            "chart-trace: fold {} events (checkpoint had {:?}) {} ms",
            e.events.len(),
            cached.as_ref().map(|c| c.events),
            started.elapsed().as_millis()
        );
    }
    save_checkpoint(git, e.checkpoint.as_ref());
    Ok(Some(e))
}

/// Refuses `pending` with `E_PROGRAM_REJECTED` if `view` enforces a program, `nid` is a
/// delegate, and the program rejects one of the ref moves the push signs.
pub fn check_push(view: &View, nid: &Nid, pending: &PendingPush, git: &Git) -> Result<()> {
    if view.program.is_none() || !view.delegates().contains(nid) {
        return Ok(());
    }
    let new = RefsBody::decode(&Envelope::decode(&unhex(&pending.refs)?)?.body)?;
    check_refs(view, nid, &new, git)
}

/// Like [`check_push`] for a push that would leave `nid`'s refs at `new` (format v2).
pub fn check_refs(view: &View, nid: &Nid, new: &RefsBody, git: &Git) -> Result<()> {
    let old = view.peers.get(nid).and_then(|p| p.refs_body());
    check_refs_from(view, nid, old, new, git)
}

/// [`check_refs`] from refs `old` read after the view (a re-read of `nid`'s heads).
pub fn check_refs_from(
    view: &View,
    nid: &Nid,
    old: Option<&RefsBody>,
    new: &RefsBody,
    git: &Git,
) -> Result<()> {
    let Some(e) = &view.program else {
        return Ok(());
    };
    if !view.delegates().contains(nid) {
        return Ok(());
    }
    e.check(ref_updates(*nid, old, new, PENDING_ORDER), git)
}

#[cfg(test)]
mod tests {
    use super::*;
    use charter_core::RefEntry;

    fn refs(entries: &[(&str, u8)]) -> RefsBody {
        RefsBody {
            rid: [0; 32],
            object_hash: 1,
            refs: entries
                .iter()
                .map(|(n, b)| RefEntry {
                    name: (*n).into(),
                    oid: vec![*b; 20],
                })
                .collect(),
        }
    }

    #[test]
    fn ancestry_is_undecided_while_a_commit_between_is_missing() {
        let d = tempfile::tempdir().expect("tmp");
        let sh = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                ])
                .args(args)
                .current_dir(d.path())
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout)
                .expect("utf8")
                .trim()
                .to_owned()
        };
        sh(&["init", "-q"]);
        let mut c = vec![];
        for i in 0..3 {
            sh(&[
                "commit",
                "-q",
                "--no-verify",
                "--allow-empty",
                "-m",
                &format!("chore: c{i}"),
            ]);
            c.push(hex::decode(sh(&["rev-parse", "HEAD"])).expect("hex"));
        }
        let git = Git::work(Some(d.path()));
        assert_eq!(is_ancestor(&git, &c[0], &c[2]), Some(true));
        assert_eq!(is_ancestor(&git, &c[2], &c[0]), Some(false));
        assert_eq!(is_ancestor(&git, &[7; 20], &c[2]), None, "unknown commit");
        // Objects still arriving: the middle commit is not here yet.
        let mid = hex::encode(&c[1]);
        std::fs::remove_file(
            d.path()
                .join(format!(".git/objects/{}/{}", &mid[..2], &mid[2..])),
        )
        .expect("remove");
        assert_eq!(is_ancestor(&git, &c[0], &c[2]), None, "not a frozen false");
    }

    #[test]
    fn ref_updates_cover_new_moved_and_deleted_refs() {
        let old = refs(&[("refs/heads/a", 1), ("refs/heads/b", 2), ("refs/tags/t", 3)]);
        let new = refs(&[
            ("refs/heads/a", 1),
            ("refs/heads/b", 4),
            ("refs/heads/c", 5),
        ]);
        let u = ref_updates([9; 32], Some(&old), &new, 7);
        let got: Vec<_> = u
            .iter()
            .map(|u| {
                (
                    u.name.as_str(),
                    u.old.first().copied(),
                    u.new.first().copied(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("refs/heads/b", Some(2), Some(4)),
                ("refs/heads/c", None, Some(5)),
                ("refs/tags/t", Some(3), None),
            ]
        );
        assert!(u.iter().all(|u| u.arkiv_order == 7 && u.author == [9; 32]));
        assert_eq!(ref_updates([9; 32], None, &new, 1).len(), 3);
    }

    #[test]
    fn due_order_becomes_since_block() {
        let blocks = [10, 11, 15];
        let ob = since_block(json!({"key": "k", "due_order": 2}), &blocks);
        assert_eq!(ob["since_block"], 15);
        let kept = since_block(json!({"due_order": 2, "since_block": 3}), &blocks);
        assert_eq!(kept["since_block"], 3, "an explicit since_block wins");
        assert!(
            since_block(json!({"due_order": 9}), &blocks)
                .get("since_block")
                .is_none()
        );
    }
}
