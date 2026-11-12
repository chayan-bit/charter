//! Pages that only hold a write form: `/r/<rid>/issues/new`, `/r/<rid>/releases/new` and
//! `/r/<rid>/manage` (rename, hand over, archive: one identity revision, issue #63).
//! The inline forms (comment, review, merge, fund, claim) live on the thread pages.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;

use super::{App, Head, Layout, Pref, blocking, frame, html, open};
use crate::gitread;

#[derive(Template)]
#[template(path = "issue_new.html")]
pub struct NewIssue {
    pub l: Layout,
    pub r: Head,
}

#[derive(Template)]
#[template(path = "release_new.html")]
pub struct NewRelease {
    pub l: Layout,
    pub r: Head,
    /// The default branch head, as the commit field's default.
    pub head: String,
}

#[derive(Template)]
#[template(path = "manage.html")]
pub struct Manage {
    pub l: Layout,
    pub r: Head,
    /// Keys are loaded (the archive banner turns `l.writes` off, but not this form).
    pub keys: bool,
}

pub async fn manage(State(app): State<Arc<App>>, t: Pref, Path(rid): Path<String>) -> Response {
    blocking(move || {
        let r = match open(&app, &t, &rid) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (mut l, head) = frame(&app, &t, &r, "code", "Manage");
        l.crumbs.push((String::new(), "manage".into()));
        let keys = app.writer.is_some();
        html(&Manage { l, r: head, keys })
    })
    .await
}

pub async fn new_issue(State(app): State<Arc<App>>, t: Pref, Path(rid): Path<String>) -> Response {
    blocking(move || {
        let r = match open(&app, &t, &rid) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (mut l, head) = frame(&app, &t, &r, "issues", "New issue");
        l.crumbs
            .push((format!("/r/{}/issues", r.hex), "issues".into()));
        l.crumbs.push((String::new(), "new".into()));
        html(&NewIssue { l, r: head })
    })
    .await
}

pub async fn new_release(
    State(app): State<Arc<App>>,
    t: Pref,
    Path(rid): Path<String>,
) -> Response {
    blocking(move || {
        let r = match open(&app, &t, &rid) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let commit =
            gitread::resolve(&r.git, r.view.as_ref(), &r.default_ref()).unwrap_or_default();
        let (mut l, head) = frame(&app, &t, &r, "releases", "New release");
        l.crumbs
            .push((format!("/r/{}/releases", r.hex), "releases".into()));
        l.crumbs.push((String::new(), "new".into()));
        html(&NewRelease {
            l,
            r: head,
            head: commit,
        })
    })
    .await
}
