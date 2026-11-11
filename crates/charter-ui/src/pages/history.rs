//! `/r/<rid>/log/<rev>` (paged commit list) and `/r/<rid>/commit/<sha>`.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use super::{App, Head, Layout, Pref, Repo, blocking, error_page, frame, html, open, short};
use crate::diff::{self, FileDiff};
use crate::gitread::{self, CommitInfo, CommitRow};
use crate::highlight::HIGHLIGHT_LINES;

pub const PAGE: usize = 50;

#[derive(Debug, Deserialize, Default)]
pub struct Paging {
    pub page: Option<usize>,
    pub all: Option<u8>,
}

/// Commits of one day.
pub struct Day {
    pub date: String,
    pub commits: Vec<CommitRow>,
}

#[derive(Template)]
#[template(path = "log.html")]
pub struct Log {
    pub l: Layout,
    pub r: Head,
    pub days: Vec<Day>,
    pub page: usize,
    pub next: bool,
}

impl Log {
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
    pub fn age(&self, secs: &i64) -> String {
        super::age(*secs)
    }
}

fn by_day(rows: Vec<CommitRow>) -> Vec<Day> {
    let mut days: Vec<Day> = vec![];
    for c in rows {
        let d = super::people::date(u64::try_from(c.time).unwrap_or(0) * 1000);
        match days.last_mut() {
            Some(last) if last.date == d => last.commits.push(c),
            _ => days.push(Day {
                date: d,
                commits: vec![c],
            }),
        }
    }
    days
}

fn log_page(app: &App, pref: &Pref, r: &Repo, rev: &str, page: usize) -> Response {
    let Some(commit) = gitread::resolve(&r.git, r.view.as_ref(), rev) else {
        return error_page(
            app,
            pref,
            StatusCode::NOT_FOUND,
            "No such branch, tag or commit.",
        );
    };
    let page = page.max(1);
    let mut rows = gitread::log(&r.git, &commit, (page - 1) * PAGE, PAGE + 1);
    let next = rows.len() > PAGE;
    rows.truncate(PAGE);
    let (mut l, head) = frame(app, pref, r, "log", "Commits");
    l.crumbs.push((
        String::new(),
        format!(
            "log {}",
            if gitread::is_oid(rev) {
                short(rev)
            } else {
                rev.to_owned()
            }
        ),
    ));
    html(&Log {
        l,
        r: head,
        days: by_day(rows),
        page,
        next,
    })
}

pub async fn log(
    State(app): State<Arc<App>>,
    pref: Pref,
    Path((rid, rev)): Path<(String, String)>,
    Query(q): Query<Paging>,
) -> Response {
    blocking(move || match open(&app, &pref, &rid) {
        Ok(r) => log_page(&app, &pref, &r, &rev, q.page.unwrap_or(1)),
        Err(e) => e,
    })
    .await
}

/// A verified peer head that contains the commit.
pub struct Anchor {
    pub nid: String,
    pub delegate: bool,
    pub seq: u64,
    pub block: u64,
    pub key: String,
    pub refs: Vec<String>,
}

#[derive(Template)]
#[template(path = "commit.html")]
pub struct Commit {
    pub l: Layout,
    pub r: Head,
    pub c: CommitInfo,
    pub subject: String,
    pub body: String,
    pub date: String,
    pub anchors: Vec<Anchor>,
    pub files: Vec<FileDiff>,
    pub hidden: usize,
}

impl Commit {
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
    /// Diff text escaped for element content only (it never lands in an attribute), which
    /// keeps quotes as one byte on long diffs.
    pub fn esc(&self, s: &str) -> String {
        crate::highlight::escape_text(s)
    }
}

/// Peers whose accepted head refs reach `oid`, with the refs that do.
fn anchors(r: &Repo, oid: &str) -> Vec<Anchor> {
    let Some(v) = &r.view else { return vec![] };
    v.peers
        .iter()
        .filter_map(|p| {
            let h = p.head.as_ref()?;
            let refs: Vec<String> = p
                .refs
                .iter()
                .filter(|(_, o)| gitread::is_oid(o) && gitread::is_ancestor(&r.git, oid, o))
                .map(|(n, _)| n.clone())
                .collect();
            (!refs.is_empty()).then(|| Anchor {
                nid: p.nid.clone(),
                delegate: p.delegate,
                seq: h.seq,
                block: h.block,
                key: h.key.clone(),
                refs,
            })
        })
        .collect()
}

fn commit_page(app: &App, pref: &Pref, r: &Repo, sha: &str, all: bool) -> Response {
    let found = gitread::is_oid(sha)
        .then(|| gitread::resolve(&r.git, None, sha))
        .flatten()
        .and_then(|oid| gitread::commit(&r.git, &oid));
    let Some(c) = found else {
        return error_page(
            app,
            pref,
            StatusCode::NOT_FOUND,
            "No such commit in the cache.",
        );
    };
    let limit = if all { usize::MAX } else { HIGHLIGHT_LINES };
    let (files, hidden) = diff::parse(&diff::of_commit(&r.git, &c.oid).unwrap_or_default(), limit);
    let (subject, body) = c.message.split_once('\n').unwrap_or((&c.message, ""));
    let (subject, body) = (subject.to_owned(), body.trim().to_owned());
    let (mut l, head) = frame(app, pref, r, "log", &subject);
    l.crumbs.push((String::new(), short(&c.oid)));
    html(&Commit {
        l,
        r: head,
        anchors: anchors(r, &c.oid),
        date: super::people::date(u64::try_from(c.author_time).unwrap_or(0) * 1000),
        subject,
        body,
        c,
        files,
        hidden,
    })
}

pub async fn commit(
    State(app): State<Arc<App>>,
    pref: Pref,
    Path((rid, sha)): Path<(String, String)>,
    Query(q): Query<Paging>,
) -> Response {
    blocking(move || match open(&app, &pref, &rid) {
        Ok(r) => commit_page(&app, &pref, &r, &sha, q.all.is_some()),
        Err(e) => e,
    })
    .await
}
