//! The private-repo API that the CLI and, later, repo programs (through obligations) call:
//! create, key packages, add and remove members (each an MLS commit, so a new epoch and a
//! new content key), and epoch-key publication or export for sponsorware and escrow.

use std::collections::BTreeSet;

use charter_core::{Body, ErrorCode, Hash32, IdBody, Kind, Nid, Rid, ZERO};
use charter_store::{Arkiv, AttributeValue, Pred};
use charter_sync::identity::RepoMeta;
use charter_sync::push_send::first_write_ops;
use charter_sync::write::{self, entity_op, sign};
use charter_sync::{Config, Keys, Result, SyncError, observe};

use crate::arkiv::Sealed;
use crate::auth::{self, CTX_WELCOME, att_msg, bad};
use crate::join::{refresh, root_rows};
use crate::keyring::{FORMAT, Keyring, Role, not_member};
use crate::mls::{Mls, SUITE};
use crate::msg::{self, T_COMMIT, T_GROUP, T_KP, T_PUB, T_WELCOME, b32, blinded_rid_at, tag};
use crate::seal::blind;

/// `id_key || content key`: everything needed to find and open one epoch's entities.
pub type Grant = zeroize::Zeroizing<Vec<u8>>;

/// Publishes a fresh key package from the wallet for this machine's device, with this
/// identity's signature over the MLS signature key and the device it carries (`ag.pk`,
/// `ag.sig`); returns its id for `member add`.
pub async fn publish_key_package<A: Arkiv>(a: &A, cfg: &Config, keys: &Keys) -> Result<Hash32> {
    let mut mls = Mls::open(&cfg.home, keys.nid())?;
    let kp = mls.key_package()?;
    let id = charter_crypto::sha256(&kp);
    let binding = auth::sign_leaf(keys, &mls.signature_key(), &mls.device()?)?;
    mls.kps.push(id);
    mls.save()?;
    let attrs = vec![
        ("ag.h", b32(id), true),
        (
            "ag.pk",
            AttributeValue::Bytes(keys.pk().as_bytes().to_vec()),
            false,
        ),
        ("ag.sig", AttributeValue::Bytes(binding), false),
    ];
    let op = msg::op(T_KP, attrs, kp, cfg.control_lifetime)?;
    write::send(a, &keys.wallet(), &cfg.pipeline, vec![vec![op]]).await?;
    Ok(id)
}

/// Creates a private repo with this peer as its single delegate and only (admin) member.
pub async fn init<A: Arkiv>(a: &A, cfg: &Config, keys: &Keys, meta: &RepoMeta) -> Result<Rid> {
    let root = IdBody {
        rid: None,
        rev: 0,
        parent: ZERO,
        name: meta.name.clone(),
        description: meta.description.clone(),
        default_branch: meta.default_branch.clone(),
        delegates: vec![keys.nid()],
        threshold: 1,
        visibility: 1,
        lang: meta.lang.clone(),
        license: meta.license.clone(),
        nonce: Some(charter_store::random_salt().to_be_bytes()),
        program: None,
    };
    root.validate()?;
    let s = sign(keys.sk(), &root)?;
    let rid = s.h;
    let mls = Mls::open(&cfg.home, keys.nid())?;
    let g = mls.create()?;
    let (epoch, key) = mls.epoch_key(&g, &rid)?;
    let mut ring = Keyring::new(rid, mls.id_key(&g, &rid)?, g.group_id().as_slice().to_vec());
    ring.epochs.insert(epoch, key);
    ring.roles.insert(hex::encode(keys.nid()), Role::Admin);
    let roles = ring.roles.clone();
    ring.record_roles(epoch, &roles);
    ring.adm_log
        .insert(epoch, BTreeSet::from([hex::encode(keys.nid())]));
    ring.format = FORMAT;
    let mut ops = first_write_ops(keys, &rid, cfg)?;
    ops.push(entity_op::<IdBody>(&s.bytes, cfg.control_lifetime)?);
    let suite = u16::from(SUITE).to_be_bytes().to_vec();
    let marker = vec![("ag.tag", b32(tag(&rid)), true)];
    ops.push(msg::op(T_GROUP, marker, suite, cfg.control_lifetime)?);
    let sealed = Sealed::new(a, ring.clone());
    write::send_funded(
        &sealed,
        &keys.control(&rid),
        &keys.wallet(),
        &cfg.pipeline,
        vec![ops],
    )
    .await?;
    mls.save()?;
    ring.save(&cfg.home)?;
    Ok(rid)
}

