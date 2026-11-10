//! `/` (repos in this `$CHARTER_HOME`) and `/r/<rid>` (repo home).

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;
use charter_sync::ids::{hex32, to_text};

use super::verify::Verify;
use super::{App, Head, Layout, Repo, Theme, blocking, frame, html, open, seg, short};
use crate::gitread::{self, Entry};
use crate::sync_loop::cached_repos;
use crate::{cache, markdown};

pub struct RepoRow {
    pub hex: String,
    pub name: String,
    pub description: String,
    /// Block of the newest accepted anchor (the last push), if any.
    pub last_push: Option<u64>,
    pub verify: Verify,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct Index {
    pub l: Layout,
    pub repos: Vec<RepoRow>,
}

fn row(app: &App, rid: charter_core::Rid) -> RepoRow {
    let hex = hex32(&rid);
    let dir = app.home.join("repos").join(&hex);
    let view: Option<cache::RepoView> = cache::read(&dir.join(cache::VIEW)).ok().flatten();
    let last_push = view
        .iter()
        .flat_map(|v| &v.peers)
        .filter_map(|p| p.head.as_ref().map(|h| h.block))
        .max();
    RepoRow {
        name: view
            .as_ref()
            .map_or_else(|| short(&hex), |v| v.name.clone()),
        description: view
            .as_ref()
            .map(|v| v.description.clone())
            .unwrap_or_default(),
        verify: Verify::new(view.as_ref(), &app.shared.round(&rid)),
        hex,
        last_push,
    }
}

pub async fn index(State(app): State<Arc<App>>, theme: Theme) -> Response {
    blocking(move || {
        let repos = cached_repos(&app.home)
            .into_iter()
            .map(|rid| row(&app, rid))
            .collect();
        html(&Index {
            l: Layout::new(&app, &theme, "Repositories"),
            repos,
        })
    })
    .await
}

pub struct PeerRefs {
    pub nid: String,
    pub delegate: bool,
    pub refs: Vec<(String, String)>,
}

#[derive(Template)]
#[template(path = "home.html")]
pub struct Home {
    pub l: Layout,
    pub r: Head,
    pub clone: String,
    /// Canonical branch and tag names with their oid.
    pub canonical: Vec<(String, String)>,
    pub peers: Vec<PeerRefs>,
    pub rev: String,
    pub entries: Vec<Entry>,
    pub readme: Option<String>,
}

impl Home {
    pub fn seg(&self, s: &str) -> String {
        seg(s)
    }
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
    /// `refs/heads/main` to `main`.
    pub fn bare<'a>(&self, name: &'a str) -> &'a str {
        name.strip_prefix("refs/heads/")
            .or_else(|| name.strip_prefix("refs/tags/"))
            .unwrap_or(name)
    }
}

/// The README at the root of `commit`, as sanitized HTML.
fn readme(r: &Repo, commit: &str, entries: &[Entry]) -> Option<String> {
    let e = entries
        .iter()
        .find(|e| e.kind == "blob" && e.name.to_ascii_lowercase().starts_with("readme"))?;
    let bytes = gitread::blob(&r.git, commit, &e.name)?;
    let text = String::from_utf8_lossy(&bytes);
    let lower = e.name.to_ascii_lowercase();
    Some(
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
    )
}

pub fn home_page(app: &App, theme: &Theme, r: &Repo) -> Response {
    let (l, head) = frame(app, theme, r, "code", "Code");
    let default = r.default_ref();
    let commit = gitread::resolve(&r.git, r.view.as_ref(), &default);
    let entries = commit
        .as_deref()
        .and_then(|c| gitread::tree_with_last(&r.git, c, ""))
        .unwrap_or_default();
    let readme = commit.as_deref().and_then(|c| readme(r, c, &entries));
    let view = r.view.as_ref();
    html(&Home {
        l,
        r: head,
        clone: format!("git clone arkiv::{}", to_text(&r.rid)),
        canonical: view.map(|v| v.canonical.clone()).unwrap_or_default(),
        peers: view
            .iter()
            .flat_map(|v| &v.peers)
            .map(|p| PeerRefs {
                nid: p.nid.clone(),
                delegate: p.delegate,
                refs: p.refs.clone(),
            })
            .collect(),
        rev: seg(&default),
        entries,
        readme,
    })
}

pub async fn home(State(app): State<Arc<App>>, theme: Theme, Path(rid): Path<String>) -> Response {
    blocking(move || match open(&app, &theme, &rid) {
        Ok(r) => home_page(&app, &theme, &r),
        Err(resp) => resp,
    })
    .await
}
