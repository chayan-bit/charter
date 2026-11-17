//! The files a patch changes, with its inline review comments (issue #38):
//! `/r/<rid>/patches/<id>/files`. The diff is the patch head against its merge base with
//! the create op's `base`, else with the default branch; a merged patch shows its head
//! commit's own diff. A comment made at the current head sits under the line it names;
//! one made at an older head, or on a line this diff does not show, is listed as outdated.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use super::{App, Head, Layout, Pref, Repo, blocking, error_page, frame, html, open, short};
use crate::diff::{self, FileDiff, Line};
use crate::fold::{self, Entry, Thread};
use crate::gitread;
use crate::highlight::HIGHLIGHT_LINES;
use crate::inline::Inline;

/// One inline comment as shown.
#[derive(Debug, Clone)]
pub struct Note {
    /// Anchor: `ic-<op id>-<index>`.
    pub anchor: String,
    pub author: String,
    pub block: u64,
    pub c: Inline,
    /// The head lines a suggestion replaces, when this diff shows them.
    pub original: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub d: Line,
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone)]
pub struct FileView {
    pub idx: usize,
    pub f: FileDiff,
    pub rows: Vec<Row>,
    /// Closed, with its lines fetched when it is opened (`?file=<idx>`).
    pub lazy: bool,
}

/// Files with more rows than this load on demand, so a big patch is one small page.
const LAZY_ROWS: usize = 60;

impl FileView {
    /// Inline comments already placed on this file's lines.
    pub fn notes(&self) -> usize {
        self.rows.iter().map(|r| r.notes.len()).sum()
    }
}

#[derive(Template)]
#[template(path = "files.html")]
pub struct Files {
    pub l: Layout,
    pub r: Head,
    pub t: Thread,
    pub head: String,
    pub files: Vec<FileView>,
    pub hidden: usize,
    pub outdated: Vec<Note>,
    /// The head is not in this computer's git cache yet.
    pub missing: bool,
}

/// The side (`n` new, `o` old) and line number a comment on row `d` names.
fn anchor(d: &Line) -> (&'static str, u32) {
    match (d.kind, d.new, d.old) {
        ('-', _, Some(o)) => ("o", o),
        (_, Some(n), _) => ("n", n),
        (_, _, o) => ("o", o.unwrap_or(0)),
    }
}

impl Files {
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
    pub fn esc(&self, s: &str) -> String {
        crate::highlight::escape_text(s)
    }
    pub fn back(&self) -> String {
        format!("/r/{}/patches/{}/files", self.r.hex, self.t.id)
    }
    pub fn anchor(&self, d: &Line) -> (&'static str, u32) {
        anchor(d)
    }
}

/// The lines of one file, the answer to `?file=<idx>`.
#[derive(Template)]
#[template(path = "files_frag.html")]
pub struct FileFrag {
    pub l: Layout,
    pub r: Head,
    pub fv: FileView,
}

impl FileFrag {
    pub fn esc(&self, s: &str) -> String {
        crate::highlight::escape_text(s)
    }
    pub fn anchor(&self, d: &Line) -> (&'static str, u32) {
        anchor(d)
    }
}

#[derive(serde::Deserialize)]
pub struct All {
    all: Option<String>,
    /// A file to render open in the page (the no-script way to read a lazy file).
    open: Option<usize>,
    /// A file whose lines alone are wanted.
    file: Option<usize>,
}

fn field<'a>(e: Option<&'a Entry>, k: &str) -> Option<&'a str> {
    let fields = e.map(|e| e.fields.as_slice()).unwrap_or_default();
    fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

/// The diff text of `t` at `head`.
fn patch_diff(r: &Repo, t: &Thread, head: &str) -> Option<String> {
    let default = gitread::resolve(&r.git, r.view.as_ref(), &r.default_ref());
    let base = field(t.entries.first(), "base")
        .filter(|b| gitread::is_oid(b))
        .map(str::to_owned)
        .or(default)?;
    match gitread::is_ancestor(&r.git, head, &base) {
        true => diff::of_commit(&r.git, head),
        false => diff::of_range(&r.git, &base, head),
    }
}

