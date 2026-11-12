//! Reading a private repo's group state from Arkiv: join from the one Welcome for one of
//! this member's key packages that an admin attested, then apply attested admin commits in
//! Arkiv order, one epoch at a time. Also decides, for any rid, whether it is public,
//! private and readable, or private and closed (`E_NOT_MEMBER`).

use std::collections::{BTreeMap, BTreeSet};

use charter_crypto::PublicKey;

use charter_core::{AttrValue, ErrorCode, IdBody, Nid, Observed, Rid};
use charter_store::{Arkiv, EntityData, Pred, Query};
use charter_sync::{Config, Keys, Result, SyncError, observe, view};

use crate::auth::{
    self, CTX_WELCOME, CommitAtt, att_msg, bad, open_att, parse_commit_att, verifies,
};
use crate::keyring::{FORMAT, Keyring, Role, not_member};
use crate::mls::Mls;
use crate::msg::{
    T_COMMIT, T_GROUP, T_PUB, T_WELCOME, bytes, find, open_box, rid_pred, tag, u64_of,
};
use crate::seal::{Key, blind};
use crate::{Sealed, entity, revoked};

/// This member's MLS state and up-to-date keyring for `rid` (`None`: not a member). Reads
/// only: it never sends (a revoked device's leaf is removed by an explicit write,
/// [`revoked::remove_revoked`]; an admin's keyring counts such leaves in `revoked_leaves`).
pub async fn refresh<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
) -> Result<(Mls, Option<Keyring>)> {
    let _held = crate::lock::lock(&cfg.home).await?;
    let (mls, ring, _) = refresh_in(a, cfg, keys, rid).await?;
    Ok((mls, ring))
}

/// [`refresh`] under a home lock the caller holds, with the repo's device records read in
/// the same query ([`revoked::device_records`]).
pub(crate) async fn refresh_in<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
) -> Result<(Mls, Option<Keyring>, Vec<EntityData>)> {
    let mls = Mls::open(&cfg.home, keys.nid())?;
    let mut ring = Keyring::load(&cfg.home, rid)?;
    if ring.is_none() {
        ring = join(a, &mls, rid).await?;
    }
    let mut revokes = vec![];
    if let Some(r) = ring.as_mut() {
        let mut member;
        (member, revokes) = apply_commits(a, &mls, keys, r).await?;
        if !member {
            member = rejoin(a, &mls, keys, r).await?;
        }
        r.removed = !member;
        if member && r.role(&keys.nid()) == Some(Role::Admin) && !revokes.is_empty() {
            r.revoked_leaves = revoked::candidates(&mls.load(&r.group)?, &revokes).len();
        }
        // An epoch end past the next epoch, or past the current one while still a member,
        // came from a commit nobody applied (a keyring saved before 0.6.0 kept any).
        let open = r.current()?.0 + u64::from(!member);
        r.rotated.retain(|e, _| *e <= open);
        check_history(r)?;
        r.save(&cfg.home)?;
        mls.save()?;
    }
    Ok((mls, ring, revokes))
}

/// After this member was removed: a welcome from a later re-add replaces `ring` (and the
/// stale MLS group); without one, `ring` keeps the old epochs for reading history but no
/// role, so nothing acts on the role it held before. Returns whether it is a member again.
async fn rejoin<A: Arkiv>(a: &A, mls: &Mls, keys: &Keys, ring: &mut Keyring) -> Result<bool> {
    if let Some(mut fresh) = join(a, mls, &ring.rid).await?
        && fresh.current()?.0 > ring.current()?.0
        && apply_commits(a, mls, keys, &mut fresh).await?.0
    {
        *ring = fresh;
        return Ok(true);
    }
    ring.roles.remove(&hex::encode(keys.nid()));
    Ok(false)
}

