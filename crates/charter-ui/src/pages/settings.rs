//! `/settings` (docs/p6/ui-v2.md, Preferences): local display and confirmation choices,
//! saved to `$CHARTER_HOME/ui/prefs.json` and applied to the next page without a restart.
//! Reads only local files; nothing here touches the chain.

use std::collections::BTreeMap;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::{App, Layout, Pref, blocking, html, short};
use crate::cache::{self, RepoView};
use crate::prefs::{self, Fonts, MAX_NICK, MAX_SKIP, Merged, Names, Prefs, Theme, Times};
use crate::sync_loop::cached_repos;

/// A peer the nickname table offers: id, alias, repos it appears in.
pub struct Peer {
    pub nid: String,
    pub alias: String,
    pub nick: String,
    pub repos: String,
}

/// Every peer and op author the cached repos know, with their aliases.
fn peers(app: &App, p: &Prefs) -> Vec<Peer> {
    let mut seen: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for rid in cached_repos(&app.home) {
        let dir = app.home.join("repos").join(charter_sync::ids::hex32(&rid));
        let Some(v) = cache::read::<RepoView>(&dir.join(cache::VIEW))
            .ok()
            .flatten()
        else {
            continue;
        };
        let nids = v
            .peers
            .iter()
            .map(|x| x.nid.clone())
            .chain(v.aliases.keys().cloned());
        for nid in nids {
            let e = seen.entry(nid.clone()).or_default();
            if let Some(a) = v.aliases.get(&nid) {
                e.0.clone_from(a);
            }
            if !e.1.contains(&v.name) {
                e.1.push(v.name.clone());
            }
        }
    }
    for nid in p.nicknames.keys() {
        seen.entry(nid.clone()).or_default();
    }
    seen.into_iter()
        .map(|(nid, (alias, repos))| Peer {
            nick: p.nicknames.get(&nid).cloned().unwrap_or_default(),
            repos: repos.join(", "),
            alias,
            nid,
        })
        .collect()
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsPage {
    pub l: Layout,
    pub p: Prefs,
    pub peers: Vec<Peer>,
    pub saved: bool,
    pub error: Option<String>,
    pub max_skip: &'static str,
    pub max_nick: usize,
    pub file: String,
}

impl SettingsPage {
    pub fn short(&self, h: &str) -> String {
        short(h)
    }
    pub fn skip(&self) -> &str {
        self.p.skip_under.as_deref().unwrap_or("0.005")
    }
    /// Whether choice `k` is `v`, as the file spells it.
    pub fn is(&self, k: &str, v: &str) -> bool {
        serde_json::to_value(&self.p).is_ok_and(|p| p[k] == v)
    }
}

fn render(app: &App, pref: &Pref, p: Prefs, saved: bool, error: Option<String>) -> Response {
    let mut l = Layout::new(app, pref, "Settings");
    l.nav = "settings";
    let page = SettingsPage {
        peers: peers(app, &p),
        l,
        p,
        saved,
        error: error.clone(),
        max_skip: MAX_SKIP,
        max_nick: MAX_NICK,
        file: prefs::FILE.into(),
    };
    match error {
        Some(_) => (StatusCode::BAD_REQUEST, html(&page)).into_response(),
        None => html(&page),
    }
}

#[derive(Debug, serde::Deserialize, Default)]
pub struct Saved {
    pub saved: Option<u8>,
}

pub async fn page(State(app): State<Arc<App>>, pref: Pref, Query(q): Query<Saved>) -> Response {
    app.shared.touch();
    blocking(move || {
        let p = (*pref.0).clone();
        render(&app, &pref, p, q.saved.is_some(), None)
    })
    .await
}

fn pick<T: Copy>(f: &BTreeMap<String, String>, k: &str, opts: &[(&str, T)]) -> Result<T, String> {
    let v = f.get(k).map_or("", String::as_str);
    opts.iter()
        .find(|(n, _)| *n == v)
        .map(|(_, t)| *t)
        .ok_or_else(|| format!("unknown {k} {v:?}"))
}

/// The settings form, as prefs; not yet validated (the page shows what was typed).
pub fn from_form(f: &BTreeMap<String, String>) -> Result<Prefs, String> {
    let nicknames = f
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("nick.")?.to_owned(), v.trim().to_owned())))
        .filter(|(_, v)| !v.is_empty())
        .collect();
    let skip = pick(f, "small", &[("always", false), ("skip", true)])?;
    Ok(Prefs {
        names: pick(
            f,
            "names",
            &[
                ("alias", Names::Alias),
                ("nickname", Names::Nickname),
                ("both", Names::Both),
            ],
        )?,
        nicknames,
        skip_under: skip.then(|| f.get("skip_under").cloned().unwrap_or_default()),
        fonts: pick(
            f,
            "fonts",
            &[("embedded", Fonts::Embedded), ("system", Fonts::System)],
        )?,
        merged: pick(
            f,
            "merged",
            &[("infer", Merged::Infer), ("ops", Merged::Ops)],
        )?,
        times: pick(
            f,
            "times",
            &[("block", Times::Block), ("both", Times::Both)],
        )?,
        theme: theme(f)?,
    })
}

