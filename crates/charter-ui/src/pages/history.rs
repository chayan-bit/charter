//! `/r/<rid>/log/<rev>` (paged commit list) and `/r/<rid>/commit/<sha>`.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;

use super::{App, Head, Layout, Repo, Theme, blocking, error_page, frame, html, open, short};
use crate::diff::{self, FileDiff};
use crate::gitread::{self, CommitInfo, CommitRow};
use crate::highlight::HIGHLIGHT_LINES;

pub const PAGE: usize = 50;

#[derive(Debug, Deserialize, Default)]
pub struct Paging {
    pub page: Option<usize>,
    pub all: Option<u8>,
}

/// `YYYY-MM-DD` (UTC) of unix seconds (Howard Hinnant's civil-from-days).
pub fn day(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
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
}

fn by_day(rows: Vec<CommitRow>) -> Vec<Day> {
    let mut days: Vec<Day> = vec![];
    for c in rows {
        let d = day(c.time);
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

fn log_page(app: &App, theme: &Theme, r: &Repo, rev: &str, page: usize) -> Response {
    let Some(commit) = gitread::resolve(&r.git, r.view.as_ref(), rev) else {
        return error_page(
            app,
            theme,
            StatusCode::NOT_FOUND,
            "No such branch, tag or commit.",
        );
    };
    let page = page.max(1);
    let mut rows =
        gitread::log(&r.git, &commit, (page - 1) * PAGE, PAGE + 1, None).unwrap_or_default();
    let next = rows.len() > PAGE;
    rows.truncate(PAGE);
    let (mut l, head) = frame(app, theme, r, "log", "Commits");
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
    theme: Theme,
    Path((rid, rev)): Path<(String, String)>,
    Query(q): Query<Paging>,
) -> Response {
    blocking(move || match open(&app, &theme, &rid) {
        Ok(r) => log_page(&app, &theme, &r, &rev, q.page.unwrap_or(1)),
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

fn commit_page(app: &App, theme: &Theme, r: &Repo, sha: &str, all: bool) -> Response {
    let found = gitread::is_oid(sha)
        .then(|| gitread::resolve(&r.git, None, sha))
        .flatten()
        .and_then(|oid| gitread::commit(&r.git, &oid).ok());
    let Some(c) = found else {
        return error_page(
            app,
            theme,
            StatusCode::NOT_FOUND,
            "No such commit in the cache.",
        );
    };
    let limit = if all { usize::MAX } else { HIGHLIGHT_LINES };
    let (files, hidden) = diff::parse(&gitread::diff(&r.git, &c.oid).unwrap_or_default(), limit);
    let (subject, body) = c.message.split_once('\n').unwrap_or((&c.message, ""));
    let (subject, body) = (subject.to_owned(), body.trim().to_owned());
    let (mut l, head) = frame(app, theme, r, "log", &subject);
    l.crumbs.push((String::new(), short(&c.oid)));
    html(&Commit {
        l,
        r: head,
        anchors: anchors(r, &c.oid),
        date: day(c.author_time),
        subject,
        body,
        c,
        files,
        hidden,
    })
}

pub async fn commit(
    State(app): State<Arc<App>>,
    theme: Theme,
    Path((rid, sha)): Path<(String, String)>,
    Query(q): Query<Paging>,
) -> Response {
    blocking(move || match open(&app, &theme, &rid) {
        Ok(r) => commit_page(&app, &theme, &r, &sha, q.all.is_some()),
        Err(e) => e,
    })
    .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn days_are_utc_dates() {
        assert_eq!(super::day(0), "1970-01-01");
        assert_eq!(super::day(1_700_000_000), "2023-11-14");
        assert_eq!(super::day(951_782_400), "2000-02-29");
    }
}
