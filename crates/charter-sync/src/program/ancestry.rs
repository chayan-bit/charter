//! Git ancestry answers that are `true`, kept next to the git cache: a commit's ancestry
//! never changes, so a proven `base -> oid` never needs `git merge-base` again. `false` and
//! undecided answers are never kept (the objects that decide them may arrive later).

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::git::Git;

const FILE: &str = "arkiv-ancestry";

/// Proven pairs per cache directory, loaded once per process.
static KNOWN: Mutex<Option<HashMap<PathBuf, BTreeSet<String>>>> = Mutex::new(None);

fn hex_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn load(dir: &std::path::Path) -> BTreeSet<String> {
    let text = std::fs::read_to_string(dir.join(FILE)).unwrap_or_default();
    // A torn last line fails the shape check and is ignored.
    text.lines()
        .filter(|l| l.split_once(' ').is_some_and(|(a, b)| hex_oid(a) && hex_oid(b)))
        .map(str::to_owned)
        .collect()
}

/// Whether `pair` ("base oid", lowercase hex) was proven in `git` before.
pub fn known(git: &Git, pair: &str) -> bool {
    let (Some(dir), Ok(mut guard)) = (git.dir(), KNOWN.lock()) else {
        return false;
    };
    let map = guard.get_or_insert_with(HashMap::new);
    map.entry(dir.to_path_buf())
        .or_insert_with(|| load(dir))
        .contains(pair)
}

/// Keeps a proven `pair`; a cache that cannot be written only costs a git call later.
pub fn remember(git: &Git, pair: &str) {
    let (Some(dir), Ok(mut guard)) = (git.dir(), KNOWN.lock()) else {
        return;
    };
    let map = guard.get_or_insert_with(HashMap::new);
    let set = map.entry(dir.to_path_buf()).or_insert_with(|| load(dir));
    if set.insert(pair.to_owned()) {
        let line = format!("{pair}\n");
        let r = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(FILE))
            .and_then(|mut f| f.write_all(line.as_bytes()));
        if let Err(e) = r {
            crate::warn(&format!("ancestry cache: {e}"));
        }
    }
}
