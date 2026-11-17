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
    /// The host of an `http(s)` download URL, the text of its link.
    pub fn host(&self, url: &str) -> Option<String> {
        host_of(url)
    }
    /// A signed size in the units the file lists use.
    pub fn bytes(&self, size: &str) -> String {
        size_text(size)
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
    pub fn glm(&self, wei: &str) -> super::money::Amount {
        super::money::glm(wei)
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

/// The host of an `http(s)` URL; anything else (a signed string can say `javascript:`) is not
/// linked.
fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    let ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-:".contains(c));
    ok.then(|| host.to_owned())
}

/// A signed size in B, KB or MB; a value that is not a number stays as signed.
fn size_text(size: &str) -> String {
    let Ok(n) = size.parse::<u64>() else {
        return format!("{size} B");
    };
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.0),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_urls_link_and_show_their_host() {
        assert_eq!(
            host_of("https://github.com/a/b.tgz").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            host_of("http://localhost:8080/x").as_deref(),
            Some("localhost:8080")
        );
        for bad in [
            "javascript:alert(1)",
            "https:///x",
            "ftp://a/b",
            "https://a b/c",
            "https://evil.com\"x/y",
        ] {
            assert_eq!(host_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn sizes_read_in_the_nearest_unit() {
        assert_eq!(size_text("15"), "15 B");
        assert_eq!(size_text("26140693"), "24.9 MB");
        assert_eq!(size_text("1536"), "1.5 KB");
        assert_eq!(size_text("not a number"), "not a number B");
    }
}
