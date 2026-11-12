//! A member's keys for one private repo: the id-blinding keys, every epoch content key it
//! holds, the roles map and its history, and where each epoch ended. The same struct is the
//! sealed lockbox a commit or welcome carries, and the local file under
//! `$CHARTER_HOME/private/`, mode 600.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use charter_core::{ErrorCode, Nid, Rid};
use charter_sync::{Result, SyncError};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::seal::Key;

/// Format of a repo whose lockboxes carry the 0.6.0 security fields (attested welcomes and
/// commits, id-key generations, role history, rotation positions); see
/// docs/p2/private-repos.md "Migration". Lower values are legacy repos.
pub const FORMAT: u32 = 2;

/// GitHub-style roles: admin manages members, write pushes, read clones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl std::str::FromStr for Role {
    type Err = SyncError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "admin" => Ok(Role::Admin),
            "write" => Ok(Role::Write),
            "read" => Ok(Role::Read),
            _ => Err(SyncError::coded(
                ErrorCode::BadFormat,
                format!("role must be admin, write or read, got {s:?}"),
            )),
        }
    }
}

/// Hex nid to role (`None`: no longer a member).
pub type RoleChanges = BTreeMap<String, Option<Role>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keyring {
    pub rid: Rid,
    /// HMAC key for `ag.rid` / `ag.name` / `ag.pack` of the creating epoch (generation 0).
    pub id_key: Key,
    /// Later id-key generations: the epoch a removal started, to the key exported there.
    #[serde(default)]
    pub id_rot: BTreeMap<u64, Key>,
    pub epochs: BTreeMap<u64, Key>,
    /// Hex nid to role, as of the newest epoch.
    pub roles: BTreeMap<String, Role>,
    /// Role changes by the epoch they took effect in; the first entry is a full snapshot.
    #[serde(default)]
    pub role_log: BTreeMap<u64, RoleChanges>,
    /// Epoch to the block of the commit that started it: epoch `e` ends at `rotated[e + 1]`.
    #[serde(default)]
    pub rotated: BTreeMap<u64, u64>,
    /// `ag.h` (hex) of the device heads an admin saw live when the newest epoch started, so
    /// an extend after that does not drop a head its device has not re-sealed since.
    #[serde(default)]
    pub pinned: BTreeSet<String>,
    /// The admins (hex nids) of each epoch since format 2, as its committer recorded them;
    /// each commit's public attestation pins its epoch's set ([`crate::auth::sign_commit`]).
    #[serde(default)]
    pub adm_log: BTreeMap<u64, BTreeSet<String>>,
    /// [`FORMAT`] once a root delegate on 0.6.0 or later wrote a commit.
    #[serde(default)]
    pub format: u32,
    /// Local MLS group id (also visible in MLS message headers, so not secret).
    pub group: Vec<u8>,
    /// An admin's refresh only: leaves whose device a revoke record names, before checking
    /// the records, so a write can offer [`crate::revoked::remove_revoked`]. Never saved.
    #[serde(skip)]
    pub revoked_leaves: usize,
    /// The refresh found this member removed (and not added again): the keyring keeps the
    /// old epochs for reading history. Never saved.
    #[serde(skip)]
    pub removed: bool,
}

impl Drop for Keyring {
    fn drop(&mut self) {
        self.id_key.zeroize();
        self.id_rot.values_mut().for_each(Zeroize::zeroize);
        self.epochs.values_mut().for_each(Zeroize::zeroize);
    }
}

impl Keyring {
    /// An empty keyring for `rid` with generation-0 id key `id_key` and MLS group `group`.
    pub fn new(rid: Rid, id_key: Key, group: Vec<u8>) -> Self {
        Self {
            rid,
            id_key,
            id_rot: BTreeMap::new(),
            epochs: BTreeMap::new(),
            roles: BTreeMap::new(),
            role_log: BTreeMap::new(),
            rotated: BTreeMap::new(),
            pinned: BTreeSet::new(),
            adm_log: BTreeMap::new(),
            format: 0,
            group,
            revoked_leaves: 0,
            removed: false,
        }
    }

    /// Newest epoch and its key; every keyring holds at least one.
    pub fn current(&self) -> Result<(u64, &Key)> {
        self.epochs
            .iter()
            .next_back()
            .map(|(e, k)| (*e, k))
            .ok_or_else(|| not_member(&self.rid))
    }

    /// The id key that blinds what epoch `epoch` seals.
    pub fn id_key_at(&self, epoch: u64) -> &Key {
        self.id_rot
            .range(..=epoch)
            .next_back()
            .map_or(&self.id_key, |(_, k)| k)
    }