/// Joins `rid`'s group. Every Welcome for one of our key packages is evaluated; exactly one
/// distinct group must verify ([`verify_welcome`]), so a Welcome someone else made for a
/// published key package can neither win a race nor hide the real one.
async fn join<A: Arkiv>(a: &A, mls: &Mls, rid: &Rid) -> Result<Option<Keyring>> {
    let mut rows = vec![];
    for kp in &mls.kps {
        let found = find(a, T_WELCOME, vec![observe::b32("ag.kp", kp)]).await?;
        rows.extend(found.into_iter().map(|w| (*kp, w)));
    }
    let mut ok: Vec<(usize, (Keyring, Vec<Observed>))> = vec![];
    let mut refused = None;
    for (i, (kp, w)) in rows.iter().enumerate() {
        let snap = mls.snapshot()?;
        let verdict = verify_welcome(a, mls, rid, kp, w).await;
        mls.restore(snap)?;
        match verdict {
            Ok(Some(ring)) => ok.push((i, ring)),
            Ok(None) => {}
            Err(e) => refused = Some(e),
        }
    }
    // Each verified welcome continues the canonical commit record ([`on_record`]), so
    // welcomes at different epochs are all genuine (a re-add, or a second add of the same
    // key package): the newest wins. Two at one epoch naming different groups cannot both
    // be canonical, so they are refused.
    let epoch_of = |r: &Keyring| r.current().map_or(0, |c| c.0);
    let newest = ok.iter().map(|(_, (r, _))| epoch_of(r)).max();
    ok.retain(|(_, (r, _))| Some(epoch_of(r)) == newest);
    let groups: BTreeSet<&[u8]> = ok.iter().map(|(_, (r, _))| r.group.as_slice()).collect();
    if groups.len() > 1 {
        return Err(SyncError::coded(
            ErrorCode::Equivocation,
            "several admin-attested welcomes for your key package name different groups; \
             ask the repo's admins which is real before you join",
        ));
    }
    let Some((i, (mut ring, _))) = ok.into_iter().next() else {
        return refused.map_or(Ok(None), Err);
    };
    let body = bytes(&rows[i].1, "ag.body").unwrap_or_default();
    let staged = mls.stage(body)?.ok_or_else(|| not_member(rid))?;
    let g = mls.finish(staged)?;
    ring.group = g.group_id().as_slice().to_vec();
    // Not handed to the clone as prefetched rows: the clone must read them itself, so its
    // memo records the identity page and later fetches replay it instead of reading again.
    Ok(Some(ring))
}

/// The keyring a Welcome hands over, if it is for us and verifies: its lockbox opens under
/// its epoch key, and its attestation is signed over the Welcome and lockbox by an admin
/// whose authority chains to the root identity (whose content hash is the rid). `Ok(None)`:
/// not for us; an error: for us but unverifiable.
async fn verify_welcome<A: Arkiv>(
    a: &A,
    mls: &Mls,
    rid: &Rid,
    kp: &charter_core::Hash32,
    w: &Observed,
) -> Result<Option<(Keyring, Vec<Observed>)>> {
    let (Some(body), Some(bx)) = (bytes(w, "ag.body"), bytes(w, "ag.box")) else {
        return Ok(None);
    };
    let Some(staged) = mls.stage(body)? else {
        return Ok(None);
    };
    let (epoch, key, history) = mls.staged_key(&staged, rid)?;
    let Some(mut ring) = open_box(rid, epoch, &history, bx) else {
        return Ok(None);
    };
    let Some((signer, sig)) = bytes(w, "ag.att").and_then(|s| open_att(&key, rid, epoch, s)) else {
        return Err(SyncError::coded(
            ErrorCode::UnsupportedVersion,
            "this welcome carries no admin attestation (it was made by charter before 0.6.0 \
             or by someone else); ask an admin on 0.6.0 or later to add you again",
        ));
    };
    let pk = charter_sync::cob::peer_key_of(a, &signer).await?;
    let msg = att_msg(rid, epoch, body, bx, Some(kp));
    if !pk
        .as_ref()
        .is_some_and(|pk| verifies(pk, CTX_WELCOME, &msg, &sig))
    {
        return Err(bad("welcome attestation does not verify"));
    }
    ring.epochs.insert(epoch, key);
    ring.rotated.insert(epoch, w.block);
    let roles = ring.roles.clone();
    ring.record_roles(epoch, &roles);
    let Some((roots, ids)) = root_rows(a, &ring).await? else {
        return Err(bad(
            "the welcome's keys do not open the repo's root identity",
        ));
    };
    let ct = auth::commits_tag(&ring.id_key, &ring.rid);
    let records = find(a, T_COMMIT, vec![observe::b32("ag.ct", &ct)]).await?;
    let pks = BTreeMap::from([(signer, pk)]);
    if !on_record(a, &mut ring, &roots, epoch, Some(&signer), &records, pks).await? {
        return Err(bad(
            "this welcome does not continue the repo's commit history: it was not made by \
             an admin of the current group",
        ));
    }
    Ok(Some((ring, ids)))
}