/// Whether row `d` is where a comment on `side` ending at `end` sits.
fn at(d: &Line, side: &str, end: u32) -> bool {
    match side {
        "new" => d.kind != '-' && d.new == Some(end),
        _ => d.kind != '+' && d.old == Some(end),
    }
}

/// Places every inline comment of `t`'s reviews: under its line, or outdated.
fn place(t: &Thread, head: &str, files: &mut [FileView]) -> Vec<Note> {
    let mut outdated = vec![];
    for e in &t.reviews {
        let at_head = field(Some(e), "head") == Some(head);
        for (i, c) in e.inline.iter().enumerate() {
            let note = Note {
                anchor: format!("ic-{}-{i}", e.id),
                author: e.author.clone(),
                block: e.block,
                c: c.clone(),
                original: vec![],
            };
            let file = files.iter_mut().find(|f| f.f.path == c.path);
            let row = file.filter(|_| at_head).and_then(|f| {
                let original = f
                    .rows
                    .iter()
                    .filter(|r| {
                        r.d.kind != '-' && r.d.new.is_some_and(|n| (c.line..=c.end).contains(&n))
                    })
                    .map(|r| r.d.text.clone())
                    .collect();
                let row = f.rows.iter_mut().find(|r| at(&r.d, c.side, c.end))?;
                Some((row, original))
            });
            match row {
                Some((row, original)) => row.notes.push(Note { original, ..note }),
                None => outdated.push(note),
            }
        }
    }
    outdated
}

fn files_page(app: &App, pref: &Pref, r: &Repo, id: &str, q: &All) -> Response {
    let all = q.all.is_some();
    let ops = r.ops();
    let found = fold::threads(&ops, fold::PATCH, r.view.as_ref())
        .into_iter()
        .find(|t| t.id == id);
    let Some(t) = found else {
        let msg = "No such patch among the verified ops.";
        return error_page(app, pref, StatusCode::NOT_FOUND, msg);
    };
    let head = t.revisions.last().cloned().unwrap_or_default();
    let limit = if all { usize::MAX } else { HIGHLIGHT_LINES };
    let text = gitread::is_oid(&head)
        .then(|| patch_diff(r, &t, &head))
        .flatten();
    let missing = text.is_none();
    let (parsed, hidden) = diff::parse(&text.unwrap_or_default(), limit);
    let mut files: Vec<FileView> = parsed
        .into_iter()
        .enumerate()
        .map(|(idx, mut f)| {
            let rows: Vec<Row> = std::mem::take(&mut f.lines)
                .into_iter()
                .map(|d| Row { d, notes: vec![] })
                .collect();
            let lazy = rows.len() > LAZY_ROWS && q.open != Some(idx);
            FileView { idx, f, rows, lazy }
        })
        .collect();
    let outdated = place(&t, &head, &mut files);
    for f in &mut files {
        f.lazy = f.lazy && f.notes() == 0;
    }
    if let Some(i) = q.file {
        let Some(fv) = files.into_iter().find(|f| f.idx == i) else {
            return error_page(
                app,
                pref,
                StatusCode::NOT_FOUND,
                "No such file in this patch.",
            );
        };
        let (l, h) = frame(app, pref, r, "patches", "");
        return html(&FileFrag {
            l,
            r: h,
            fv: FileView { lazy: false, ..fv },
        });
    }
    let (mut l, h) = frame(app, pref, r, "patches", &format!("Files of {}", t.title));
    l.crumbs
        .push((format!("/r/{}/patches", r.hex), "patches".into()));
    l.crumbs
        .push((format!("/r/{}/patches/{}", r.hex, t.id), short(&t.id)));
    l.crumbs.push((String::new(), "files".into()));
    html(&Files {
        l,
        r: h,
        t,
        head,
        files,
        hidden,
        outdated,
        missing,
    })
}

pub async fn files(
    State(app): State<Arc<App>>,
    pref: Pref,
    Path((rid, id)): Path<(String, String)>,
    Query(q): Query<All>,
) -> Response {
    blocking(move || match open(&app, &pref, &rid) {
        Ok(r) => files_page(&app, &pref, &r, &id, &q),
        Err(e) => e,
    })
    .await
}