    /// Every id-key generation, oldest first.
    pub fn id_keys(&self) -> impl Iterator<Item = &Key> {
        std::iter::once(&self.id_key).chain(self.id_rot.values())
    }

    pub fn role(&self, nid: &Nid) -> Option<Role> {
        self.roles.get(&hex::encode(nid)).copied()
    }

    /// `nid`'s role during `epoch`, from the role history.
    pub fn role_at(&self, epoch: u64, nid: &Nid) -> Option<Role> {
        let n = hex::encode(nid);
        self.role_log
            .range(..=epoch)
            .rev()
            .find_map(|(_, m)| m.get(&n))
            .copied()
            .flatten()
    }

    /// Every member of epoch `epoch` and its role then.
    pub fn roles_at(&self, epoch: u64) -> BTreeMap<Nid, Role> {
        let mut out = BTreeMap::new();
        for (_, m) in self.role_log.range(..=epoch) {
            for (n, r) in m {
                let Some(nid) = hex::decode(n).ok().and_then(|b| Nid::try_from(b).ok()) else {
                    continue;
                };
                match r {
                    Some(r) => out.insert(nid, *r),
                    None => out.remove(&nid),
                };
            }
        }
        out
    }

    /// Records `roles` as the full role set from `epoch` on (only the differences are kept).
    pub fn record_roles(&mut self, epoch: u64, roles: &BTreeMap<String, Role>) {
        let first = self.role_log.range(..=epoch).next().is_none();
        let mut known: BTreeSet<String> = self
            .role_log
            .range(..=epoch)
            .flat_map(|(_, m)| m.keys().cloned())
            .collect();
        known.extend(roles.keys().cloned());
        let changes: RoleChanges = known
            .into_iter()
            .filter_map(|n| {
                let nid: Nid = hex::decode(&n).ok()?.try_into().ok()?;
                let now = roles.get(&n).copied();
                (first || self.role_at(epoch, &nid) != now).then_some((n, now))
            })
            .collect();
        if !changes.is_empty() {
            self.role_log.entry(epoch).or_default().extend(changes);
        }
    }

    /// The block epoch `epoch` ended at, if a later epoch's start is known.
    pub fn cutoff(&self, epoch: u64) -> Option<u64> {
        self.rotated
            .range(epoch.saturating_add(1)..)
            .next()
            .map(|(_, b)| *b)
    }

    /// `E_ROLE_DENIED` unless `nid` holds at least `min`.
    pub fn require(&self, nid: &Nid, min: Role) -> Result<()> {
        match self.role(nid) {
            Some(r) if r >= min => Ok(()),
            have => Err(SyncError::coded(
                ErrorCode::RoleDenied,
                format!("{min:?} role needed in this private repo, you have {have:?}"),
            )),
        }
    }

    /// Adopts a newer lockbox: its roles and pinned heads, plus everything either side holds.
    pub fn merge(&mut self, newer: &Keyring) {
        self.epochs
            .extend(newer.epochs.iter().map(|(e, k)| (*e, *k)));
        self.id_rot
            .extend(newer.id_rot.iter().map(|(e, k)| (*e, *k)));
        for (e, m) in &newer.role_log {
            self.role_log.entry(*e).or_insert_with(|| m.clone());
        }
        // An epoch end the committer could not have placed yet is junk.
        let last = newer.current().map_or(0, |c| c.0);
        for (e, b) in newer.rotated.range(..=last) {
            self.rotated.entry(*e).or_insert(*b);
        }
        self.pinned.clone_from(&newer.pinned);
        self.roles.clone_from(&newer.roles);
        for (e, a) in &newer.adm_log {
            self.adm_log.entry(*e).or_insert_with(|| a.clone());
        }
        self.format = self.format.max(newer.format);
    }

    /// What a commit for `epoch` hands the members who apply it: that epoch's key, id-key
    /// generation, roles, admin set and pinned heads, and nothing older (they hold it), so
    /// a grant for `epoch`, which opens the commit's lockbox, opens no other epoch.
    pub fn delta(&self, epoch: u64) -> Keyring {
        fn at<V: Clone>(m: &BTreeMap<u64, V>, e: u64) -> BTreeMap<u64, V> {
            m.get(&e).map(|v| (e, v.clone())).into_iter().collect()
        }
        let mut d = Keyring::new(self.rid, *self.id_key_at(epoch), self.group.clone());
        d.epochs = at(&self.epochs, epoch);
        d.id_rot = at(&self.id_rot, epoch);
        d.role_log = at(&self.role_log, epoch);
        d.adm_log = at(&self.adm_log, epoch);
        d.roles.clone_from(&self.roles);
        d.pinned.clone_from(&self.pinned);
        d.format = self.format;
        d
    }

