//! Read-only git over a repo's bare cache, in process with gix (docs/p6/ui.md 4.1): no
//! subprocess and no lock on the request path. Every ref and path from a URL is validated
//! here; refs resolve only through the verified canonical refs or a full oid.

use std::path::Path;

use gix::ObjectId;
use gix::bstr::ByteSlice;

use crate::cache::RepoView;

/// A thread-local handle on one repo's cache.
pub type Git = gix::Repository;

/// Opens `<repo_dir>/git`, ignoring global and system git config.
pub fn open(repo_dir: &Path) -> Option<gix::ThreadSafeRepository> {
    let opts = gix::open::Options::isolated();
    gix::ThreadSafeRepository::open_opts(repo_dir.join("git"), opts).ok()
}

/// A handle for this thread, with an object cache for walks that revisit commits.
pub fn local(ts: &gix::ThreadSafeRepository) -> Git {
    let mut g = ts.to_thread_local();
    g.object_cache_size_if_unset(4 << 20);
    g
}

/// 40 or 64 lowercase hex digits.
pub fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn oid(s: &str) -> Option<ObjectId> {
    is_oid(s).then(|| ObjectId::from_hex(s.as_bytes()).ok())?
}

/// A repo-relative path with no empty, `.` or `..` segment and no control bytes; `""` is
/// the root. `None` means the path is refused.
pub fn clean_path(p: &str) -> Option<String> {
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let ok = segs
        .iter()
        .all(|s| *s != "." && *s != ".." && !s.bytes().any(|b| b < 0x20 || b == 0x7f));
    ok.then(|| segs.join("/"))
}

/// The commit a URL ref names: an oid, or a branch or tag of the canonical refs.
pub fn resolve(git: &Git, view: Option<&RepoView>, r: &str) -> Option<String> {
    let candidate = if is_oid(r) {
        r.to_owned()
    } else {
        let refs = view.map(|v| v.canonical.as_slice()).unwrap_or_default();
        let names = [
            format!("refs/heads/{r}"),
            format!("refs/tags/{r}"),
            r.to_owned(),
        ];
        names
            .iter()
            .find_map(|n| refs.iter().find(|(name, _)| name == n))
            .map(|(_, o)| o.clone())?
    };
    let c = git
        .find_object(oid(&candidate)?)
        .ok()?
        .peel_to_commit()
        .ok()?;
    Some(c.id.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `blob`, `tree` or `commit` (a submodule).
    pub kind: &'static str,
    pub oid: String,
    pub size: Option<u64>,
    pub name: String,
    /// The newest commit that changed it, when the page asked ([`crate::last`]).
    pub last: Option<CommitRow>,
}

/// The tree object at `<commit>:<path>`, `None` if it is missing or not a tree.
fn tree_at<'r>(git: &'r Git, commit: &str, path: &str) -> Option<gix::Tree<'r>> {
    let root = git.find_commit(oid(commit)?).ok()?.tree().ok()?;
    if path.is_empty() {
        return Some(root);
    }
    let e = root.lookup_entry_by_path(path).ok()??;
    e.mode()
        .is_tree()
        .then(|| git.find_tree(e.object_id()).ok())?
}

/// Entries of the tree at `<commit>:<path>`, directories first, then by name; `None` if
/// there is no directory there.
pub fn tree(git: &Git, commit: &str, path: &str) -> Option<Vec<Entry>> {
    let t = tree_at(git, commit, path)?;
    let mut v: Vec<Entry> = t
        .decode()
        .ok()?
        .entries
        .iter()
        .map(|e| {
            let kind = match e.mode {
                m if m.is_tree() => "tree",
                m if m.is_commit() => "commit",
                _ => "blob",
            };
            let size = (kind == "blob")
                .then(|| git.find_header(e.oid).ok().map(|h| h.size()))
                .flatten();
            Entry {
                kind,
                oid: e.oid.to_string(),
                size,
                name: e.filename.to_str_lossy().into_owned(),
                last: None,
            }
        })
        .collect();
    v.sort_by(|a, b| (a.kind != "tree", &a.name).cmp(&(b.kind != "tree", &b.name)));
    Some(v)
}

