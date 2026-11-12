//! Issues and patches from the cached ops: lists and threads.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
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

/// `?label=`, `?assignee=` (peer hex) and `?milestone=` on a list: threads carrying it.
#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Filter {
    pub label: Option<String>,
    pub assignee: Option<String>,
    pub milestone: Option<String>,
}

impl Filter {
    fn keeps(&self, t: &Thread) -> bool {
        let tr = &t.triage;
        self.label.as_ref().is_none_or(|l| tr.labels.contains(l))
            && self
                .assignee
                .as_ref()
                .is_none_or(|a| tr.assignees.contains(a))
            && self
                .milestone
                .as_ref()
                .is_none_or(|m| tr.milestone.as_ref() == Some(m))
    }

    /// What the list is filtered by: `(what, value)`, the value a peer id for `assignee`.
    fn active(&self) -> Option<(&'static str, String)> {
        let pick = |w, v: &Option<String>| v.clone().map(|v| (w, v));
        pick("label", &self.label)
            .or_else(|| pick("assignee", &self.assignee))
            .or_else(|| pick("milestone", &self.milestone))
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
    /// The active filter, `(what, value)`.
    pub filter: Option<(&'static str, String)>,
}

/// `?label=<l>` with the value escaped for a URL query.
fn query(what: &str, v: &str) -> String {
    format!(
        "?{}",
        serde_urlencoded::to_string([(what, v)]).unwrap_or_default()
    )
}

impl Threads {
    pub fn q(&self, what: &str, v: &str) -> String {
        query(what, v)
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
    /// The list of this kind filtered by `what` = `v`.
    pub fn q(&self, what: &str, v: &str) -> String {
        format!("/r/{}/{}{}", self.r.hex, self.kind, query(what, v))
    }
    /// People the assignee form offers: maintainers, current assignees, and everyone who
    /// wrote on this thread, each once, maintainers first.
    pub fn assignable(&self) -> Vec<String> {
        let mut out: Vec<String> = self.r.verify.delegates.clone();
        let more = self.t.triage.assignees.iter().cloned();
        let authors = self.t.entries.iter().map(|e| e.author.clone());
        for p in more.chain(authors) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }
    /// The current labels as the form's comma list.
    pub fn label_list(&self) -> String {
        let l: Vec<&str> = self.t.triage.labels.iter().map(String::as_str).collect();
        l.join(", ")
    }
    pub fn assignee_list(&self) -> String {
        let a: Vec<&str> = self.t.triage.assignees.iter().map(String::as_str).collect();
        a.join(",")
    }
}

fn type_of(kind: &str) -> &'static str {
    if kind == "patches" {
        fold::PATCH
    } else {
        fold::ISSUE
    }
}

fn list_page(app: &App, pref: &Pref, r: &Repo, kind: &'static str, f: &Filter) -> Response {
    let ops = r.ops();
    let mut threads = fold::threads(&ops, type_of(kind), r.view.as_ref());
    threads.retain(|t| f.keeps(t));
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
        filter: f.active(),
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

async fn list(app: Arc<App>, pref: Pref, rid: String, kind: &'static str, f: Filter) -> Response {
    blocking(move || match open(&app, &pref, &rid) {
        Ok(r) => list_page(&app, &pref, &r, kind, &f),
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

pub async fn issues(
    State(a): State<Arc<App>>,
    t: Pref,
    Path(rid): Path<String>,
    Query(f): Query<Filter>,
) -> Response {
    list(a, t, rid, "issues", f).await
}

pub async fn patches(
    State(a): State<Arc<App>>,
    t: Pref,
    Path(rid): Path<String>,
    Query(f): Query<Filter>,
) -> Response {
    list(a, t, rid, "patches", f).await
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