/// This member's current keyring, or `E_NOT_MEMBER`.
pub async fn member_ring<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
) -> Result<(Mls, Keyring)> {
    match refresh(a, cfg, keys, rid).await? {
        (mls, Some(ring)) => Ok((mls, ring)),
        _ => Err(not_member(rid)),
    }
}

/// The current epoch and every member with its role, number of MLS leaves (devices), and
/// how many of those name no device (made before 0.6.0: a device revoke cannot find them).
pub async fn members<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
) -> Result<(u64, Vec<(Nid, Role, usize, usize)>)> {
    let (mls, ring) = member_ring(a, cfg, keys, rid).await?;
    let leaves = Mls::leaves(&mls.load(&ring.group)?);
    let list = ring
        .roles
        .iter()
        .filter_map(|(n, r)| {
            let nid: Nid = hex::decode(n).ok()?.try_into().ok()?;
            let (n, unbound) = leaves.get(&nid).copied().unwrap_or_default();
            Some((nid, *r, n, unbound))
        })
        .collect();
    Ok((ring.current()?.0, list))
}

/// Admin only: adds the owner of key package `kp_id` with `role`; returns their nid.
/// Refuses a key package not signed by the identity it names, and one whose identity is
/// already a member (use [`add_device`]), so an add never changes an existing role.
pub async fn add_member<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
    kp_id: &Hash32,
    role: Role,
) -> Result<Nid> {
    add(a, cfg, keys, rid, kp_id, Some(role)).await
}

/// Admin only: adds another device (MLS leaf) of an existing member, keeping its role.
pub async fn add_device<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
    kp_id: &Hash32,
) -> Result<Nid> {
    add(a, cfg, keys, rid, kp_id, None).await
}

async fn add<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
    kp_id: &Hash32,
    role: Option<Role>,
) -> Result<Nid> {
    let (mls, mut ring) = member_ring(a, cfg, keys, rid).await?;
    ring.require(&keys.nid(), Role::Admin)?;
    let found = msg::find(a, T_KP, vec![observe::b32("ag.h", kp_id)]).await?;
    let row = found
        .iter()
        .find(|o| msg::bytes(o, "ag.body").is_some_and(|b| charter_crypto::sha256(b) == *kp_id))
        .ok_or_else(|| SyncError::coded(ErrorCode::NotLive, "no live key package with that id"))?;
    let kp = msg::bytes(row, "ag.body").unwrap_or_default();
    let (nid, device, sig_key) = mls.kp_identity(kp)?;
    let bound = match (device, msg::bytes(row, "ag.pk"), msg::bytes(row, "ag.sig")) {
        (Some(d), Some(pk), Some(sig)) => auth::leaf_bound(&nid, pk, sig, &sig_key, &d),
        _ => false,
    };
    if !bound {
        return Err(bad(
            "this key package is not signed by the identity it names for one of its devices; \
             the invitee must run `chart member keypackage` on charter 0.6.0 or later",
        ));
    }
    match (role, ring.role(&nid)) {
        (Some(_), Some(have)) => {
            return Err(SyncError::coded(
                ErrorCode::BadFormat,
                format!(
                    "{} is already a member ({have:?}); to add another of its devices use \
                     `chart member add --device`",
                    charter_sync::ids::to_text(&nid)
                ),
            ));
        }
        (None, None) => return Err(not_member(rid)),
        _ => {}
    }
    let pre = prepare(a, cfg, keys, &ring).await?;
    let mut g = mls.load(&ring.group)?;
    let (commit, welcome, _) = mls.add(&mut g, kp)?;
    if let Some(role) = role {
        ring.roles.insert(hex::encode(nid), role);
    }
    let (epoch, key) = mls.epoch_key(&g, rid)?;
    ring.epochs.insert(epoch, key);
    let welcome = Some((*kp_id, welcome, mls.history_key(&g, rid)?));
    finish(a, cfg, keys, &mls, ring, pre, commit, welcome).await?;
    Ok(nid)
}

/// Admin only: removes `nid` from every device. The commit starts a new epoch whose content
/// key and id key `nid` cannot derive.
pub async fn remove_member<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
    nid: &Nid,
) -> Result<()> {
    let (mls, mut ring) = member_ring(a, cfg, keys, rid).await?;
    ring.require(&keys.nid(), Role::Admin)?;
    let pre = prepare(a, cfg, keys, &ring).await?;
    let mut g = mls.load(&ring.group)?;
    let commit = mls.remove(&mut g, nid)?;
    ring.roles.remove(&hex::encode(nid));
    let (epoch, key) = mls.epoch_key(&g, rid)?;
    ring.epochs.insert(epoch, key);
    ring.id_rot.insert(epoch, mls.id_key(&g, rid)?);
    finish(a, cfg, keys, &mls, ring, pre, commit, None).await?;
    Ok(())
}