/// Bytes of the blob at `<commit>:<path>`.
pub fn blob(git: &Git, commit: &str, path: &str) -> Option<Vec<u8>> {
    let root = git.find_commit(oid(commit)?).ok()?.tree().ok()?;
    let e = root.lookup_entry_by_path(path).ok()??;
    let blob = e
        .mode()
        .is_blob_or_symlink()
        .then(|| git.find_blob(e.object_id()).ok())??;
    Some(blob.detach().data)
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CommitRow {
    pub oid: String,
    pub author: String,
    pub time: i64,
    pub subject: String,
}

pub(crate) fn row(git: &Git, id: ObjectId) -> Option<CommitRow> {
    let c = git.find_commit(id).ok()?;
    let a = c.author().ok()?;
    Some(CommitRow {
        oid: id.to_string(),
        author: a.name.trim().to_str_lossy().into_owned(),
        time: a.seconds(),
        subject: c.message().ok()?.summary().to_str_lossy().into_owned(),
    })
}

/// The row of `commit` (an oid; an annotated tag is peeled to its commit).
pub fn tip(git: &Git, commit: &str) -> Option<CommitRow> {
    let c = git.find_object(oid(commit)?).ok()?.peel_to_commit().ok()?;
    row(git, c.id)
}

/// Commits reachable from `commit`, newest commit time first (as `git log` orders them).
pub(crate) fn walk<'r>(
    git: &'r Git,
    commit: &str,
) -> Option<impl Iterator<Item = gix::revision::walk::Info<'r>> + use<'r>> {
    use gix::revision::walk::Sorting;
    let w = git
        .rev_walk([oid(commit)?])
        .sorting(Sorting::ByCommitTime(Default::default()))
        .all()
        .ok()?;
    Some(w.filter_map(Result::ok))
}

/// Up to `n` commits from `commit` after skipping `skip`, newest first.
pub fn log(git: &Git, commit: &str, skip: usize, n: usize) -> Vec<CommitRow> {
    let Some(w) = walk(git, commit) else {
        return vec![];
    };
    w.skip(skip)
        .take(n)
        .filter_map(|i| row(git, i.id))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub oid: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_time: i64,
    pub committer: String,
    pub message: String,
    /// The commit object carries a `gpgsig` header (not checked: no gpg on this path).
    pub git_signed: bool,
}

/// `Name <mail>`.
fn person(s: &gix::actor::SignatureRef<'_>) -> String {
    format!(
        "{} <{}>",
        s.name.trim().to_str_lossy(),
        s.email.to_str_lossy()
    )
}

pub fn commit(git: &Git, oid_hex: &str) -> Option<CommitInfo> {
    let c = git.find_commit(oid(oid_hex)?).ok()?;
    let d = c.decode().ok()?;
    let (author, committer) = (d.author().ok()?, d.committer().ok()?);
    Some(CommitInfo {
        oid: c.id.to_string(),
        parents: c.parent_ids().map(|p| p.to_string()).collect(),
        author: person(&author),
        author_time: author.seconds(),
        committer: person(&committer),
        message: d.message.to_str_lossy().into_owned(),
        git_signed: d.extra_headers().pgp_signature().is_some(),
    })
}

/// Whether `a` is an ancestor of (or equal to) `b`.
pub fn is_ancestor(git: &Git, a: &str, b: &str) -> bool {
    let (Some(a), Some(b)) = (oid(a), oid(b)) else {
        return false;
    };
    git.merge_base(a, b)
        .ok()
        .flatten()
        .is_some_and(|m| m.detach() == a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_oids_are_validated() {
        assert_eq!(clean_path("a//b/").as_deref(), Some("a/b"));
        assert_eq!(clean_path("").as_deref(), Some(""));
        for bad in ["../etc", "a/../../b", "./a", "a\nb", "a\0b"] {
            assert_eq!(clean_path(bad), None, "{bad:?}");
        }
        assert!(is_oid(&"a".repeat(40)) && !is_oid(&"A".repeat(40)) && !is_oid("--all"));
    }
}
