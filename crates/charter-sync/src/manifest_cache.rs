//! Facts of snapshot manifests by `h` (stored size and git pack trailer), kept in
//! `git/arkiv-manifests.json`. `h` hashes the signed manifest, so its facts never change
//! and a push asks Arkiv for each manifest at most once (head-model 4.4 snapshot policy).

use std::collections::BTreeMap;

use charter_core::{Device, Hash32, Rid};
use charter_store::Arkiv;
use serde::{Deserialize, Serialize};

use crate::ids::hex32;
use crate::repo::Repo;
use crate::state::{read_json, write_json};
use crate::{Result, pieces};

/// What a push needs from a manifest without reading it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub size: u64,
    /// Hex git pack trailer: the cache names the indexed pack `pack-<trailer>`.
    pub trailer: String,
}

type Cache = BTreeMap<String, Facts>;

fn path(repo: &Repo) -> std::path::PathBuf {
    repo.git_dir().join("arkiv-manifests.json")
}

fn load(repo: &Repo) -> Cache {
    // A missing or unreadable cache only costs one query.
    read_json(&path(repo)).ok().flatten().unwrap_or_default()
}

/// Records `facts` of manifests this device just signed (or read).
pub fn record(repo: &Repo, facts: &[(Hash32, Facts)]) -> Result<()> {
    if facts.is_empty() {
        return Ok(());
    }
    let mut cache = load(repo);
    cache.extend(facts.iter().map(|(h, f)| (hex32(h), f.clone())));
    write_json(&path(repo), &cache)
}

/// Records the manifests `hashes` of the snapshot packs `files` this push just wrote, in
/// the same order; `Git::snapshot_packs` names each file `<prefix>-<trailer>.pack`.
pub fn record_files(
    repo: &Repo,
    hashes: &[Hash32],
    files: &[(std::path::PathBuf, Hash32)],
) -> Result<()> {
    let facts = hashes
        .iter()
        .zip(files)
        .map(|(h, (f, _))| {
            let size = std::fs::metadata(f)
                .map_err(|e| crate::SyncError::io(f.display(), e))?
                .len();
            let trailer = f
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit_once('-'))
                .map(|(_, t)| t.to_owned())
                .unwrap_or_default();
            Ok((*h, Facts { size, trailer }))
        })
        .collect::<Result<Vec<_>>>()?;
    record(repo, &facts)
}

/// Facts of `dev`'s snapshot manifests `hashes`, in order: cached ones from disk, the rest
/// from Arkiv (verified as in [`pieces::snapshot_manifests`]) and then cached.
pub async fn facts<A: Arkiv>(
    a: &A,
    repo: &Repo,
    rid: &Rid,
    dev: &Device,
    hashes: &[Hash32],
) -> Result<Vec<Facts>> {
    let cache = load(repo);
    let missing: Vec<Hash32> = hashes
        .iter()
        .filter(|h| !cache.contains_key(&hex32(h)))
        .copied()
        .collect();
    let read: Vec<(Hash32, Facts)> = pieces::snapshot_manifests(a, rid, dev, &missing)
        .await?
        .into_iter()
        .map(|(h, m)| {
            let f = Facts {
                size: m.size,
                trailer: hex::encode(&m.trailer),
            };
            (h, f)
        })
        .collect();
    record(repo, &read)?;
    let all: BTreeMap<String, Facts> = cache
        .into_iter()
        .chain(read.into_iter().map(|(h, f)| (hex32(&h), f)))
        .collect();
    Ok(hashes
        .iter()
        .filter_map(|h| all.get(&hex32(h)).cloned())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_facts_survive_a_reload_and_merge() {
        let home = tempfile::tempdir().expect("tmp");
        let repo = Repo::open(home.path(), &[3; 32]).expect("repo");
        let f = |n| Facts {
            size: n,
            trailer: "ab".repeat(20),
        };
        record(&repo, &[([1; 32], f(10))]).expect("record");
        record(&repo, &[([2; 32], f(20))]).expect("record");
        let cache = load(&repo);
        assert_eq!(cache.get(&hex32(&[1; 32])), Some(&f(10)));
        assert_eq!(cache.get(&hex32(&[2; 32])), Some(&f(20)));
    }

    #[tokio::test]
    async fn cached_manifests_are_answered_without_a_query() {
        let home = tempfile::tempdir().expect("tmp");
        let rid = [3; 32];
        let repo = Repo::open(home.path(), &rid).expect("repo");
        let keys = crate::Keys::from_seed(charter_crypto::MasterSeed::from_bytes([7; 32]));
        let dev = Device {
            nid: keys.nid(),
            pk: keys.pk().clone(),
            control: [1; 20],
            bulk: [2; 20],
            h: charter_core::ZERO,
            position: charter_core::Position {
                block: 0,
                tx: 0,
                key: charter_core::ZERO,
            },
        };
        // An empty chain: any query for these manifests fails `E_NOT_LIVE`.
        let mem = charter_store::fake::MemArkiv::new(7);
        let f = Facts {
            size: 10,
            trailer: "cd".repeat(20),
        };
        record(&repo, &[([1; 32], f.clone())]).expect("record");
        let got = facts(&mem, &repo, &rid, &dev, &[[1; 32]]).await;
        assert_eq!(got.expect("cached"), vec![f]);
        assert!(facts(&mem, &repo, &rid, &dev, &[[9; 32]]).await.is_err());
    }
}
