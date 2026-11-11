//! Push, part 1 (design 4.2 steps 1-5, 4.4): stage objects in the cache, compute the new
//! signed refs, generate self-contained packs, sign manifests and record `pending.json`.
//! Part 2 ([`crate::push_send`]) uploads, anchors and commits.

use std::path::Path;

use charter_core::{
    Body, ErrorCode, Kind, MAX_PACK_SIZE, ManifestBody, Nid, RefEntry, RefsBody, Require, ZERO,
    content_hash,
};
use charter_store::Arkiv;

use crate::git::Git;
use crate::ids::{hex32, unhex32};
use crate::keys::Keys;
use crate::repo::{Config, Repo, staging};
use crate::state::{PendingPack, PendingPush, unhex};
use crate::view::View;
use crate::write::sign;
use crate::{Result, SyncError, packs, warn};

const PENDING_VERSION: u32 = 1;
/// Object format written by P1 (SHA-1 repos).
const OBJECT_HASH_SHA1: u8 = 1;

/// One ref update as git sends it: `src` (a local ref or oid; `None` deletes) to `dst`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub src: Option<String>,
    pub dst: String,
}

/// A push in progress; see [`Push::prepare`] and the steps in [`crate::push_send`].
pub struct Push<'a, A: Arkiv> {
    pub(crate) a: &'a A,
    pub(crate) repo: &'a Repo,
    pub(crate) cfg: &'a Config,
    pub(crate) keys: &'a Keys,
    pub view: View,
    pub pending: PendingPush,
}

fn check_names(updates: &[Update]) -> Result<()> {
    for u in updates {
        let probe = RefsBody {
            rid: ZERO,
            object_hash: OBJECT_HASH_SHA1,
            refs: vec![RefEntry {
                name: u.dst.clone(),
                oid: vec![0; 20],
            }],
        };
        probe.validate().map_err(|_| {
            SyncError::coded(
                ErrorCode::BadRefs,
                format!(
                    "{}: only refs/heads/, refs/tags/ and refs/notes/ can be pushed",
                    u.dst
                ),
            )
        })?;
    }
    Ok(())
}

/// Copies `updates` from `work` into the cache's staging refs of `nid`; returns
/// `(dst, new oid hex)` per update (`None` deletes).
pub(crate) fn stage(
    repo: &Repo,
    nid: &Nid,
    work: &Git,
    updates: &[Update],
) -> Result<Vec<(String, Option<String>)>> {
    check_names(updates)?;
    let nid = *nid;
    let refspecs: Vec<String> = updates
        .iter()
        .filter_map(|u| {
            u.src
                .as_ref()
                .map(|s| format!("+{s}:{}{}", staging(&nid), u.dst))
        })
        .collect();
    work.push_to(&repo.git_dir(), &refspecs)?;
    let staged: std::collections::BTreeMap<String, String> =
        repo.git().refs(&staging(&nid))?.into_iter().collect();
    let mut planned = vec![];
    for u in updates {
        let oid = match &u.src {
            None => None,
            Some(_) => Some(
                staged
                    .get(&format!("{}{}", staging(&nid), u.dst))
                    .cloned()
                    .ok_or_else(|| SyncError::coded(ErrorCode::Internal, "staging failed"))?,
            ),
        };
        planned.push((u.dst.clone(), oid));
    }
    Ok(planned)
}

impl<'a, A: Arkiv> Push<'a, A> {
    /// Stages `updates` from `work` and builds (or resumes) the pending push. `view` must
    /// come from a fetch sync that included this peer (`Want::must`).
    pub fn prepare(
        a: &'a A,
        repo: &'a Repo,
        cfg: &'a Config,
        keys: &'a Keys,
        view: View,
        work: &Git,
        updates: &[Update],
    ) -> Result<Self> {
        // Every chunk, piece and head tx of this push leaves through `Self`.
        crate::guard::require_sealed(a, &cfg.home, &repo.rid)?;
        let planned = stage(repo, &keys.nid(), work, updates)?;
        let mut push = Self {
            a,
            repo,
            cfg,
            keys,
            view,
            pending: empty_pending(),
        };
        push.pending = match push.resumable(&planned)? {
            Some(p) => p,
            None => push.build(planned)?,
        };
        Ok(push)
    }

    pub(crate) fn nid(&self) -> Nid {
        self.keys.nid()
    }

    /// `(seq, prev)` of the next own anchor, from the verified view.
    fn next_seq(&self) -> (u64, charter_core::Hash32) {
        match self
            .view
            .peers
            .get(&self.nid())
            .and_then(|p| p.head.as_ref())
        {
            Some(h) => (h.anchor.body.seq + 1, h.anchor.h),
            None => (0, ZERO),
        }
    }