/// What a commit needs from before it: whether it migrates a legacy repo (then the caller
/// must be a root delegate), the heads to pin, and this admin's peer document if it has none
/// yet (readers look its key up to check the attestation). Two queries on the add and remove
/// paths, three on the migrating one.
pub(crate) struct Pre {
    migrate: bool,
    pinned: BTreeSet<String>,
    peer: Option<charter_store::Operation>,
}

pub(crate) async fn prepare<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    ring: &Keyring,
) -> Result<Pre> {
    let migrate = ring.format < FORMAT;
    if migrate
        && !root_rows(a, ring)
            .await?
            .is_some_and(|(roots, _)| roots.contains(&keys.nid()))
    {
        return Err(SyncError::coded(
            ErrorCode::RoleDenied,
            "this private repo predates charter 0.5.1: a root delegate must make its next \
             membership change, on 0.5.1 or later, to upgrade it",
        ));
    }
    let heads = Pred::And(vec![
        observe::kind(Kind::Head),
        observe::b32("ag.rid", &ring.rid),
    ]);
    let pinned = observe::all(&Sealed::new(a, ring.clone()), heads)
        .await?
        .iter()
        .filter(|o| o.attr("ag.ep").is_none())
        .filter_map(|o| match o.attr("ag.h") {
            Some(charter_core::AttrValue::Bytes32(h)) => Some(hex::encode(h)),
            _ => None,
        })
        .collect();
    let peer = match charter_sync::cob::peer_key_of(a, &keys.nid()).await? {
        Some(_) => None,
        None => first_write_ops(keys, &ring.rid, cfg)?.into_iter().next(),
    };
    Ok(Pre {
        migrate,
        pinned,
        peer,
    })
}

/// Sends the commit (and welcome) and saves the MLS state and the keyring it returns.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finish<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    mls: &Mls,
    mut ring: Keyring,
    pre: Pre,
    commit: Vec<u8>,
    welcome: Option<Welcome>,
) -> Result<Keyring> {
    if pre.migrate {
        ring.format = FORMAT;
    }
    ring.pinned = pre.pinned;
    let epoch = ring.current()?.0;
    let roles = ring.roles.clone();
    ring.record_roles(epoch, &roles);
    let admins = roles
        .iter()
        .filter(|(_, r)| **r == Role::Admin)
        .map(|(n, _)| n.clone())
        .collect();
    ring.adm_log.insert(epoch, admins);
    let extra = pre.peer.into_iter().collect();
    let landed = publish_commit(a, cfg, keys, &ring, commit, welcome, pre.migrate, extra).await?;
    if let Some(block) = landed {
        ring.rotated.insert(epoch, block);
    }
    mls.save()?;
    ring.save(&cfg.home)?;
    Ok(ring)
}

/// A Welcome for key package `.0` and the epoch's history key (`.2`).
type Welcome = (Hash32, Vec<u8>, crate::seal::Key);

/// One tx from the admin's control address: `extra` (a missing peer document), the commit
/// with only the new epoch's part of the keyring ([`Keyring::delta`]) sealed under the new
/// epoch key and this admin's public attestation, the Welcome if any with the whole keyring
/// sealed under the epoch's history key (which a published content key does not give) and
/// its sealed attestation; and on a migration, the stopper. Returns the block it landed in.
#[allow(clippy::too_many_arguments)]
async fn publish_commit<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    ring: &Keyring,
    commit: Vec<u8>,
    welcome: Option<Welcome>,
    stop: bool,
    extra: Vec<charter_store::Operation>,
) -> Result<Option<u64>> {
    let (epoch, key) = ring.current()?;
    let rid = &ring.rid;
    let bx = msg::seal_box(&ring.delta(epoch), epoch, key)?;
    let ep = || ("ag.ep", AttributeValue::U64(epoch), true);
    let lockbox = |b: &[u8]| ("ag.box", AttributeValue::Bytes(b.to_vec()), false);
    let prev = epoch.saturating_sub(1);
    let rid_attr = ("ag.rid", b32(blinded_rid_at(ring, prev)), true);
    let admins = ring.adm_log.get(&epoch).cloned().unwrap_or_default();
    let signed = auth::sign_commit(keys, rid, &ring.id_key, epoch, key, &commit, &bx, &admins)?;
    let commit_att = ("ag.att", AttributeValue::Bytes(signed), false);
    let ct = ("ag.ct", b32(auth::commits_tag(&ring.id_key, rid)), true);
    let mut ops = extra;
    ops.push(msg::op(
        T_COMMIT,
        vec![rid_attr, ct, ep(), lockbox(&bx), commit_att],
        commit,
        cfg.control_lifetime,
    )?);
    if let Some((kp, w, history)) = welcome {
        let hbx = msg::seal_box(ring, epoch, &history)?;
        let msg = att_msg(rid, epoch, &w, &hbx, Some(&kp));
        let att = auth::seal_att(keys, CTX_WELCOME, key, rid, epoch, &msg)?;
        let attrs = vec![
            ("ag.kp", b32(kp), true),
            ep(),
            lockbox(&hbx),
            ("ag.att", AttributeValue::Bytes(att), false),
        ];
        ops.push(msg::op(T_WELCOME, attrs, w, cfg.control_lifetime)?);
    }
    if stop {
        ops.push(msg::stop_op(ring, cfg.control_lifetime)?);
    }
    let control = keys.control(rid);
    let landed = write::send_funded(a, &control, &keys.wallet(), &cfg.pipeline, vec![ops]).await?;
    Ok(landed.last().map(|l| l.receipt.block_number))
}

