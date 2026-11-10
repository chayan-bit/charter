//! `state.json` (high-water marks, applied packs, equivocation notes) and `pending.json`
//! (an unfinished push), both written by temp file, fsync, rename (design 3.9, 4.4).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use charter_core::{Equivocation, Hash32, IdMark, Nid, PeerMark};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ids::{hex32, unhex32};
use crate::{Result, SyncError};

const STATE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdMarkJson {
    pub rev: u64,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerMarkJson {
    pub seq: u64,
    pub hash: String,
    pub block: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EquivocationJson {
    pub nid: String,
    pub seq: u64,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub v: u32,
    pub chain_id: Option<u64>,
    pub identity: Option<IdMarkJson>,
    pub peers: BTreeMap<String, PeerMarkJson>,
    pub applied_packs: BTreeSet<String>,
    /// Manifest `h` values already applied (core's `HeadInputs::applied` is keyed by `h`).
    #[serde(default)]
    pub applied_manifests: BTreeSet<String>,
    pub equivocations: Vec<EquivocationJson>,
    /// Format v2: high-water mark of each device head, by device nid (head-model 6).
    #[serde(default)]
    pub devices: BTreeMap<String, PeerMarkJson>,
    /// Format v2: where each head body landed, by its `h` (head-model 5.4).
    #[serde(default)]
    pub landings: BTreeMap<String, LandingJson>,
}

/// The tx that carried a head body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandingJson {
    #[serde(with = "hex_tx")]
    pub tx: [u8; 32],
    pub block: u64,
    pub index: u32,
}

mod hex_tx {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let v = hex::decode(&s).map_err(serde::de::Error::custom)?;
        v.try_into()
            .map_err(|_| serde::de::Error::custom("tx hash is not 32 bytes"))
    }
}

impl Default for State {
    fn default() -> Self {
        Self {
            v: STATE_VERSION,
            chain_id: None,
            identity: None,
            peers: BTreeMap::new(),
            applied_packs: BTreeSet::new(),
            applied_manifests: BTreeSet::new(),
            equivocations: vec![],
            devices: BTreeMap::new(),
            landings: BTreeMap::new(),
        }
    }
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(read_json(path)?.unwrap_or_default())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }

    pub fn id_mark(&self) -> Result<Option<IdMark>> {
        self.identity
            .as_ref()
            .map(|m| {
                Ok(IdMark {
                    rev: m.rev,
                    hash: unhex32(&m.hash)?,
                })
            })
            .transpose()
    }

    pub fn peer_mark(&self, nid: &Nid) -> Result<Option<PeerMark>> {
        self.peers
            .get(&hex32(nid))
            .map(|m| {
                Ok(PeerMark {
                    seq: m.seq,
                    hash: unhex32(&m.hash)?,
                    block: m.block,
                })
            })
            .transpose()
    }

    pub fn set_id_mark(&mut self, m: &IdMark) {
        self.identity = Some(IdMarkJson {
            rev: m.rev,
            hash: hex32(&m.hash),
        });
    }

    pub fn set_peer_mark(&mut self, nid: &Nid, m: &PeerMark) {
        let j = PeerMarkJson {
            seq: m.seq,
            hash: hex32(&m.hash),
            block: m.block,
        };
        self.peers.insert(hex32(nid), j);
    }

    pub fn device_mark(&self, dev: &Nid) -> Result<Option<PeerMark>> {
        self.devices
            .get(&hex32(dev))
            .map(|m| {
                Ok(PeerMark {
                    seq: m.seq,
                    hash: unhex32(&m.hash)?,
                    block: m.block,
                })
            })
            .transpose()
    }

    pub fn set_device_mark(&mut self, dev: &Nid, m: &PeerMark) {
        let j = PeerMarkJson {
            seq: m.seq,
            hash: hex32(&m.hash),
            block: m.block,
        };
        self.devices.insert(hex32(dev), j);
    }

    pub fn landing(&self, h: &Hash32) -> Option<LandingJson> {
        self.landings.get(&hex32(h)).copied()
    }

    pub fn applied_manifest_set(&self) -> Result<BTreeSet<Hash32>> {
        self.applied_manifests.iter().map(|h| unhex32(h)).collect()
    }

    pub fn note_equivocations(&mut self, eqs: &[Equivocation]) {
        for e in eqs {
            let j = EquivocationJson {
                nid: hex32(&e.nid),
                seq: e.seq,
                keys: e.keys.iter().map(hex32).collect(),
            };
            if !self.equivocations.contains(&j) {
                self.equivocations.push(j);
            }
        }
    }
}