    /// A pending push for the same updates whose base is still the own head, or whose
    /// anchor already landed; anything else is discarded (orphaned chunks expire).
    fn resumable(&self, planned: &[(String, Option<String>)]) -> Result<Option<PendingPush>> {
        let Some(p) = PendingPush::load(&self.repo.pending_path())? else {
            return Ok(None);
        };
        let (seq, prev) = self.next_seq();
        let landed = p
            .anchor
            .as_ref()
            .map(|b| unhex(b).map(|b| content_hash(Kind::Anchor, &b)))
            .transpose()?;
        let head = self
            .view
            .peers
            .get(&self.nid())
            .and_then(|v| v.head.as_ref());
        let anchored = landed.is_some() && head.map(|h| h.anchor.h) == landed;
        let base_ok = p.seq == seq && unhex32(&p.prev)? == prev;
        let files_ok = p
            .packs
            .iter()
            .all(|pk| self.repo.outgoing().join(&pk.file).exists());
        if p.updates == planned && files_ok && (anchored || base_ok) {
            return Ok(Some(p));
        }
        warn("discarding an unfinished push (its chunks on Arkiv expire unanchored)");
        self.discard(&p);
        Ok(None)
    }

    pub fn discard(&self, p: &PendingPush) {
        for pk in &p.packs {
            let _ = std::fs::remove_file(self.repo.outgoing().join(&pk.file));
        }
        let _ = std::fs::remove_file(self.repo.pending_path());
    }

    fn new_refs(&self, planned: &[(String, Option<String>)]) -> Result<RefsBody> {
        let mut refs: std::collections::BTreeMap<String, Vec<u8>> = self
            .view
            .peers
            .get(&self.nid())
            .and_then(|p| p.head.as_ref())
            .map(|h| {
                h.refs
                    .refs
                    .iter()
                    .map(|e| (e.name.clone(), e.oid.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (dst, oid) in planned {
            match oid {
                Some(o) => refs.insert(
                    dst.clone(),
                    hex::decode(o).map_err(|e| SyncError::io("oid", e))?,
                ),
                None => refs.remove(dst),
            };
        }
        Ok(RefsBody {
            rid: self.repo.rid,
            object_hash: OBJECT_HASH_SHA1,
            refs: refs
                .into_iter()
                .map(|(name, oid)| RefEntry { name, oid })
                .collect(),
        })
    }

    /// Verified tips of every synced peer (excluded from packs) and the matching `requires`.
    fn excluded(&self) -> (Vec<String>, Vec<Require>) {
        let mut tips = vec![];
        let mut requires = vec![];
        for (nid, pv) in &self.view.peers {
            let Some(h) = &pv.head else { continue };
            tips.extend(h.refs.refs.iter().map(|e| hex::encode(&e.oid)));
            if *nid != self.nid() && !h.refs.refs.is_empty() {
                requires.push(Require {
                    nid: *nid,
                    anchor: h.anchor.h,
                });
            }
        }
        (tips, requires)
    }

    fn build(&self, planned: Vec<(String, Option<String>)>) -> Result<PendingPush> {
        let refs = sign(self.keys.sk(), &self.new_refs(&planned)?)?;
        let tips: Vec<String> = planned.iter().filter_map(|(_, o)| o.clone()).collect();
        let (exclude, requires) = self.excluded();
        let files = match tips.is_empty() {
            true => vec![],
            false => {
                self.repo
                    .git()
                    .pack_objects(&tips, &exclude, &self.repo.outgoing().join("pk"))?
            }
        };
        let mut packs = vec![];
        let mut manifests = vec![];
        for file in files {
            let (pk, m) = self.manifest(&file, &requires)?;
            packs.push(pk);
            manifests.push(hex::encode(sign(self.keys.sk(), &m)?.bytes));
        }
        let (seq, prev) = self.next_seq();
        let p = PendingPush {
            v: PENDING_VERSION,
            updates: planned,
            packs,
            manifests,
            refs: hex::encode(refs.bytes),
            seq,
            prev: hex32(&prev),
            anchor: None,
        };
        p.save(&self.repo.pending_path())?;
        Ok(p)
    }

    fn manifest(&self, file: &Path, requires: &[Require]) -> Result<(PendingPack, ManifestBody)> {
        let (pack_id, size, trailer, chunk_hashes) = packs::describe(file)?;
        if size > MAX_PACK_SIZE {
            return Err(SyncError::coded(
                ErrorCode::BadPack,
                format!("pack of {size} B exceeds 100 MiB (an object larger than a pack?)"),
            ));
        }
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let m = ManifestBody {
            rid: self.repo.rid,
            pack_id,
            trailer,
            size,
            chunk_size: charter_core::CHUNK_SIZE as u64,
            chunk_hashes,
            requires: requires.to_vec(),
            pack_kind: 1,
        };
        let pk = PendingPack {
            file: name,
            pack_id: hex32(&pack_id),
        };
        Ok((pk, m))
    }
}

fn empty_pending() -> PendingPush {
    PendingPush {
        v: PENDING_VERSION,
        updates: vec![],
        packs: vec![],
        manifests: vec![],
        refs: String::new(),
        seq: 0,
        prev: hex32(&ZERO),
        anchor: None,
    }
}
