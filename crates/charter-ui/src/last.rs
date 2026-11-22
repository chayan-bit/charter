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

type Entries = Rc<HashMap<String, (ObjectId, bool)>>;

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
                let m = es.map(|e| {
                    (
                        e.filename.to_str_lossy().into_owned(),
                        (e.oid.to_owned(), e.mode.is_tree()),
                    )
                });
                Rc::new(m.collect())
            })
            .clone()
    }

    /// Collects every path under `tree` into `out` and populates ancestor directory counts in
    /// `dir_remaining`.
    fn collect_all(
        &mut self,
        tree: Option<ObjectId>,
        prefix: &str,
        out: &mut HashSet<String>,
        dir_remaining: &mut HashMap<String, usize>,
    ) {
        let entries = self.entries(tree);
        for (name, &(oid, is_tree)) in entries.iter() {
            let path = join_path(prefix, name);
            if is_tree {
                self.collect_all(Some(oid), &path, out, dir_remaining);
            }
            if out.insert(path.clone()) {
                for (i, _) in path.match_indices('/') {
                    *dir_remaining.entry(path[..i].to_owned()).or_insert(0) += 1;
                }
            }
        }
    }

    /// Adds to `out` every path in `want` under `prefix` whose entry differs between trees
    /// `a` and `b`, pruning subtrees with no remaining paths in `dir_remaining`.
    fn diff_wanted(
        &mut self,
        a: Option<ObjectId>,
        b: Option<ObjectId>,
        prefix: &str,
        want: &HashSet<String>,
        dir_remaining: &HashMap<String, usize>,
        out: &mut HashSet<String>,
    ) {
        if a == b {
            return;
        }
        let (ea, eb) = (self.entries(a), self.entries(b));
        for (name, &(oid_a, is_tree_a)) in ea.iter() {
            let other = eb.get(name).copied();
            if other.is_some_and(|(oid_b, _)| oid_b == oid_a) {
                continue;
            }
            self.visit_diff(
                prefix,
                name,
                is_tree_a.then_some(oid_a),
                other.filter(|e| e.1).map(|e| e.0),
                want,
                dir_remaining,
                out,
            );
        }
        for (name, &(oid_b, is_tree_b)) in eb.iter() {
            if ea.contains_key(name) {
                continue;
            }
            self.visit_diff(
                prefix,
                name,
                None,
                is_tree_b.then_some(oid_b),
                want,
                dir_remaining,
                out,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_diff(
        &mut self,
        prefix: &str,
        name: &str,
        fa: Option<ObjectId>,
        fb: Option<ObjectId>,
        want: &HashSet<String>,
        dir_remaining: &HashMap<String, usize>,
        out: &mut HashSet<String>,
    ) {
        let is_dir = fa.is_some() || fb.is_some();
        let (path, wanted_here, wanted_below) = if prefix.is_empty() {
            let here = want.contains(name);
            let below = is_dir && dir_remaining.contains_key(name);
            if !here && !below {
                return;
            }
            (name.to_owned(), here, below)
        } else {
            let path = format!("{prefix}/{name}");
            let here = want.contains(&path);
            let below = is_dir && dir_remaining.contains_key(&path);
            if !here && !below {
                return;
            }
            (path, here, below)
        };
        if wanted_below {
            self.diff_wanted(fa, fb, &path, want, dir_remaining, out);
        }
        if wanted_here {
            out.insert(path);
        }
    }
}

fn join_path(prefix: &str, name: &str) -> String {
    match prefix {
        "" => name.to_owned(),
        p => format!("{p}/{name}"),
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
    let mut dir_remaining = HashMap::new();
    trees.collect_all(tree_of(head), "", &mut want, &mut dir_remaining);
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
            trees.diff_wanted(mine, p, "", &want, &dir_remaining, &mut c);
            if let Some(prev) = &changed {
                c.retain(|x| prev.contains(x));
            }
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
            if want.remove(&p) {
                for (i, _) in p.match_indices('/') {
                    let dir = &p[..i];
                    if let Some(rem) = dir_remaining.get_mut(dir) {
                        *rem -= 1;
                        if *rem == 0 {
                            dir_remaining.remove(dir);
                        }
                    }
                }
            }
            tip.paths.insert(p, n);
        }
    }
    Some(tip)
}
