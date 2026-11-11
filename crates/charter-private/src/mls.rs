//! OpenMLS behind one ciphersuite constant: X-Wing (ML-KEM-768 + X25519) key agreement with
//! ML-DSA-44 signatures. State (signature key, key package secrets, groups) persists under
//! `$CHARTER_HOME/private/mls.json`, mode 600.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use charter_core::{ErrorCode, Hash32, Nid, Rid};
use charter_sync::{Result, SyncError};
use openmls::prelude::tls_codec::{Deserialize as _, Serialize as _};
use openmls::prelude::*;
use openmls_basic_credential::SignatureKeyPair;
use openmls_libcrux_crypto::Provider;
use openmls_traits::OpenMlsProvider;
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::keyring::write_private;
use crate::seal::Key;

/// The one place the suite is named. ML-DSA-44 inside MLS is not yet confirmed; switching
/// to `MLS_128_MLKEM768X25519_AES256GCM_SHA384_Ed25519` (0x004E) is this one line.
pub const SUITE: Ciphersuite = Ciphersuite::MLS_128_MLKEM768X25519_CHACHA20POLY1305_SHA384_MLDSA44;
/// MLS exporter label of the per-epoch content key; the context is the rid.
pub const CONTENT_LABEL: &str = "arkiv-git content"; // protocol constant: unchanged by the Charter rename
/// MLS exporter label of the id-blinding key, taken once at the creating epoch.
pub const ID_LABEL: &str = "arkiv-git id"; // protocol constant: unchanged by the Charter rename
/// MLS exporter label of the key a welcome's history lockbox is sealed under: only members
/// of that epoch derive it, not holders of its published content key.
pub const HISTORY_LABEL: &str = "charter history";

fn mls_err(what: &str, e: impl std::fmt::Debug) -> SyncError {
    SyncError::coded(ErrorCode::BadFormat, format!("mls {what}: {e:?}"))
}

#[derive(Serialize)]
struct SavedRef<'a> {
    signer: &'a SignatureKeyPair,
    storage: Vec<(String, String)>,
    kps: &'a [Hash32],
    pending: &'a BTreeMap<String, u64>,
}

#[derive(Deserialize)]
struct Saved {
    signer: SignatureKeyPair,
    /// OpenMLS storage, hex key to hex value.
    storage: Vec<(String, String)>,
    /// Ids (`sha256` of the bytes) of key packages this member published.
    kps: Vec<Hash32>,
    #[serde(default)]
    pending: BTreeMap<String, u64>,
}

/// OpenMLS storage contents, from [`Mls::snapshot`].
pub struct Snapshot(HashMap<Vec<u8>, Vec<u8>>);

pub struct Mls {
    provider: Provider,
    signer: SignatureKeyPair,
    nid: Nid,
    path: PathBuf,
    pub kps: Vec<Hash32>,
    /// Hex group id to the block this member's pending commit there landed in (or the head
    /// when its send failed): [`crate::group`] discards a pending commit that never showed
    /// up on chain only well after it.
    pub pending: BTreeMap<String, u64>,
}

fn create_config() -> MlsGroupCreateConfig {
    MlsGroupCreateConfig::builder()
        .ciphersuite(SUITE)
        .use_ratchet_tree_extension(true)
        // Commits as PrivateMessage: proposals name members, so they must not be public.
        .wire_format_policy(PURE_CIPHERTEXT_WIRE_FORMAT_POLICY)
        .build()
}

impl Mls {
    /// Loads this member's MLS state, or creates a fresh ML-DSA-44 identity for `nid`.
    pub fn open(home: &Path, nid: Nid) -> Result<Self> {
        let path = home.join("private").join("mls.json");
        let provider = Provider::default();
        let (signer, kps, pending) = match std::fs::read(&path) {
            Ok(mut b) => {
                let s = serde_json::from_slice::<Saved>(&b);
                b.zeroize();
                let s = s.map_err(|e| mls_err("state file", e))?;
                let mut values = provider
                    .storage()
                    .values
                    .write()
                    .map_err(|_| mls_err("storage", "poisoned"))?;
                for (k, mut v) in s.storage {
                    let k = hex::decode(k).map_err(|e| mls_err("state file", e))?;
                    let decoded = hex::decode(&v).map_err(|e| mls_err("state file", e));
                    v.zeroize();
                    values.insert(k, decoded?);
                }
                (s.signer, s.kps, s.pending)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
                SignatureKeyPair::new(SUITE.signature_algorithm())
                    .map_err(|e| mls_err("keygen", e))?,
                vec![],
                BTreeMap::new(),
            ),
            Err(e) => return Err(SyncError::io("read mls state", e)),
        };
        Ok(Self {
            provider,
            signer,
            nid,
            path,
            kps,
            pending,
        })
    }