fn theme(f: &BTreeMap<String, String>) -> Result<Theme, String> {
    pick(
        f,
        "theme",
        &[
            ("system", Theme::System),
            ("light", Theme::Light),
            ("dark", Theme::Dark),
        ],
    )
}

/// `POST /settings` (CSRF-guarded like every POST): validate, save, apply.
pub async fn save(
    State(app): State<Arc<App>>,
    pref: Pref,
    Form(f): Form<BTreeMap<String, String>>,
) -> Response {
    blocking(move || {
        let p = match from_form(&f) {
            Ok(p) => p,
            Err(e) => return render(&app, &pref, (*pref.0).clone(), false, Some(e)),
        };
        if let Err(e) = app.prefs.save(p.clone()) {
            return render(&app, &pref, p, false, Some(e));
        }
        (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, "/settings?saved=1")],
        )
            .into_response()
    })
    .await
}

/// `POST /settings/theme`: the header toggle, so the theme lives in one place.
pub async fn theme_toggle(
    State(app): State<Arc<App>>,
    Form(f): Form<BTreeMap<String, String>>,
) -> Response {
    let t = match theme(&f) {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let p = Prefs {
        theme: t,
        ..(*app.prefs.get()).clone()
    };
    match app.prefs.save(p) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const BASE: [(&str, &str); 6] = [
        ("names", "both"),
        ("small", "always"),
        ("fonts", "embedded"),
        ("merged", "infer"),
        ("times", "block"),
        ("theme", "system"),
    ];

    #[test]
    fn the_default_form_is_the_default_prefs() {
        assert_eq!(from_form(&form(&BASE)).expect("test"), Prefs::default());
    }

    #[test]
    fn nicknames_threshold_and_choices_come_from_the_form() {
        let nid = "a".repeat(64);
        let key = format!("nick.{nid}");
        let mut kv = BASE.to_vec();
        kv.retain(|(k, _)| *k != "small" && *k != "theme");
        kv.extend([
            ("small", "skip"),
            ("skip_under", "0.002"),
            ("theme", "dark"),
            (key.as_str(), " Ravi "),
            ("nick.bbbb", ""),
        ]);
        let p = from_form(&form(&kv)).expect("test");
        assert_eq!(p.skip_under.as_deref(), Some("0.002"));
        assert_eq!(p.theme, Theme::Dark);
        assert_eq!(p.nicknames, [(nid, "Ravi".to_owned())].into());
        assert!(p.validate().is_ok());
        let mut bad = BASE.to_vec();
        bad[0] = ("names", "nick");
        assert!(from_form(&form(&bad)).is_err());
    }
}
