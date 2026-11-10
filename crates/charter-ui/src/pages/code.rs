//! Tree, blob and raw pages: `/r/<rid>/{tree,blob,raw}/<rev>/<path>`.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{App, Head, Layout, Repo, Theme, blocking, error_page, frame, html, open, seg, short};
use crate::gitread::{self, CommitRow, Entry};
use crate::highlight::{self, HIGHLIGHT_LINES};

/// Blobs above this are offered as raw only.
const MAX_VIEW_BYTES: usize = 4 << 20;

#[derive(Debug, Deserialize, Default)]
pub struct All {
    pub all: Option<u8>,
}

/// A resolved `<rev>/<path>`.
struct At {
    commit: String,
    rev: String,
    path: String,
}

fn at(app: &App, theme: &Theme, r: &Repo, rev: &str, path: &str) -> Result<At, Response> {
    let bad = |m| error_page(app, theme, StatusCode::NOT_FOUND, m);
    let path = gitread::clean_path(path).ok_or_else(|| bad("Invalid path."))?;
    let commit = gitread::resolve(&r.git, r.view.as_ref(), rev)
        .ok_or_else(|| bad("No such branch, tag or commit in the verified refs."))?;
    Ok(At {
        commit,
        rev: seg(rev),
        path,
    })
}

/// `charter / repo / tree / rev / a / b` crumbs.
fn crumbs(l: &mut Layout, r: &Repo, at: &At) {
    let base = format!("/r/{}/tree/{}", r.hex, at.rev);
    l.crumbs.push((base.clone(), short_rev(&at.rev)));
    let mut acc = String::new();
    for s in at.path.split('/').filter(|s| !s.is_empty()) {
        acc = if acc.is_empty() {
            s.to_owned()
        } else {
            format!("{acc}/{s}")
        };
        l.crumbs.push((format!("{base}/{acc}"), s.to_owned()));
    }
    if let Some(last) = l.crumbs.last_mut() {
        last.0.clear();
    }
}

fn short_rev(rev: &str) -> String {
    if gitread::is_oid(rev) {
        short(rev)
    } else {
        rev.replace("%2F", "/").replace("%25", "%")
    }
}

#[derive(Template)]
#[template(path = "tree.html")]
pub struct Tree {
    pub l: Layout,
    pub r: Head,
    pub rev: String,
    pub path: String,
    pub up: Option<String>,
    pub entries: Vec<Entry>,
    pub last: Option<CommitRow>,
}

impl Tree {
    pub fn join(&self, name: &str) -> String {
        if self.path.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", self.path)
        }
    }
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
}

fn up(r: &Repo, at: &At) -> Option<String> {
    if at.path.is_empty() {
        return None;
    }
    let parent = at.path.rsplit_once('/').map_or("", |(p, _)| p);
    Some(format!("/r/{}/tree/{}/{parent}", r.hex, at.rev))
}

fn tree_page(app: &App, theme: &Theme, r: &Repo, rev: &str, path: &str) -> Response {
    let at = match at(app, theme, r, rev, path) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(entries) = gitread::tree(&r.git, &at.commit, &at.path) else {
        return error_page(app, theme, StatusCode::NOT_FOUND, "No such directory.");
    };
    let last = gitread::last_change(&r.git, &at.commit, &at.path);
    let (mut l, head) = frame(app, theme, r, "code", "Tree");
    crumbs(&mut l, r, &at);
    html(&Tree {
        l,
        r: head,
        up: up(r, &at),
        rev: at.rev,
        path: at.path,
        entries,
        last,
    })
}

pub async fn tree_root(
    State(app): State<Arc<App>>,
    theme: Theme,
    Path((rid, rev)): Path<(String, String)>,
) -> Response {
    blocking(move || match open(&app, &theme, &rid) {
        Ok(r) => tree_page(&app, &theme, &r, &rev, ""),
        Err(e) => e,
    })
    .await
}

pub async fn tree(
    State(app): State<Arc<App>>,
    theme: Theme,
    Path((rid, rev, path)): Path<(String, String, String)>,
) -> Response {
    blocking(move || match open(&app, &theme, &rid) {
        Ok(r) => tree_page(&app, &theme, &r, &rev, &path),
        Err(e) => e,
    })
    .await
}

