//! `/` (repos in this `$CHARTER_HOME`), `/r/<rid>` (repo home: Code) and
//! `/r/<rid>/branches` (every ref, so none needs a typed URL).

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;
use charter_sync::ids::hex32;

use super::people::Clock;
use super::verify::Verify;
use super::{
    App, Head, Layout, Pref, Repo, blocking, frame, html, last_push, open, people, seg, short,
};
use crate::gitread::{self, CommitRow, Entry};
use crate::sync_loop::cached_repos;
use crate::{cache, markdown};

pub struct RepoRow {
    pub hex: String,
    pub name: String,
    pub description: String,
    pub private: bool,
    /// `Updated 12 min ago by ravi`, from the newest signed head.
    pub updated: Option<(String, String, String)>,
    pub issues: usize,
    pub patches: usize,
    pub verify: Verify,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct Index {
    pub l: Layout,
    pub repos: Vec<RepoRow>,
}

impl Index {
    /// Repos that need a look: conflicts and failed syncs (exceptions only).
    pub fn attention(&self) -> Vec<&RepoRow> {
        self.repos
            .iter()
            .filter(|r| r.verify.state == "equivocation" || r.verify.error.is_some())
            .collect()
    }
}

fn row(app: &App, pref: &Pref, rid: charter_core::Rid) -> RepoRow {
    let hex = hex32(&rid);
    let dir = app.home.join("repos").join(&hex);
    let view: Option<cache::RepoView> = cache::read(&dir.join(cache::VIEW)).ok().flatten();
    let v = view.as_ref();
    let (clock, who) = (Clock::of(v), people(app, pref, v));
    RepoRow {
        name: v.map_or_else(|| short(&hex), |v| v.name.clone()),
        description: v.map(|v| v.description.clone()).unwrap_or_default(),
        private: v.is_some_and(|v| v.private),
        updated: last_push(v).map(|(b, nid)| (clock.when(b), clock.full(b), who.name(&nid))),
        issues: v.map_or(0, |v| v.issues),
        patches: v.map_or(0, |v| v.patches),
        verify: Verify::new(v, &app.shared.round(&rid)),
        hex,
    }
}

pub async fn index(State(app): State<Arc<App>>, pref: Pref) -> Response {
    app.shared.touch();
    blocking(move || {
        let repos = cached_repos(&app.home)
            .into_iter()
            .map(|rid| row(&app, &pref, rid))
            .collect();
        html(&Index {
            l: Layout::new(&app, &pref, "Repositories"),
            repos,
        })
    })
    .await
}

/// One branch or tag of the canonical refs.
#[derive(Debug, Clone)]
pub struct RefRow {
    /// `main`, `v1`.
    pub name: String,
    /// The name as one URL segment.
    pub seg: String,
    pub tag: bool,
    pub default: bool,
    pub tip: Option<CommitRow>,
}

/// Canonical branches (default first, then newest) and tags (newest first).
pub fn refs(r: &Repo) -> Vec<RefRow> {
    let default = r.default_ref();
    let canonical = r
        .view
        .as_ref()
        .map(|v| v.canonical.as_slice())
        .unwrap_or_default();
    let mut rows: Vec<RefRow> = canonical
        .iter()
        .filter_map(|(name, oid)| {
            let (tag, bare) = match name.strip_prefix("refs/tags/") {
                Some(t) => (true, t),
                None => (false, name.strip_prefix("refs/heads/")?),
            };
            Some(RefRow {
                name: bare.to_owned(),
                seg: seg(bare),
                tag,
                default: !tag && bare == default,
                tip: r.tip(oid),
            })
        })
        .collect();
    let time = |x: &RefRow| x.tip.as_ref().map_or(0, |t| t.time);
    rows.sort_by_key(|x| (x.tag, !x.default, std::cmp::Reverse(time(x))));
    rows
}

/// The branch picker of the Code pages.
#[derive(Debug, Clone, Default)]
pub struct Picker {
    /// What is shown: a branch, a tag or a short oid.
    pub current: String,
    pub refs: Vec<RefRow>,
    pub branches: usize,
    pub tags: usize,
}

impl Picker {
    pub fn new(r: &Repo, rev: &str) -> Self {
        let refs = refs(r);
        let current = rev.replace("%2F", "/").replace("%25", "%");
        Self {
            current: if gitread::is_oid(&current) {
                short(&current)
            } else {
                current
            },
            branches: refs.iter().filter(|x| !x.tag).count(),
            tags: refs.iter().filter(|x| x.tag).count(),
            refs,
        }
    }
}

#[derive(Template)]
#[template(path = "home.html")]
pub struct Home {
    pub l: Layout,
    pub r: Head,
    pub rev: String,
    pub picker: Picker,
    pub last: Option<CommitRow>,
    pub entries: Vec<Entry>,
    pub readme: Option<(String, String)>,
    /// How main may change, in a sentence.
    pub rule: String,
}

impl Home {
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
    pub fn age(&self, secs: &i64) -> String {
        super::age(*secs)
    }
    /// Path of a root entry (`entries.html` is shared with the tree page).
    pub fn join(&self, name: &str) -> String {
        name.to_owned()
    }
}

/// The repo's rule for changing the default branch, in a sentence.
fn rule(v: Option<&cache::RepoView>) -> String {
    let Some(v) = v else {
        return "Shown once the first sync finishes.".into();
    };
    if v.program.is_some() {
        return format!(
            "Every push to {} is checked against the repo's pinned rules.",
            v.default_branch
        );
    }
    match v.delegates.len() {
        0 | 1 => format!("Its maintainer signs every change to {}.", v.default_branch),
        n => format!(
            "{} of its {n} maintainers must sign a change to {}.",
            v.threshold, v.default_branch
        ),
    }
}

/// The README at the root of `commit`, as `(file name, sanitized HTML)`.
fn readme(r: &Repo, commit: &str, entries: &[Entry]) -> Option<(String, String)> {
    let e = entries
        .iter()
        .find(|e| e.kind == "blob" && e.name.to_ascii_lowercase().starts_with("readme"))?;
    let bytes = gitread::blob(&r.git, commit, &e.name)?;
    let text = String::from_utf8_lossy(&bytes);
    let lower = e.name.to_ascii_lowercase();
    Some((
        e.name.clone(),
        match lower.ends_with(".md") || lower.ends_with(".markdown") {
            true => markdown::render_in(
                &text,
                &markdown::Links {
                    rid: &r.hex,
                    rev: &seg(&r.default_ref()),
                    dir: "",
                },
            ),
            false => format!("<pre>{}</pre>", crate::highlight::escape(&text)),
        },
    ))
}

pub fn home_page(app: &App, pref: &Pref, r: &Repo) -> Response {
    let (l, head) = frame(app, pref, r, "code", "Code");
    let default = r.default_ref();
    let commit = gitread::resolve(&r.git, r.view.as_ref(), &default);
    let entries = commit
        .as_deref()
        .and_then(|c| Some(r.last.fill(c, "", gitread::tree(&r.git, c, "")?)))
        .unwrap_or_default();
    let readme = commit.as_deref().and_then(|c| readme(r, c, &entries));
    let last = commit.as_deref().and_then(|c| r.tip(c));
    html(&Home {
        l,
        r: head,
        picker: Picker::new(r, &seg(&default)),
        rev: seg(&default),
        last,
        entries,
        readme,
        rule: rule(r.view.as_ref()),
    })
}

/// A peer's own refs, as the branches page lists them.
pub struct PeerRefs {
    pub nid: String,
    pub delegate: bool,
    pub refs: Vec<(String, String)>,
}

#[derive(Template)]
#[template(path = "branches.html")]
pub struct Branches {
    pub l: Layout,
    pub r: Head,
    pub refs: Vec<RefRow>,
    pub peers: Vec<PeerRefs>,
}

impl Branches {
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
    pub fn age(&self, secs: &i64) -> String {
        super::age(*secs)
    }
    pub fn bare<'a>(&self, name: &'a str) -> &'a str {
        name.strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/tags/"))
            .unwrap_or(name)
    }
}

fn branches_page(app: &App, pref: &Pref, r: &Repo) -> Response {
    let (mut l, head) = frame(app, pref, r, "code", "Branches and tags");
    l.crumbs.push((String::new(), "branches".into()));
    let peers = r
        .view
        .iter()
        .flat_map(|v| &v.peers)
        .map(|p| PeerRefs {
            nid: p.nid.clone(),
            delegate: p.delegate,
            refs: p.refs.clone(),
        })
        .collect();
    html(&Branches {
        l,
        r: head,
        refs: refs(r),
        peers,
    })
}

pub async fn branches(
    State(app): State<Arc<App>>,
    pref: Pref,
    Path(rid): Path<String>,
) -> Response {
    blocking(move || match open(&app, &pref, &rid) {
        Ok(r) => branches_page(&app, &pref, &r),
        Err(resp) => resp,
    })
    .await
}

pub async fn home(State(app): State<Arc<App>>, pref: Pref, Path(rid): Path<String>) -> Response {
    blocking(move || match open(&app, &pref, &rid) {
        Ok(r) => home_page(&app, &pref, &r),
        Err(resp) => resp,
    })
    .await
}
