//! The newest commit that changed each path, per ref tip: built once per tip by the sync
//! round from one history walk (what `git log --name-only` reads), kept in `last.json`, and
//! only looked up while a page renders. A path changed in a commit when its entry differs
//! from that of every parent, as `git log -1 -- <path>` decides without history
//! simplification.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use gix::ObjectId;
use gix::bstr::ByteSlice;
use serde::{Deserialize, Serialize};

use crate::gitread::{self, CommitRow, Entry, Git};

pub const FILE: &str = "last.json";

/// Most commits one tip's walk reads; paths unchanged within them show no commit.
// shortcut: a bounded walk, so a path untouched for longer shows no bar; extend the
// previous tip's index instead of walking again if histories that deep need it.
pub const MAX_WALK: usize = 20_000;

/// One tip's index: every path of its tree (`""` is the root) to a row of `rows`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tip {
    rows: Vec<CommitRow>,
    paths: HashMap<String, u32>,
}

/// The indexes of a repo's ref tips, by commit id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Last {
    tips: HashMap<String, Tip>,
}

impl Last {
    /// The newest commit up to `commit` that changed `path`, if `commit` is an indexed tip.
    pub fn get(&self, commit: &str, path: &str) -> Option<CommitRow> {
        let t = self.tips.get(commit)?;
        t.rows.get(*t.paths.get(path)? as usize).cloned()
    }

    /// `entries` of the folder `dir` with their last commits filled in.
    pub fn fill(&self, commit: &str, dir: &str, entries: Vec<Entry>) -> Vec<Entry> {
        let path = |n: &str| match dir {
            "" => n.to_owned(),
            d => format!("{d}/{n}"),
        };
        let fill = |e: Entry| Entry {
            last: self.get(commit, &path(&e.name)),
            ..e
        };
        entries.into_iter().map(fill).collect()
    }

    /// The index of exactly `tips` (commit ids): a tip already in `old` is kept (a commit's
    /// history never changes), the others are built.
    pub fn refresh(git: &Git, old: Last, tips: impl IntoIterator<Item = String>) -> Last {
        let mut old = old.tips;
        let wanted: BTreeSet<String> = tips.into_iter().collect();
        let tips = wanted
            .into_iter()
            .filter_map(|c| {
                let t = old.remove(&c).or_else(|| build(git, &c))?;
                Some((c, t))
            })
            .collect();
        Last { tips }
    }
}

type Entries = Rc<Vec<(String, ObjectId, bool)>>;

/// Decoded trees, each read once per build.
struct Trees<'g> {
    git: &'g Git,
    seen: HashMap<ObjectId, Entries>,
}

impl Trees<'_> {
    fn entries(&mut self, tree: Option<ObjectId>) -> Entries {
        let Some(t) = tree else {
            return Rc::default();
        };
        let git = self.git;
        self.seen
            .entry(t)
            .or_insert_with(|| {
                let tree = git.find_tree(t).ok();
                let d = tree.as_ref().and_then(|t| t.decode().ok());
                let es = d.iter().flat_map(|d| d.entries.iter());
                let v = es.map(|e| {
                    (
                        e.filename.to_str_lossy().into_owned(),
                        e.oid.to_owned(),
                        e.mode.is_tree(),
                    )
                });
                Rc::new(v.collect())
            })
            .clone()
    }

    /// Adds to `out` every path under `prefix` whose entry differs between trees `a` and
    /// `b`, descending only into folders that differ.
    fn diff(
        &mut self,
        a: Option<ObjectId>,
        b: Option<ObjectId>,
        prefix: &str,
        out: &mut HashSet<String>,
    ) {
        if a == b {
            return;
        }
        let (ea, eb) = (self.entries(a), self.entries(b));
        let find = |es: &Entries, n: &str| es.iter().find(|e| e.0 == n).cloned();
        let names: BTreeSet<&str> = ea.iter().chain(eb.iter()).map(|e| e.0.as_str()).collect();
        for name in names {
            let (x, y) = (find(&ea, name), find(&eb, name));
            if x.as_ref().map(|e| e.1) == y.as_ref().map(|e| e.1) {
                continue;
            }
            let path = match prefix {
                "" => name.to_owned(),
                p => format!("{p}/{name}"),
            };
            let folder = |e: Option<(String, ObjectId, bool)>| e.filter(|e| e.2).map(|e| e.1);
            let (fa, fb) = (folder(x), folder(y));
            if fa.is_some() || fb.is_some() {
                self.diff(fa, fb, &path, out);
            }
            out.insert(path);
        }
    }
}

/// The index of the tip `commit`, from one walk that stops once every path is placed.
pub fn build(git: &Git, commit: &str) -> Option<Tip> {
    let tree_of = |c: ObjectId| Some(git.find_commit(c).ok()?.tree_id().ok()?.detach());
    let head = ObjectId::from_hex(commit.as_bytes()).ok()?;
    let mut trees = Trees {
        git,
        seen: HashMap::new(),
    };
    let mut want = HashSet::new();
    trees.diff(tree_of(head), None, "", &mut want);
    let mut tip = Tip {
        rows: vec![gitread::tip(git, commit)?],
        paths: HashMap::from([(String::new(), 0)]),
    };
    for info in gitread::walk(git, commit)?.take(MAX_WALK) {
        if want.is_empty() {
            break;
        }
        let mine = tree_of(info.id);
        let mut changed: Option<HashSet<String>> = None;
        let parents: Vec<Option<ObjectId>> = match info.parent_ids.is_empty() {
            true => vec![None],
            false => info.parent_ids.iter().map(|p| tree_of(*p)).collect(),
        };
        for p in parents {
            let mut c = HashSet::new();
            trees.diff(mine, p, "", &mut c);
            c.retain(|x| want.contains(x) && changed.as_ref().is_none_or(|s| s.contains(x)));
            let done = c.is_empty();
            changed = Some(c);
            if done {
                break;
            }
        }
        let hits = changed.unwrap_or_default();
        if hits.is_empty() {
            continue;
        }
        let n = match info.id == head {
            true => 0,
            false => {
                let Some(r) = gitread::row(git, info.id) else {
                    continue;
                };
                tip.rows.push(r);
                tip.rows.len() as u32 - 1
            }
        };
        for p in hits {
            want.remove(&p);
            tip.paths.insert(p, n);
        }
    }
    Some(tip)
}