#[derive(Template)]
#[template(path = "blob.html")]
pub struct Blob {
    pub l: Layout,
    pub r: Head,
    pub rev: String,
    pub path: String,
    pub up: Option<String>,
    pub size: usize,
    pub total: usize,
    /// Per-line HTML (escaped or classed); empty for binary or oversized files.
    pub lines: Vec<String>,
    pub binary: bool,
    /// Lines not shown (the "show all" link).
    pub hidden: usize,
}

fn is_binary(b: &[u8]) -> bool {
    b.iter().take(8_000).any(|c| *c == 0) || std::str::from_utf8(b).is_err()
}

fn blob_page(app: &App, theme: &Theme, r: &Repo, rev: &str, path: &str, all: bool) -> Response {
    let at = match at(app, theme, r, rev, path) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let Some(bytes) = gitread::blob(&r.git, &at.commit, &at.path) else {
        return error_page(app, theme, StatusCode::NOT_FOUND, "No such file.");
    };
    let binary = is_binary(&bytes) || bytes.len() > MAX_VIEW_BYTES;
    let text = if binary {
        ""
    } else {
        std::str::from_utf8(&bytes).unwrap_or("")
    };
    let total = text.lines().count();
    let limit = if all { usize::MAX } else { HIGHLIGHT_LINES };
    let name = at.path.rsplit('/').next().unwrap_or("");
    let lines = highlight::lines(name, text, limit);
    let (mut l, head) = frame(app, theme, r, "code", name);
    crumbs(&mut l, r, &at);
    html(&Blob {
        l,
        r: head,
        up: up(r, &at),
        hidden: total.saturating_sub(lines.len()),
        size: bytes.len(),
        total,
        lines,
        binary,
        rev: at.rev,
        path: at.path,
    })
}

pub async fn blob(
    State(app): State<Arc<App>>,
    theme: Theme,
    Path((rid, rev, path)): Path<(String, String, String)>,
    Query(q): Query<All>,
) -> Response {
    blocking(move || match open(&app, &theme, &rid) {
        Ok(r) => blob_page(&app, &theme, &r, &rev, &path, q.all.is_some()),
        Err(e) => e,
    })
    .await
}

/// Text as `text/plain; charset=utf-8`, anything else as an attachment.
/// Image types served inline by the raw route. SVG is not one (it can carry script), so it
/// goes out as text like any other text file.
const IMAGES: [(&str, &str); 5] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
];

/// Repo content as an allowlisted image, `text/plain`, or a download; always sandboxed so
/// nothing in it can run with this origin.
fn raw_response(name: &str, bytes: Vec<u8>) -> Response {
    let mut r = raw_body(name, bytes);
    let sandbox = header::HeaderValue::from_static("sandbox");
    r.headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, sandbox);
    r
}

fn raw_body(name: &str, bytes: Vec<u8>) -> Response {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    if let Some((_, ct)) = IMAGES.iter().find(|(e, _)| ext.as_deref() == Some(*e)) {
        return ([(header::CONTENT_TYPE, *ct)], bytes).into_response();
    }
    if is_binary(&bytes) {
        let safe: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
            .collect();
        let disp = format!(
            "attachment; filename=\"{}\"",
            if safe.is_empty() { "file" } else { &safe }
        );
        return (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                (header::CONTENT_DISPOSITION, disp),
            ],
            bytes,
        )
            .into_response();
    }
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], bytes).into_response()
}

pub async fn raw(
    State(app): State<Arc<App>>,
    theme: Theme,
    Path((rid, rev, path)): Path<(String, String, String)>,
) -> Response {
    blocking(move || {
        let r = match open(&app, &theme, &rid) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let at = match at(&app, &theme, &r, &rev, &path) {
            Ok(a) => a,
            Err(e) => return e,
        };
        match gitread::blob(&r.git, &at.commit, &at.path) {
            Some(b) => raw_response(at.path.rsplit('/').next().unwrap_or(""), b),
            None => error_page(&app, &theme, StatusCode::NOT_FOUND, "No such file."),
        }
    })
    .await
}