/// Whether the welcome for epoch `epoch` (attested by `signer`) continues the commits on
/// record: the [`canonical`] commits reach `epoch`, and each one's admin set and epoch
/// authenticator (which only that epoch's key holders can compute) match `ring`. So
/// `ring`'s whole admin history up to `epoch` is the one the admins committed to, a removed
/// admin is out of it, and a welcome she forges with keys she never held fails at the
/// first epoch she was not in, even before any genuine welcome exists. `signer`, if given,
/// must have made the commit of `epoch`. Sets `rotated` from those commits. Reads `rows`
/// (the commit rows); costs one key lookup per admin who committed (`pks` holds those
/// already looked up).
async fn on_record<A: Arkiv>(
    a: &A,
    ring: &mut Keyring,
    roots: &[Nid],
    epoch: u64,
    signer: Option<&Nid>,
    rows: &[Observed],
    mut pks: BTreeMap<Nid, Option<PublicKey>>,
) -> Result<bool> {
    let by_ep = records(rows, &ring.id_key, &ring.rid);
    let roots: BTreeSet<Nid> = roots.iter().copied().collect();
    let chain = canonical(a, &mut pks, &ring.rid, &by_ep, &roots, epoch).await?;
    if chain.last().map(|(k, _, _)| *k) != Some(epoch) {
        return Ok(false);
    }
    for (k, _, att) in &chain {
        let (Some(key), Some(adm)) = (ring.epochs.get(k), ring.adm_log.get(k)) else {
            return Ok(false);
        };
        let pinned = att.admins == *adm && att.auth == auth::epoch_auth(key, &ring.rid);
        if !pinned || (*k == epoch && signer.is_some_and(|s| att.signer != *s)) {
            return Ok(false);
        }
    }
    for (k, c, _) in chain {
        ring.rotated.insert(k, c.block);
    }
    Ok(true)
}

pub(crate) type Records<'c> = BTreeMap<u64, Vec<(&'c Observed, CommitAtt)>>;

/// The commit rows of `rows` whose public attestation opens under the generation-0 id key
/// `gen0`, by epoch, in Arkiv order.
pub(crate) fn records<'c>(rows: &'c [Observed], gen0: &Key, rid: &Rid) -> Records<'c> {
    let mut by_ep: Records = BTreeMap::new();
    for c in rows {
        let Some(ep) = u64_of(c, "ag.ep") else {
            continue;
        };
        if let Some(att) = bytes(c, "ag.att").and_then(|b| parse_commit_att(b, gen0, rid, ep)) {
            by_ep.entry(ep).or_default().push((c, att));
        }
    }
    by_ep
}

/// One canonical commit: its epoch, row and attestation.
type Canon<'c> = (u64, &'c Observed, &'c CommitAtt);

/// The canonical commit of each epoch, from the first one a root delegate signed, up to
/// `upto`: at each epoch the first in Arkiv order validly signed by an admin of the epoch
/// before (the root's delegates before the first one), whose attested admin set names the
/// admins of its own epoch. No commit makes epoch 0, so a record naming it is junk. Stops
/// at the first epoch with none. Needs no epoch key, so a grant reader can follow it too.
async fn canonical<'c, A: Arkiv>(
    a: &A,
    pks: &mut BTreeMap<Nid, Option<PublicKey>>,
    rid: &Rid,
    by_ep: &'c Records<'c>,
    roots: &BTreeSet<Nid>,
    upto: u64,
) -> Result<Vec<Canon<'c>>> {
    let mut chain: Vec<Canon<'c>> = vec![];
    for (ep, cands) in by_ep.range(1..=upto) {
        if let Some((c, att)) = first_signed(a, pks, rid, *ep, cands, roots).await? {
            chain.push((*ep, c, att));
            break;
        }
    }
    while let Some(&(k, _, att)) = chain.last()
        && k < upto
    {
        let cands = by_ep.get(&(k + 1)).map_or(&[][..], Vec::as_slice);
        let admins = nids(Some(&att.admins));
        let Some((c, next)) = first_signed(a, pks, rid, k + 1, cands, &admins).await? else {
            break;
        };
        chain.push((k + 1, c, next));
    }
    Ok(chain)
}

/// The first of `cands` (one epoch's commits, in Arkiv order) validly signed by one of
/// `admins`; looks each signer's key up once.
pub(crate) async fn first_signed<'c, A: Arkiv>(
    a: &A,
    pks: &mut BTreeMap<Nid, Option<PublicKey>>,
    rid: &Rid,
    epoch: u64,
    cands: &'c [(&'c Observed, CommitAtt)],
    admins: &BTreeSet<Nid>,
) -> Result<Option<(&'c Observed, &'c CommitAtt)>> {
    for (c, att) in cands {
        if !admins.contains(&att.signer) {
            continue;
        }
        if let std::collections::btree_map::Entry::Vacant(e) = pks.entry(att.signer) {
            e.insert(charter_sync::cob::peer_key_of(a, &att.signer).await?);
        }
        let (body, bx) = (
            bytes(c, "ag.body").unwrap_or_default(),
            bytes(c, "ag.box").unwrap_or_default(),
        );
        if pks[&att.signer]
            .as_ref()
            .is_some_and(|pk| auth::commit_signed(pk, att, rid, epoch, body, bx, &c.creator))
        {
            return Ok(Some((*c, att)));
        }
    }
    Ok(None)
}

