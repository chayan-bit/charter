//! Issues and patches from the cached ops: lists and threads.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{App, Head, Layout, Repo, Theme, blocking, error_page, frame, html, open, short};
use crate::fold::{self, Thread};

#[derive(Template)]
#[template(path = "threads.html")]
pub struct Threads {
    pub l: Layout,
    pub r: Head,
    /// `issues` or `patches`.
    pub kind: &'static str,
    pub threads: Vec<Thread>,
}

impl Threads {
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
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
    pub fn ago(&self, ms: &u64) -> String {
        super::ago(*ms)
    }
}

fn type_of(kind: &str) -> &'static str {
    if kind == "patches" {
        fold::PATCH
    } else {
        fold::ISSUE
    }
}

fn list_page(app: &App, theme: &Theme, r: &Repo, kind: &'static str) -> Response {
    let ops = r.ops();
    let threads = fold::threads(&ops, type_of(kind), r.view.as_ref());
    let title = if kind == "patches" {
        "Patches"
    } else {
        "Issues"
    };
    let (mut l, head) = frame(app, theme, r, kind, title);
    l.crumbs.push((String::new(), kind.to_owned()));
    html(&Threads {
        l,
        r: head,
        kind,
        threads,
    })
}

fn thread_page(app: &App, theme: &Theme, r: &Repo, kind: &'static str, id: &str) -> Response {
    let ops = r.ops();
    let found = fold::threads(&ops, type_of(kind), r.view.as_ref())
        .into_iter()
        .find(|t| t.id == id);
    let Some(t) = found else {
        return error_page(
            app,
            theme,
            StatusCode::NOT_FOUND,
            "No such thread among the verified ops.",
        );
    };
    let (mut l, head) = frame(app, theme, r, kind, &t.title);
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

async fn list(app: Arc<App>, theme: Theme, rid: String, kind: &'static str) -> Response {
    blocking(move || match open(&app, &theme, &rid) {
        Ok(r) => list_page(&app, &theme, &r, kind),
        Err(e) => e,
    })
    .await
}

async fn one(app: Arc<App>, theme: Theme, rid: String, id: String, kind: &'static str) -> Response {
    blocking(move || match open(&app, &theme, &rid) {
        Ok(r) => thread_page(&app, &theme, &r, kind, &id),
        Err(e) => e,
    })
    .await
}

pub async fn issues(State(a): State<Arc<App>>, t: Theme, Path(rid): Path<String>) -> Response {
    list(a, t, rid, "issues").await
}

pub async fn patches(State(a): State<Arc<App>>, t: Theme, Path(rid): Path<String>) -> Response {
    list(a, t, rid, "patches").await
}

pub async fn issue(
    State(a): State<Arc<App>>,
    t: Theme,
    Path((rid, id)): Path<(String, String)>,
) -> Response {
    one(a, t, rid, id, "issues").await
}

pub async fn patch(
    State(a): State<Arc<App>>,
    t: Theme,
    Path((rid, id)): Path<(String, String)>,
) -> Response {
    one(a, t, rid, id, "patches").await
}