    pub fn path(home: &Path, rid: &Rid) -> PathBuf {
        home.join("private")
            .join(format!("{}.json", hex::encode(rid)))
    }

    pub fn load(home: &Path, rid: &Rid) -> Result<Option<Self>> {
        let p = Self::path(home, rid);
        match std::fs::read(&p) {
            Ok(mut b) => {
                let ring = serde_json::from_slice(&b).map_err(|e| bad_file(&p, e));
                b.zeroize();
                Ok(Some(ring?))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SyncError::io("read private keyring", e)),
        }
    }

    pub fn save(&self, home: &Path) -> Result<()> {
        let mut bytes = serde_json::to_vec(self).map_err(|e| bad_file(&home.join("private"), e))?;
        let r = write_private(&Self::path(home, &self.rid), &bytes);
        bytes.zeroize();
        r
    }
}

pub fn not_member(rid: &Rid) -> SyncError {
    SyncError::coded(
        ErrorCode::NotMember,
        format!(
            "{} is a private repo and you hold no key for it (ask an admin for `chart member add`)",
            charter_sync::ids::to_text(rid)
        ),
    )
}

fn bad_file(p: &Path, e: serde_json::Error) -> SyncError {
    SyncError::coded(ErrorCode::BadFormat, format!("{}: {e}", p.display()))
}

/// Writes `bytes` to `path` with mode 600 (directory 700), via a temp file and rename.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let io = |e| SyncError::io("write private state", e);
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(io)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io)?;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(io)?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_order_and_files_are_private() {
        let mut ring = Keyring::new([1; 32], [2; 32], vec![7]);
        ring.epochs = BTreeMap::from([(1, [3; 32]), (2, [4; 32])]);
        ring.roles = BTreeMap::from([(hex::encode([5u8; 32]), Role::Write)]);
        assert_eq!(ring.current().expect("cur").0, 2);
        assert!(ring.require(&[5; 32], Role::Read).is_ok());
        assert!(ring.require(&[5; 32], Role::Admin).is_err());
        assert!(ring.require(&[6; 32], Role::Read).is_err());
        let home = tempfile::tempdir().expect("tmp");
        ring.save(home.path()).expect("save");
        let mode = std::fs::metadata(Keyring::path(home.path(), &ring.rid))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let back = Keyring::load(home.path(), &ring.rid).expect("load");
        assert_eq!(back.as_ref(), Some(&ring));
        ring.epochs.clear();
        assert_eq!(
            ring.current().map_err(|e| e.code()).err(),
            Some(ErrorCode::NotMember)
        );
    }

    #[test]
    fn a_legacy_keyring_file_still_loads() {
        let legacy = format!(
            r#"{{"rid":{r:?},"id_key":{k:?},"epochs":{{"0":{k:?}}},"roles":{{}},"group":[1]}}"#,
            r = [1u8; 32],
            k = [2u8; 32]
        );
        let ring: Keyring = serde_json::from_str(&legacy).expect("legacy");
        assert_eq!(ring.format, 0);
        assert!(ring.role_log.is_empty() && ring.id_rot.is_empty());
    }

    #[test]
    fn role_history_id_generations_and_cutoffs() {
        let (a, b) = ([5u8; 32], [6u8; 32]);
        let mut ring = Keyring::new([0; 32], [1; 32], vec![]);
        ring.record_roles(0, &BTreeMap::from([(hex::encode(a), Role::Admin)]));
        ring.record_roles(
            1,
            &BTreeMap::from([(hex::encode(a), Role::Admin), (hex::encode(b), Role::Write)]),
        );
        ring.record_roles(2, &BTreeMap::from([(hex::encode(a), Role::Admin)]));
        assert_eq!(ring.role_at(0, &b), None);
        assert_eq!(ring.role_at(1, &b), Some(Role::Write));
        assert_eq!(ring.role_at(5, &b), None);
        assert_eq!(ring.role_at(5, &a), Some(Role::Admin));
        assert_eq!(ring.role_log[&2].len(), 1, "only the change is kept");

        ring.id_rot.insert(2, [9; 32]);
        assert_eq!(ring.id_key_at(1), &[1; 32]);
        assert_eq!(ring.id_key_at(3), &[9; 32]);
        assert_eq!(ring.id_keys().count(), 2);

        ring.rotated = BTreeMap::from([(1, 100), (2, 200)]);
        assert_eq!(ring.cutoff(0), Some(100));
        assert_eq!(ring.cutoff(1), Some(200));
        assert_eq!(ring.cutoff(2), None);
    }
}