/// Root delegate only: publishes epoch `epoch`'s grant, signed, with that epoch's roles, so
/// anyone who knows the rid can read what was sealed in that epoch, and no earlier one
/// (sponsorware, source escrow release), and no one else can substitute a grant.
pub async fn publish_epoch_key<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
    epoch: u64,
) -> Result<Grant> {
    let mut grants = publish_epoch_keys(a, cfg, keys, rid, epoch..=epoch).await?;
    Ok(grants.remove(0))
}

/// [`publish_epoch_key`] for every epoch in `epochs`: a grant covers exactly the one epoch
/// it signs, so a range is one grant per epoch, each in its own tx.
pub async fn publish_epoch_keys<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
    epochs: std::ops::RangeInclusive<u64>,
) -> Result<Vec<Grant>> {
    let (_, ring) = member_ring(a, cfg, keys, rid).await?;
    ring.require(&keys.nid(), Role::Admin)?;
    let roots = root_rows(a, &ring).await?.map(|r| r.0).unwrap_or_default();
    if !roots.contains(&keys.nid()) {
        return Err(SyncError::coded(
            ErrorCode::RoleDenied,
            "only a delegate of the root identity can publish a grant readers can verify",
        ));
    }
    let (mut grants, mut batches) = (vec![], vec![]);
    for epoch in epochs {
        let g = grant(&ring, epoch)?;
        let roles: Vec<(Nid, Role)> = ring.roles_at(epoch).into_iter().collect();
        let key: crate::seal::Key = g[32..].try_into().map_err(|_| not_member(rid))?;
        let body = auth::encode_grant(keys, rid, epoch, ring.id_key_at(epoch), &key, &roles)?;
        let attrs = vec![
            ("ag.tag", b32(tag(rid)), true),
            ("ag.ep", AttributeValue::U64(epoch), true),
        ];
        batches.push(vec![msg::op(T_PUB, attrs, body, cfg.control_lifetime)?]);
        grants.push(g);
    }
    write::send_funded(
        a,
        &keys.control(rid),
        &keys.wallet(),
        &cfg.pipeline,
        batches,
    )
    .await?;
    Ok(grants)
}

/// Epoch `epoch`'s grant from the local keyring, for an escrow agent or a timelock.
pub fn export_epoch_key(home: &std::path::Path, rid: &Rid, epoch: u64) -> Result<Grant> {
    grant(
        &Keyring::load(home, rid)?.ok_or_else(|| not_member(rid))?,
        epoch,
    )
}

fn grant(ring: &Keyring, epoch: u64) -> Result<Grant> {
    let key = ring.epochs.get(&epoch).ok_or_else(|| {
        SyncError::coded(
            ErrorCode::NotMember,
            format!("no key held for epoch {epoch}"),
        )
    })?;
    Ok(zeroize::Zeroizing::new(
        [ring.id_key_at(epoch).as_slice(), key].concat(),
    ))
}

/// `HMAC(name)` as stored in `ag.name`, for tests and tooling.
pub fn blinded_name(ring: &Keyring, name: &str) -> Hash32 {
    let epoch = ring.current().map_or(0, |c| c.0);
    blind(ring.id_key_at(epoch), "name", name.as_bytes())
}