pub(crate) fn nids(set: Option<&BTreeSet<String>>) -> BTreeSet<Nid> {
    set.into_iter()
        .flatten()
        .filter_map(|n| hex::decode(n).ok()?.try_into().ok())
        .collect()
}

/// The root identity's delegates and the identity rows `ring` opens, or `None` when no
/// row opens to the root (content hash = rid, revision 0).
pub async fn root_rows<A: Arkiv>(
    a: &A,
    ring: &Keyring,
) -> Result<Option<(Vec<Nid>, Vec<Observed>)>> {
    let rows = observe::all(
        &Sealed::new(a, ring.clone()),
        view::identity_filter(&ring.rid),
    )
    .await?;
    Ok(root_of(&ring.rid, &rows).map(|d| (d, rows)))
}

fn root_of(rid: &Rid, rows: &[Observed]) -> Option<Vec<Nid>> {
    rows.iter().find_map(|o| {
        charter_core::open::<IdBody>(o)
            .ok()
            .filter(|e| e.h == *rid && e.body.is_root())
            .map(|e| e.body.delegates)
    })
}

/// Applies attested admin commits for the next epochs; any other commit is skipped and
/// undone. This member's own pending commit ([`crate::group::finish`]) is recognised by its
/// lockbox opening under the pending epoch key, and merged only if it comes first; one from
/// another admin coming first drops it. Stops once this member is removed: it then holds no
/// key for later epochs, and the commit that removed it (an admin's, as MLS authenticates)
/// ends its last epoch.
/// Returns whether it is still a member, and the repo's device records (revokes and
/// delegations, [`revoked::device_records`]), read in the same query as the commits. The
/// id-key generation this member's own pending removal starts is read in that query too,
/// so the refresh after it costs no second query; its own commit needs no key lookup.
pub(crate) async fn apply_commits<A: Arkiv>(
    a: &A,
    mls: &Mls,
    keys: &Keys,
    ring: &mut Keyring,
) -> Result<(bool, Vec<EntityData>)> {
    let mut g = mls.load(&ring.group)?;
    loop {
        let mut probe = ring.clone();
        if let Some((ep, _, id, _)) = mls.pending_keys(&g, &ring.rid)? {
            probe.id_rot.entry(ep).or_insert(id);
        }
        let queried: Vec<Key> = probe.id_keys().copied().collect();
        let commits = Pred::And(vec![
            observe::kind(charter_core::Kind::Mls),
            Pred::eq(charter_store::attr("ag.t"), T_COMMIT),
            rid_pred(&probe),
        ]);
        let q = Query::from(Pred::Or(vec![commits, revoked::device_records(&probe)]));
        let raw = charter_store::paging::offset_all(a, &q, observe::full_select()).await?;
        let (devices, raw): (Vec<_>, Vec<_>) = raw.into_iter().partition(revoked::is_device_row);
        let rows = observe::to_observed(raw);
        derive_history(ring, &rows, ring.format < FORMAT, &BTreeMap::new());
        for c in &rows {
            if !g.is_active() {
                break;
            }
            let next = g.epoch().as_u64() + 1;
            let (Some(body), Some(bx)) = (bytes(c, "ag.body"), bytes(c, "ag.box")) else {
                continue;
            };
            if u64_of(c, "ag.ep") != Some(next) {
                continue;
            }
            let snap = mls.snapshot()?;
            let sender = match ours(mls, &g, &ring.rid, next, bx)? {
                true => mls.merge_pending(&mut g).map(|()| Some(mls.nid())),
                false => {
                    let admins = ring.clone();
                    let is_admin = |n: &Nid| admins.role(n) == Some(Role::Admin);
                    mls.process(&mut g, body, is_admin)
                }
            };
            let Ok(Some(sender)) = sender else {
                mls.restore(snap)?;
                g = mls.load(&ring.group)?;
                continue;
            };
            if !g.is_active() {
                ring.rotated.entry(next).or_insert(c.block);
                break;
            }
            let (epoch, key) = mls.epoch_key(&g, &ring.rid)?;
            let attested = match open_box(&ring.rid, epoch, &key, bx) {
                Some(newer) => commit_attested(a, keys, ring, &newer, &sender, epoch, &key, c)
                    .await?
                    .then_some(newer),
                None => None,
            };
            let Some(newer) = attested else {
                mls.restore(snap)?;
                g = mls.load(&ring.group)?;
                continue;
            };
            ring.epochs.insert(epoch, key);
            ring.rotated.insert(epoch, c.block);
            ring.merge(&newer);
            let roles = ring.roles.clone();
            ring.record_roles(epoch, &roles);
        }
        // A removal started a new id-key generation not read yet: its later commits use it.
        if ring.id_keys().all(|k| queried.contains(k)) || !g.is_active() {
            return Ok((g.is_active(), devices));
        }
    }
}

