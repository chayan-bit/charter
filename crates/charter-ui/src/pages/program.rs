//! `/r/<rid>/releases` and `/r/<rid>/program`: from the cached ops and the program fold
//! the last sync round ran.

use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::Response;
use serde_json::Value;

use super::{App, Head, Layout, Pref, Repo, blocking, frame, html, open, short};
use crate::cache::{MoveJson, ProgramJson};
use crate::fold::{self, Release, Verdict};

#[derive(Template)]
#[template(path = "releases.html")]
pub struct Releases {
    pub l: Layout,
    pub r: Head,
    pub pinned: bool,
    pub releases: Vec<Release>,
}

impl Releases {
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
    pub fn valid(&self, v: &Verdict) -> bool {
        *v == Verdict::Valid
    }
    pub fn missing(&self, v: &Verdict) -> Option<String> {
        match v {
            Verdict::Invalid(m) => Some(m.join("; ")),
            _ => None,
        }
    }
}

fn releases_page(app: &App, pref: &Pref, r: &Repo) -> Response {
    let ops = r.ops();
    let (mut l, head) = frame(app, pref, r, "releases", "Releases");
    l.crumbs.push((String::new(), "releases".into()));
    html(&Releases {
        l,
        r: head,
        pinned: r.view.as_ref().is_some_and(|v| v.program.is_some()),
        releases: fold::releases(&ops, r.view.as_ref()),
    })
}

pub async fn releases(State(app): State<Arc<App>>, t: Pref, Path(rid): Path<String>) -> Response {
    blocking(move || match open(&app, &t, &rid) {
        Ok(r) => releases_page(&app, &t, &r),
        Err(e) => e,
    })
    .await
}

/// One `bounty` map entry of a program state: `(issue id, amount)`.
pub type Bounty = (String, String);

#[derive(Template)]
#[template(path = "program.html")]
pub struct ProgramPage {
    pub l: Layout,
    pub r: Head,
    pub p: Option<ProgramJson>,
    pub bounties: Vec<Bounty>,
    pub state: String,
}

impl ProgramPage {
    pub fn short(&self, s: &str) -> String {
        short(s)
    }
    /// A bounty amount (wei, as the program keeps it) in tGLM.
    pub fn glm(&self, wei: &str) -> String {
        super::glm(wei)
    }
    pub fn rejected(&self) -> Vec<&MoveJson> {
        self.p
            .iter()
            .flat_map(|p| &p.moves)
            .filter(|m| m.reason.is_some())
            .collect()
    }
    pub fn accepted(&self) -> Vec<&MoveJson> {
        self.p
            .iter()
            .flat_map(|p| &p.moves)
            .filter(|m| m.reason.is_none())
            .collect()
    }
}

fn entries(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.to_string())).collect())
        .unwrap_or_default()
}

fn program_page(app: &App, pref: &Pref, r: &Repo) -> Response {
    let p = r.view.as_ref().and_then(|v| v.program.clone());
    let state = p.as_ref().map(|p| &p.state);
    let (mut l, head) = frame(app, pref, r, "program", "Rules and CI");
    l.crumbs.push((String::new(), "rules".into()));
    html(&ProgramPage {
        l,
        r: head,
        bounties: state.map(|s| entries(&s["bounty"])).unwrap_or_default(),
        state: state
            .and_then(|s| serde_json::to_string_pretty(s).ok())
            .unwrap_or_default(),
        p,
    })
}

pub async fn program(State(app): State<Arc<App>>, t: Pref, Path(rid): Path<String>) -> Response {
    blocking(move || match open(&app, &t, &rid) {
        Ok(r) => program_page(&app, &t, &r),
        Err(e) => e,
    })
    .await
}
