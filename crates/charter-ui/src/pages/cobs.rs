//! Issues and patches from the cached ops: lists and threads.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{App, Head, Layout, Pref, Repo, blocking, error_page, frame, html, open, short};
use crate::fold::{self, Thread};
use crate::gitread;
use crate::prefs::Merged;

/// Open patches whose latest head the default branch already contains read as merged,
/// unless Settings asks for merge ops only. Local git only.
fn infer_merged(r: &Repo, pref: &Pref, threads: &mut [Thread]) {
    if pref.0.merged != Merged::Infer {
        return;
    }
    let Some(main) = gitread::resolve(&r.git, r.view.as_ref(), &r.default_ref()) else {
        return;
    };
    for t in threads.iter_mut().filter(|t| t.state == "open") {
        if t.revisions
            .last()
            .is_some_and(|h| gitread::is_ancestor(&r.git, h, &main))
        {
            (t.state, t.inferred) = ("merged", true);
        }
    }
}

#[derive(Template)]
#[template(path = "threads.html")]
pub struct Threads {
    pub l: Layout,
    pub r: Head,
    /// `issues` or `patches`.
    pub kind: &'static str,
    pub threads: Vec<Thread>,
}

#[derive(Template)]
#[template(path = "thread.html")]
pub struct ThreadPage {
    pub l: Layout,
    pub r: Head,
    /// `issues` or `patches`.
    pub kind: &'static str,
    pub t: Thread,
}

impl ThreadPage {
    /// This page, where a write form returns to.
    pub fn back(&self) -> String {
        format!("/r/{}/{}/{}", self.r.hex, self.kind, self.t.id)
    }
    /// The patch's latest revision head, which a review approves.
    pub fn head(&self) -> &str {
        self.t.revisions.last().map_or("", String::as_str)
    }
    /// The issue the patch closes, if its create op names one.
    pub fn closes(&self) -> &str {
        let fields = self.t.entries.first().map(|e| e.fields.as_slice());
        let hit = fields
            .unwrap_or_default()
            .iter()
            .find(|(k, _)| k == "closes");
        hit.map_or("", |(_, v)| v.as_str())
    }
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
}

fn type_of(kind: &str) -> &'static str {
    if kind == "patches" {
        fold::PATCH
    } else {
        fold::ISSUE
    }
}

fn list_page(app: &App, pref: &Pref, r: &Repo, kind: &'static str) -> Response {
    let ops = r.ops();
    let mut threads = fold::threads(&ops, type_of(kind), r.view.as_ref());
    if kind == "patches" {
        infer_merged(r, pref, &mut threads);
    }
    let title = if kind == "patches" {
        "Patches"
    } else {
        "Issues"
    };
    let (mut l, head) = frame(app, pref, r, kind, title);
    l.crumbs.push((String::new(), kind.to_owned()));
    html(&Threads {
        l,
        r: head,
        kind,
        threads,
    })
}

fn thread_page(app: &App, pref: &Pref, r: &Repo, kind: &'static str, id: &str) -> Response {
    let ops = r.ops();
    let mut found: Vec<Thread> = fold::threads(&ops, type_of(kind), r.view.as_ref())
        .into_iter()
        .filter(|t| t.id == id)
        .collect();
    if kind == "patches" {
        infer_merged(r, pref, &mut found);
    }
    let Some(t) = found.pop() else {
        return error_page(
            app,
            pref,
            StatusCode::NOT_FOUND,
            "No such thread among the verified ops.",
        );
    };
    let (mut l, head) = frame(app, pref, r, kind, &t.title);
    l.crumbs
        .push((format!("/r/{}/{kind}", r.hex), kind.to_owned()));
    l.crumbs.push((String::new(), short(&t.id)));
    html(&ThreadPage {
        l,
        r: head,
        kind,
        t,
    })
}

async fn list(app: Arc<App>, pref: Pref, rid: String, kind: &'static str) -> Response {
    blocking(move || match open(&app, &pref, &rid) {
        Ok(r) => list_page(&app, &pref, &r, kind),
        Err(e) => e,
    })
    .await
}

async fn one(app: Arc<App>, pref: Pref, rid: String, id: String, kind: &'static str) -> Response {
    blocking(move || match open(&app, &pref, &rid) {
        Ok(r) => thread_page(&app, &pref, &r, kind, &id),
        Err(e) => e,
    })
    .await
}

pub async fn issues(State(a): State<Arc<App>>, t: Pref, Path(rid): Path<String>) -> Response {
    list(a, t, rid, "issues").await
}

pub async fn patches(State(a): State<Arc<App>>, t: Pref, Path(rid): Path<String>) -> Response {
    list(a, t, rid, "patches").await
}

pub async fn issue(
    State(a): State<Arc<App>>,
    t: Pref,
    Path((rid, id)): Path<(String, String)>,
) -> Response {
    one(a, t, rid, id, "issues").await
}

pub async fn patch(
    State(a): State<Arc<App>>,
    t: Pref,
    Path((rid, id)): Path<(String, String)>,
) -> Response {
    one(a, t, rid, id, "patches").await
}