/// Whether lockbox `bx` of a commit for epoch `next` is this member's pending commit's: it
/// opens under the key only that pending commit derives.
fn ours(
    mls: &Mls,
    g: &openmls::prelude::MlsGroup,
    rid: &Rid,
    next: u64,
    bx: &[u8],
) -> Result<bool> {
    Ok(mls
        .pending_keys(g, rid)?
        .is_some_and(|(ep, key, ..)| ep == next && open_box(rid, ep, &key, bx).is_some()))
}

/// Whether commit row `c` carries `sender`'s public attestation of its bytes and lockbox,
/// `sender` was an admin of the epoch before, and the attested admin digest and epoch
/// authenticator are those of `newer` (its lockbox) and `key`, so joiners replaying the
/// record see what members applied. A commit without one is accepted only while the repo
/// is still in the legacy format, and the commit that migrates it only from a root
/// delegate (one identity query, once per repo).
#[allow(clippy::too_many_arguments)]
async fn commit_attested<A: Arkiv>(
    a: &A,
    keys: &Keys,
    ring: &Keyring,
    newer: &Keyring,
    sender: &Nid,
    epoch: u64,
    key: &Key,
    c: &Observed,
) -> Result<bool> {
    let (body, bx) = (
        bytes(c, "ag.body").unwrap_or_default(),
        bytes(c, "ag.box").unwrap_or_default(),
    );
    let att = bytes(c, "ag.att").and_then(|b| parse_commit_att(b, &ring.id_key, &ring.rid, epoch));
    let Some(att) = att else {
        return Ok(ring.format < FORMAT);
    };
    let prev = epoch.saturating_sub(1);
    let pinned = newer.adm_log.get(&epoch) == Some(&att.admins)
        && att.auth == auth::epoch_auth(key, &ring.rid);
    if att.signer != *sender || !pinned {
        return Ok(false);
    }
    let was_admin = match ring.adm_log.get(&prev) {
        Some(set) => set.contains(&hex::encode(sender)),
        // The migrating commit: a joiner's replay starts at a root delegate's commit.
        None => {
            ring.role_at(prev, sender) == Some(Role::Admin)
                && root_rows(a, ring)
                    .await?
                    .is_some_and(|(roots, _)| roots.contains(sender))
        }
    };
    if !was_admin {
        return Ok(false);
    }
    // This home's own commit: its key is at hand, so the same check needs no lookup.
    let looked_up;
    let pk = match *sender == keys.nid() {
        true => Some(keys.pk()),
        false => {
            looked_up = charter_sync::cob::peer_key_of(a, sender).await?;
            looked_up.as_ref()
        }
    };
    Ok(pk.is_some_and(|pk| auth::commit_signed(pk, &att, &ring.rid, epoch, body, bx, &c.creator)))
}

/// Where each epoch held started, from commit rows already read: the earliest row naming
/// that epoch whose lockbox opens under its key (nobody holds that key before the commit
/// lands). A row naming an epoch not held places nothing: anyone can write one. A key in a
/// published grant is public, and a byte copy of a commit opens too, so a row also needs a
/// creator binding: an attested row counts only if its attestation verifies, with the row's
/// creator, under a key in `pks` (looked up already, so no query); a row without one only
/// while `legacy` (the repo has no attested commit, or a keyring written before 0.6.0 is
/// filled in). Also fills the role history of a keyring written before 0.6.0 from those
/// lockboxes. Costs no query.
fn derive_history(
    ring: &mut Keyring,
    rows: &[Observed],
    legacy: bool,
    pks: &BTreeMap<Nid, Option<PublicKey>>,
) {
    for c in rows {
        let Some(ep) = u64_of(c, "ag.ep") else {
            continue;
        };
        if ring.rotated.contains_key(&ep) {
            continue;
        }
        let (Some(k), Some(bx)) = (ring.epochs.get(&ep), bytes(c, "ag.box")) else {
            continue;
        };
        if !bound(ring, c, ep, legacy, pks) {
            continue;
        }
        let Some(b) = open_box(&ring.rid, ep, k, bx) else {
            continue;
        };
        ring.rotated.insert(ep, c.block);
        if !ring.role_log.contains_key(&ep) {
            ring.record_roles(ep, &b.roles);
        }
    }
    let Some(first) = ring.epochs.keys().next().copied() else {
        return;
    };
    if ring.role_log.range(..=first).next().is_none() {
        // A legacy keyring's first epoch: its admins are the earliest admins on record.
        let seed: std::collections::BTreeMap<String, Role> = match ring.role_log.values().next() {
            Some(m) => m
                .iter()
                .filter(|(_, r)| **r == Some(Role::Admin))
                .map(|(n, _)| (n.clone(), Role::Admin))
                .collect(),
            None => ring.roles.clone(),
        };
        ring.record_roles(first, &seed);
    }
}