    pub fn save(&self) -> Result<()> {
        let values = self
            .provider
            .storage()
            .values
            .read()
            .map_err(|_| mls_err("storage", "poisoned"))?;
        let saved = SavedRef {
            signer: &self.signer,
            storage: values
                .iter()
                .map(|(k, v)| (hex::encode(k), hex::encode(v)))
                .collect(),
            kps: &self.kps,
            pending: &self.pending,
        };
        let mut bytes = serde_json::to_vec(&saved).map_err(|e| mls_err("state file", e))?;
        let r = write_private(&self.path, &bytes);
        bytes.zeroize();
        r
    }

    /// This machine's head-model device (docs/p5/head-model.md 4.1): the leaf this state
    /// makes is that device's, so a revoke of the device names exactly this leaf.
    pub fn device(&self) -> Result<Nid> {
        let home = self
            .path
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&self.path);
        Ok(charter_sync::device::device_keys(home)?.nid())
    }

    /// The leaf credential: `nid || device`.
    fn credential(&self) -> Result<CredentialWithKey> {
        Ok(CredentialWithKey {
            credential: BasicCredential::new([self.nid, self.device()?].concat()).into(),
            signature_key: self.signer.to_public_vec().into(),
        })
    }

    /// A fresh key package (its secrets stay in storage); returns its TLS bytes.
    pub fn key_package(&self) -> Result<Vec<u8>> {
        let kp = KeyPackage::builder()
            .build(SUITE, &self.provider, &self.signer, self.credential()?)
            .map_err(|e| mls_err("key package", e))?;
        kp.key_package()
            .tls_serialize_detached()
            .map_err(|e| mls_err("key package", e))
    }

    /// A new group with this member alone; the group id is random, never the rid.
    pub fn create(&self) -> Result<MlsGroup> {
        let mut gid = [0u8; 16];
        getrandom::fill(&mut gid).map_err(|e| mls_err("group id", e))?;
        MlsGroup::new_with_group_id(
            &self.provider,
            &self.signer,
            &create_config(),
            GroupId::from_slice(&gid),
            self.credential()?,
        )
        .map_err(|e| mls_err("create group", e))
    }

    pub fn load(&self, gid: &[u8]) -> Result<MlsGroup> {
        MlsGroup::load(self.provider.storage(), &GroupId::from_slice(gid))
            .map_err(|e| mls_err("load group", e))?
            .ok_or_else(|| mls_err("load group", "unknown group id"))
    }

    /// This member's nid.
    pub fn nid(&self) -> Nid {
        self.nid
    }

    /// Adds the owner of `kp` (TLS bytes); returns `(commit, welcome, new member nid)`. The
    /// commit stays pending (see [`Self::pending_keys`]) until it is seen first on chain.
    pub fn add(&self, g: &mut MlsGroup, kp: &[u8]) -> Result<(Vec<u8>, Vec<u8>, Nid)> {
        let kp = KeyPackageIn::tls_deserialize_exact(kp)
            .map_err(|e| mls_err("key package", e))?
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(|e| mls_err("key package", e))?;
        let nid = nid_of(kp.leaf_node().credential())?;
        let (commit, welcome, _) = g
            .add_members(&self.provider, &self.signer, &[kp])
            .map_err(|e| mls_err("add", e))?;
        Ok((ser(&commit)?, ser(&welcome)?, nid))
    }

    /// Removes every leaf naming `nid` (all of its devices); returns the commit.
    pub fn remove(&self, g: &mut MlsGroup, nid: &Nid) -> Result<Vec<u8>> {
        let leaves: Vec<LeafNodeIndex> = Self::device_leaves(g)
            .into_iter()
            .filter(|(_, n, _)| n == nid)
            .map(|(i, _, _)| i)
            .collect();
        self.remove_leaves(g, &leaves)
    }

    /// Removes `leaves` (and nothing else); returns the commit, left pending.
    pub fn remove_leaves(&self, g: &mut MlsGroup, leaves: &[LeafNodeIndex]) -> Result<Vec<u8>> {
        if leaves.is_empty() {
            return Err(SyncError::coded(
                ErrorCode::NotMember,
                "no such member in the group",
            ));
        }
        let (commit, _, _) = g
            .remove_members(&self.provider, &self.signer, leaves)
            .map_err(|e| mls_err("remove", e))?;
        ser(&commit)
    }

    /// Every leaf: its index, member nid and device (`None` for a leaf made before leaves
    /// named their device).
    pub fn device_leaves(g: &MlsGroup) -> Vec<(LeafNodeIndex, Nid, Option<Nid>)> {
        g.members()
            .filter_map(|m| {
                let (n, d) = identity_of(&m.credential).ok()?;
                Some((m.index, n, d))
            })
            .collect()
    }

    /// Leaves per nid (a member on several devices has several), and how many of them name
    /// no device.
    pub fn leaves(g: &MlsGroup) -> BTreeMap<Nid, (usize, usize)> {
        let mut out = BTreeMap::new();
        for (_, n, d) in Self::device_leaves(g) {
            let e: &mut (usize, usize) = out.entry(n).or_default();
            e.0 += 1;
            e.1 += usize::from(d.is_none());
        }
        out
    }

    /// This member's MLS signature public key (what its leaf binding signs).
    pub fn signature_key(&self) -> Vec<u8> {
        self.signer.to_public_vec()
    }

    /// The nid and device a key package claims and the MLS signature key it carries.
    pub fn kp_identity(&self, kp: &[u8]) -> Result<(Nid, Option<Nid>, Vec<u8>)> {
        let kp = KeyPackageIn::tls_deserialize_exact(kp)
            .map_err(|e| mls_err("key package", e))?
            .validate(self.provider.crypto(), ProtocolVersion::Mls10)
            .map_err(|e| mls_err("key package", e))?;
        let leaf = kp.leaf_node();
        let (nid, device) = identity_of(leaf.credential())?;
        Ok((nid, device, leaf.signature_key().as_slice().to_vec()))
    }

    /// A copy of the OpenMLS storage, to undo a join or commit that fails a later check.
    pub fn snapshot(&self) -> Result<Snapshot> {
        let v = self.provider.storage().values.read();
        Ok(Snapshot(
            v.map_err(|_| mls_err("storage", "poisoned"))?.clone(),
        ))
    }

    pub fn restore(&self, s: Snapshot) -> Result<()> {
        let mut v = self.provider.storage().values.write();
        *v.as_deref_mut()
            .map_err(|_| mls_err("storage", "poisoned"))? = s.0;
        Ok(())
    }

    /// Decrypts a Welcome for one of this member's key packages without joining yet.
    /// OpenMLS consumes the key package here: take a [`Self::snapshot`] first. Joining
    /// replaces this member's old state of the same group (left when it was removed), so a
    /// re-add can be joined; callers join only a Welcome they verified. `None`: the Welcome
    /// is for none of this member's unused key packages; any other failure is an error.
    pub fn stage(&self, welcome: &[u8]) -> Result<Option<StagedWelcome>> {
        let body = MlsMessageIn::tls_deserialize_exact(welcome)
            .map_err(|e| mls_err("welcome", e))?
            .extract();
        let MlsMessageBodyIn::Welcome(w) = body else {
            return Err(mls_err("welcome", "not a Welcome"));
        };
        let built =
            StagedWelcome::build_from_welcome(&self.provider, create_config().join_config(), w);
        match built {
            Err(WelcomeError::NoMatchingKeyPackage) => Ok(None),
            Err(e) => Err(mls_err("welcome", e)),
            Ok(b) => b
                .replace_old_group()
                .build()
                .map(Some)
                .map_err(|e| mls_err("welcome", e)),
        }
    }

    /// `(epoch, content key, history key)` of a staged Welcome.
    pub fn staged_key(&self, s: &StagedWelcome, rid: &Rid) -> Result<(u64, Key, Key)> {
        let export = |label| -> Result<Key> {
            let k = s
                .export_secret(self.provider.crypto(), label, rid, 32)
                .map_err(|e| mls_err("export", e))?;
            k.as_slice().try_into().map_err(|e| mls_err("export", e))
        };
        Ok((
            s.group_context().epoch().as_u64(),
            export(CONTENT_LABEL)?,
            export(HISTORY_LABEL)?,
        ))
    }

    pub fn finish(&self, s: StagedWelcome) -> Result<MlsGroup> {
        s.into_group(&self.provider)
            .map_err(|e| mls_err("welcome", e))
    }

    /// Processes a commit; merges it only if `allow(sender nid)`. Returns the sender if it
    /// merged.
    pub fn process(
        &self,
        g: &mut MlsGroup,
        commit: &[u8],
        allow: impl Fn(&Nid) -> bool,
    ) -> Result<Option<Nid>> {
        let msg = MlsMessageIn::tls_deserialize_exact(commit)
            .map_err(|e| mls_err("commit", e))?
            .try_into_protocol_message()
            .map_err(|e| mls_err("commit", e))?;
        let processed = g
            .process_message(&self.provider, msg)
            .map_err(|e| mls_err("commit", e))?;
        let sender = match processed.sender() {
            Sender::Member(i) => g.member(*i).map(nid_of).transpose()?,
            _ => None,
        };
        let ProcessedMessageContent::StagedCommitMessage(sc) = processed.into_content() else {
            return Err(mls_err("commit", "not a commit"));
        };
        let Some(sender) = sender.filter(|n| allow(n)) else {
            return Ok(None);
        };
        g.merge_staged_commit(&self.provider, *sc)
            .map_err(|e| mls_err("merge", e))?;
        Ok(Some(sender))
    }

    fn export(&self, g: &MlsGroup, label: &str, rid: &Rid) -> Result<Key> {
        let s = g
            .export_secret(self.provider.crypto(), label, rid, 32)
            .map_err(|e| mls_err("export", e))?;
        s.as_slice().try_into().map_err(|e| mls_err("export", e))
    }

    /// `(epoch, content key, id key, history key)` of the epoch this member's pending commit
    /// in `g` makes, before it is merged; `None` without one.
    pub fn pending_keys(&self, g: &MlsGroup, rid: &Rid) -> Result<Option<(u64, Key, Key, Key)>> {
        let Some(s) = g.pending_commit() else {
            return Ok(None);
        };
        let export = |label| -> Result<Key> {
            let k = s
                .export_secret(self.provider.crypto(), label, rid, 32)
                .map_err(|e| mls_err("export", e))?;
            k.as_slice().try_into().map_err(|e| mls_err("export", e))
        };
        Ok(Some((
            s.group_context().epoch().as_u64(),
            export(CONTENT_LABEL)?,
            export(ID_LABEL)?,
            export(HISTORY_LABEL)?,
        )))
    }

    /// Merges this member's pending commit: it was seen first on chain.
    pub fn merge_pending(&self, g: &mut MlsGroup) -> Result<()> {
        g.merge_pending_commit(&self.provider)
            .map_err(|e| mls_err("merge", e))
    }

    /// Drops this member's pending commit: it can no longer land first.
    pub fn clear_pending(&self, g: &mut MlsGroup) -> Result<()> {
        g.clear_pending_commit(self.provider.storage())
            .map_err(|e| mls_err("clear", e))
    }

    /// `(epoch, export_secret("arkiv-git content", rid, 32))` of the group's current epoch.
    pub fn epoch_key(&self, g: &MlsGroup, rid: &Rid) -> Result<(u64, Key)> {
        Ok((g.epoch().as_u64(), self.export(g, CONTENT_LABEL, rid)?))
    }

    pub fn id_key(&self, g: &MlsGroup, rid: &Rid) -> Result<Key> {
        self.export(g, ID_LABEL, rid)
    }

    /// The key the current epoch's welcomes seal their history lockbox under.
    pub fn history_key(&self, g: &MlsGroup, rid: &Rid) -> Result<Key> {
        self.export(g, HISTORY_LABEL, rid)
    }
}

fn ser(m: &MlsMessageOut) -> Result<Vec<u8>> {
    m.tls_serialize_detached()
        .map_err(|e| mls_err("serialize", e))
}

/// The nid a basic credential names.
pub fn nid_of(c: &Credential) -> Result<Nid> {
    identity_of(c).map(|(n, _)| n)
}

/// The nid and device a basic credential names: `nid || device`, or `nid` alone for a
/// leaf made before leaves named their device.
pub fn identity_of(c: &Credential) -> Result<(Nid, Option<Nid>)> {
    let b = BasicCredential::try_from(c.clone()).map_err(|e| mls_err("credential", e))?;
    let id = b.identity();
    let nid = |b: &[u8]| -> Result<Nid> { b.try_into().map_err(|e| mls_err("credential", e)) };
    match id.len() {
        32 => Ok((nid(id)?, None)),
        64 => Ok((nid(&id[..32])?, Some(nid(&id[32..])?))),
        n => Err(mls_err("credential", format!("{n}-byte identity"))),
    }
}