/// One pack of a pending push, kept in `outgoing/` until anchored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPack {
    pub file: String,
    pub pack_id: String,
}

/// What an unfinished push intends (design 4.2 step 5). Signed envelopes are stored as
/// sent, because signatures are randomized and re-signing would change every `h`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPush {
    pub v: u32,
    /// `(dst, new oid)`; `None` deletes the ref.
    pub updates: Vec<(String, Option<String>)>,
    pub packs: Vec<PendingPack>,
    /// Manifest envelopes, hex.
    pub manifests: Vec<String>,
    /// Refs envelope, hex.
    pub refs: String,
    pub seq: u64,
    /// `h` of the anchor this push extends (zero for seq 0), hex.
    pub prev: String,
    /// Anchor envelope once built (just before it is sent), hex.
    pub anchor: Option<String>,
}

impl PendingPush {
    pub fn load(path: &Path) -> Result<Option<Self>> {
        read_json(path)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_json(path, self)
    }
}

pub fn unhex(s: &str) -> Result<Vec<u8>> {
    hex::decode(s).map_err(|e| SyncError::io("pending.json", e))
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| SyncError::io(path.display(), e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SyncError::io(path.display(), e)),
    }
}

/// Temp file, fsync, rename: a crash leaves the old or the new file, never a torn one.
pub fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    // One temp file per write (process id and a per-process counter), so concurrent
    // writers never rename each other's half-written file into place.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let err = |e: std::io::Error| SyncError::io(path.display(), e);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{n}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(v).map_err(|e| SyncError::io(path.display(), e))?;
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(&bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    });
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use charter_core::Kind;

    #[test]
    fn concurrent_writes_to_one_file_all_land_whole() {
        let d = tempfile::tempdir().expect("tmp");
        let p = d.path().join("cache.json");
        let big: Vec<u64> = (0..20_000).collect();
        std::thread::scope(|s| {
            for t in 0..8u64 {
                let (p, big) = (&p, &big);
                s.spawn(move || {
                    for i in 0..40 {
                        write_json(p, &(t, i, big)).expect("write");
                    }
                });
            }
        });
        let back: (u64, u64, Vec<u64>) =
            serde_json::from_slice(&std::fs::read(&p).expect("read")).expect("whole json");
        assert_eq!(back.2, big);
        let left = std::fs::read_dir(d.path()).expect("dir").count();
        assert_eq!(left, 1, "no temp files left behind");
    }

    #[test]
    fn state_round_trips_and_marks_convert() {
        let d = tempfile::tempdir().expect("tmp");
        let p = d.path().join("state.json");
        assert_eq!(State::load(&p).expect("empty"), State::default());
        let mut s = State {
            chain_id: Some(7),
            ..State::default()
        };
        let nid = [5u8; 32];
        s.set_id_mark(&IdMark {
            rev: 2,
            hash: [1; 32],
        });
        let pm = PeerMark {
            seq: 4,
            hash: [2; 32],
            block: 99,
        };
        s.set_peer_mark(&nid, &pm);
        s.applied_manifests.insert(hex32(&[3; 32]));
        let eq = Equivocation {
            kind: Kind::Anchor,
            nid,
            seq: 1,
            keys: [[8; 32], [9; 32]],
        };
        s.note_equivocations(&[eq.clone(), eq]);
        s.save(&p).expect("save");
        let back = State::load(&p).expect("load");
        assert_eq!(back, s);
        assert_eq!(back.peer_mark(&nid).expect("ok"), Some(pm));
        assert_eq!(back.peer_mark(&[0; 32]).expect("ok"), None);
        assert_eq!(back.id_mark().expect("ok").map(|m| m.rev), Some(2));
        assert!(back.applied_manifest_set().expect("ok").contains(&[3; 32]));
        assert_eq!(back.equivocations.len(), 1);
        std::fs::write(&p, b"{").expect("w");
        assert!(State::load(&p).is_err());
    }

    #[test]
    fn pending_round_trips() {
        let d = tempfile::tempdir().expect("tmp");
        let p = d.path().join("pending.json");
        assert_eq!(PendingPush::load(&p).expect("none"), None);
        let pp = PendingPush {
            v: 1,
            updates: vec![("refs/heads/main".into(), Some("ab".into()))],
            packs: vec![],
            manifests: vec![],
            refs: "00".into(),
            seq: 0,
            prev: hex32(&[0; 32]),
            anchor: None,
        };
        pp.save(&p).expect("save");
        assert_eq!(PendingPush::load(&p).expect("load"), Some(pp));
        assert_eq!(unhex("0a").expect("hex"), vec![10]);
        assert!(unhex("x").is_err());
    }
}