/// Whether commit row `c` for epoch `ep` may place that epoch ([`derive_history`]).
fn bound(
    ring: &Keyring,
    c: &Observed,
    ep: u64,
    legacy: bool,
    pks: &BTreeMap<Nid, Option<PublicKey>>,
) -> bool {
    let Some(raw) = bytes(c, "ag.att") else {
        return legacy;
    };
    let Some(att) = parse_commit_att(raw, &ring.id_key, &ring.rid, ep) else {
        return false;
    };
    let (body, bx) = (
        bytes(c, "ag.body").unwrap_or_default(),
        bytes(c, "ag.box").unwrap_or_default(),
    );
    pks.get(&att.signer)
        .and_then(Option::as_ref)
        .is_some_and(|pk| auth::commit_signed(pk, &att, &ring.rid, ep, body, bx, &c.creator))
}

/// Fails loudly when some epoch's end cannot be placed (its commit expired before this
/// keyring learned it), instead of reading that epoch with no bound on late writes.
fn check_history(ring: &Keyring) -> Result<()> {
    let (Some(first), Ok((cur, _))) = (ring.epochs.keys().next(), ring.current()) else {
        return Ok(());
    };
    let missing: Vec<u64> = (first + 1..=cur)
        .filter(|e| !ring.rotated.contains_key(e))
        .collect();
    match missing.is_empty() {
        true => Ok(()),
        false => Err(SyncError::coded(
            ErrorCode::NotLive,
            format!(
                "cannot place the start of epochs {missing:?} of this private repo (their \
                 commits expired); ask an admin to remove and re-add you"
            ),
        )),
    }
}

/// How to read `rid`: `None` for a public (or unknown) repo, a keyring for a private repo
/// this peer can read (as a member, or through published epoch grants), else `E_NOT_MEMBER`.
///
/// A verified public root is remembered on disk: `rid` hashes the root body, which records
/// the visibility, so a public repo can never turn private and later calls send no query.
pub async fn ring_for<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: Option<&Keys>,
    rid: &Rid,
) -> Result<Option<Keyring>> {
    if public_marker(&cfg.home, rid).exists() {
        return Ok(None);
    }
    let member = keys.filter(|_| matches!(Keyring::load(&cfg.home, rid), Ok(Some(_))));
    if let Some(keys) = member
        && let (_, Some(ring)) = refresh(a, cfg, keys, rid).await?
    {
        return Ok(Some(ring));
    }
    // The view's identity query: on a public repo the view reuses these rows.
    let ids = observe::all(a, view::identity_filter(rid)).await?;
    if ids.iter().any(|o| is_public_root(o, rid)) {
        remember_public(&cfg.home, rid)?;
        view::prefetched_identity(rid, ids);
        return Ok(None);
    }
    if let Some(keys) = keys.filter(|_| member.is_none())
        && let (_, Some(ring)) = refresh(a, cfg, keys, rid).await?
    {
        return Ok(Some(ring));
    }
    // A public identity wins, so a forged marker cannot close a public repo.
    if ids
        .iter()
        .any(|o| o.attr("ag.kind") == Some(&AttrValue::Str("id".into())))
    {
        view::prefetched_identity(rid, ids);
        return Ok(None);
    }
    // The marker and the grants in one query.
    let types = Pred::Or(
        [T_GROUP, T_PUB]
            .map(|t| Pred::eq(charter_store::attr("ag.t"), t))
            .to_vec(),
    );
    let marked = find_any(a, types, rid).await?;
    let is = |o: &Observed, t: &str| o.attr("ag.t") == Some(&AttrValue::Str(t.into()));
    if !marked.iter().any(|o| is(o, T_GROUP)) {
        return Ok(None);
    }
    let grants: Vec<Observed> = marked.into_iter().filter(|o| is(o, T_PUB)).collect();
    published(a, rid, &grants)
        .await?
        .map(Some)
        .ok_or_else(|| not_member(rid))
}

async fn find_any<A: Arkiv>(a: &A, types: Pred, rid: &Rid) -> Result<Vec<Observed>> {
    let f = Pred::And(vec![
        observe::kind(charter_core::Kind::Mls),
        types,
        observe::b32("ag.tag", &tag(rid)),
    ]);
    observe::all(a, f).await
}

/// The root identity of `rid` itself, saying public: its content hash is `rid`, so no other
/// row can pass (a forged `kind = id` row with this `ag.rid` does not).
fn is_public_root(o: &Observed, rid: &Rid) -> bool {
    charter_core::open::<IdBody>(o)
        .is_ok_and(|e| e.h == *rid && e.body.is_root() && e.body.visibility == 0)
}

fn public_marker(home: &std::path::Path, rid: &Rid) -> std::path::PathBuf {
    home.join("private")
        .join(format!("{}.public", hex::encode(rid)))
}

fn remember_public(home: &std::path::Path, rid: &Rid) -> Result<()> {
    let p = public_marker(home, rid);
    let dir = p.parent().unwrap_or(home);
    std::fs::create_dir_all(dir).map_err(|e| SyncError::io(dir.display(), e))?;
    std::fs::write(&p, b"").map_err(|e| SyncError::io(p.display(), e))
}

/// Most distinct id keys tried for the root in one query (junk grants cannot widen it).
const MAX_ID_CANDIDATES: usize = 16;

/// A read-only keyring from published grants that authenticate. The grant whose keys open
/// the root identity (content hash = rid) gives the generation-0 id key, which opens the
/// public commit attestations; the [`canonical`] commit chain they form (no epoch key
/// needed) fixes each epoch's authenticator and admin set, and a grant counts only if its
/// key is the one its epoch's commit authenticates and its signer was an admin of the
/// epoch before. So a root delegate removed from the group cannot publish a grant for an
/// epoch it did not hold, and a grant opens only its own epoch. A repo with no attested
/// commit (legacy, or never changed) falls back to grants signed by a root delegate.
/// Costs the identity query the view then skips and one commit query, plus a key lookup
/// per committing admin that signed no grant.
async fn published<A: Arkiv>(a: &A, rid: &Rid, grants: &[Observed]) -> Result<Option<Keyring>> {
    let cands: Vec<(u64, auth::SignedGrant)> = grants
        .iter()
        .filter_map(|o| {
            Some((
                u64_of(o, "ag.ep")?,
                auth::decode_grant(bytes(o, "ag.body")?)?,
            ))
        })
        .collect();
    let mut ids: Vec<Key> = vec![];
    for (_, g) in &cands {
        if !ids.contains(&g.id_key) && ids.len() < MAX_ID_CANDIDATES {
            ids.push(g.id_key);
        }
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let blinded: Vec<Pred> = ids
        .iter()
        .map(|k| observe::b32("ag.rid", &blind(k, "rid", rid)))
        .collect();
    let kinds = Pred::Or(
        [charter_core::Kind::Id, charter_core::Kind::IdAcc]
            .map(observe::kind)
            .to_vec(),
    );
    let q = Query::from(Pred::And(vec![Pred::Or(blinded.clone()), kinds]));
    let raw = charter_store::paging::offset_all(a, &q, observe::full_select()).await?;
    let Some((root, roots)) = root_grant(rid, &cands, &raw) else {
        return Ok(None);
    };
    // Attested commits by their generation-0 tag, legacy ones by a granted id key.
    let ct = observe::b32("ag.ct", &auth::commits_tag(&root.id_key, rid));
    let commits = find(a, T_COMMIT, vec![Pred::Or([vec![ct], blinded].concat())]).await?;
    let signed: Vec<&(u64, auth::SignedGrant)> = cands
        .iter()
        .filter(|(ep, g)| auth::grant_signed(g, rid, *ep, &roots))
        .collect();
    let by_ep = records(&commits, &root.id_key, rid);
    let (mut ring, pks) = match by_ep.is_empty() {
        true => (legacy_grants(root, &signed), BTreeMap::new()),
        false => attested_grants(a, rid, root, &roots, &signed, &by_ep).await?,
    };
    derive_history(&mut ring, &commits, by_ep.is_empty(), &pks);
    let rows = crate::arkiv::open_all(&ring, raw);
    view::prefetched_identity(rid, observe::to_observed(rows));
    Ok(Some(ring))
}

/// Adds grant `g` for epoch `ep` (its keys and the roles it signs) to `ring`.
fn take_grant(ring: &mut Keyring, ep: u64, g: &auth::SignedGrant) {
    ring.epochs.insert(ep, g.key);
    if g.id_key != ring.id_key {
        ring.id_rot.insert(ep, g.id_key);
    }
    let roles = g.roles.iter().map(|(n, r)| (hex::encode(n), *r)).collect();
    ring.record_roles(ep, &roles);
}

/// Before any attested commit, every root-delegate grant counts (the first per epoch).
fn legacy_grants(mut ring: Keyring, signed: &[&(u64, auth::SignedGrant)]) -> Keyring {
    for (ep, g) in signed {
        if !ring.epochs.contains_key(ep) {
            take_grant(&mut ring, *ep, g);
        }
    }
    ring
}

/// `ring` (the root grant's) with each grant of `signed` whose key the [`canonical`]
/// commit of its epoch authenticates, whose admins are the admin set that commit attests
/// (the roles below admin rest on the signer's word: no commit attests them), and whose
/// signer was an admin of the epoch before, and with the start of every epoch on record. A
/// grant for an epoch before the first attested commit (a legacy one) needs only the root
/// delegate's signature. Also returns the keys looked up. Costs a key lookup per committing
/// admin that signed no grant.
async fn attested_grants<A: Arkiv>(
    a: &A,
    rid: &Rid,
    mut ring: Keyring,
    roots: &[Nid],
    signed: &[&(u64, auth::SignedGrant)],
    by_ep: &Records<'_>,
) -> Result<(Keyring, BTreeMap<Nid, Option<PublicKey>>)> {
    let signer = |g: &auth::SignedGrant| {
        let (pk, _) = g.sig.as_ref()?;
        PublicKey::from_bytes(charter_crypto::Alg::MlDsa65Ed25519, pk).ok()
    };
    let mut pks: BTreeMap<Nid, Option<PublicKey>> = signed
        .iter()
        .filter_map(|(_, g)| signer(g))
        .map(|pk| (pk.nid(), Some(pk)))
        .collect();
    let root_set: BTreeSet<Nid> = roots.iter().copied().collect();
    let chain = canonical(a, &mut pks, rid, by_ep, &root_set, u64::MAX).await?;
    let base = chain.first().map(|(k, _, _)| *k);
    let admins_before = |ep: u64| match chain.iter().find(|(k, _, _)| k + 1 == ep) {
        Some((_, _, att)) => nids(Some(&att.admins)),
        None => root_set.clone(),
    };
    for (ep, g) in signed {
        if ring.epochs.contains_key(ep) {
            continue;
        }
        let counts = match base {
            Some(b) if *ep >= b => chain.iter().any(|(k, _, att)| {
                k == ep
                    && att.auth == auth::epoch_auth(&g.key, rid)
                    && admins_of(g) == att.admins
                    && signer(g).is_some_and(|pk| admins_before(*ep).contains(&pk.nid()))
            }),
            Some(_) => true,
            None => false,
        };
        if counts {
            take_grant(&mut ring, *ep, g);
        }
    }
    for (k, c, _) in &chain {
        ring.rotated.insert(*k, c.block);
    }
    Ok((ring, pks))
}

/// The admins (hex nids) a grant's roles name.
fn admins_of(g: &auth::SignedGrant) -> BTreeSet<String> {
    g.roles
        .iter()
        .filter(|(_, r)| *r == Role::Admin)
        .map(|(n, _)| hex::encode(n))
        .collect()
}

/// The keyring of the grant whose keys open the root identity (one a root delegate also
/// signed, if any), and the root's delegates.
fn root_grant(
    rid: &Rid,
    cands: &[(u64, auth::SignedGrant)],
    raw: &[charter_store::EntityData],
) -> Option<(Keyring, Vec<Nid>)> {
    let mut opening: Vec<(bool, Keyring, Vec<Nid>)> = cands
        .iter()
        .filter_map(|(ep, g)| {
            let (ring, roots) = root_ring(rid, *ep, g, raw)?;
            Some((auth::grant_signed(g, rid, *ep, &roots), ring, roots))
        })
        .collect();
    let best = opening
        .iter()
        .position(|(signed, _, _)| *signed)
        .unwrap_or(0);
    (!opening.is_empty()).then(|| {
        let (_, ring, roots) = opening.swap_remove(best);
        (ring, roots)
    })
}

fn root_ring(
    rid: &Rid,
    ep: u64,
    g: &auth::SignedGrant,
    raw: &[charter_store::EntityData],
) -> Option<(Keyring, Vec<Nid>)> {
    let mut ring = Keyring::new(*rid, g.id_key, vec![]);
    ring.epochs.insert(ep, g.key);
    let opened = raw
        .iter()
        .map(|r| entity::open_row_unchecked(&ring, r.clone()))
        .collect();
    let roots = root_of(rid, &observe::to_observed(opened))?;
    let mut roles: std::collections::BTreeMap<String, Role> = roots
        .iter()
        .map(|n| (hex::encode(n), Role::Admin))
        .collect();
    roles.extend(g.roles.iter().map(|(n, r)| (hex::encode(n), *r)));
    ring.record_roles(ep, &roles);
    Some((ring, roots))
}
