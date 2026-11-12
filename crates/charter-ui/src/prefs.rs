//! Local preferences (docs/p6/ui-v2.md, Preferences): `$CHARTER_HOME/ui/prefs.json`,
//! never written on chain. A missing file means the defaults; an unreadable or unknown one
//! also means the defaults, with a notice on every page until Settings saves a good file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use charter_store::U256;
use charter_sync::units::glm;
use serde::{Deserialize, Serialize};

use crate::writes::Estimate;

pub const FILE: &str = "ui/prefs.json";
/// The small-write threshold may not exceed this (wei of 0.1 tGLM).
pub const MAX_SKIP: &str = "0.1";
/// A nickname is at most this many characters.
pub const MAX_NICK: usize = 40;

/// How a peer is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Names {
    /// The alias the peer published in its identity.
    Alias,
    /// The nickname set on this computer.
    Nickname,
    #[default]
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fonts {
    #[default]
    Embedded,
    System,
}

/// When a patch reads as merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Merged {
    /// Also when the default branch has moved past its head.
    #[default]
    Infer,
    /// Only after a merge op.
    Ops,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Times {
    /// The block's time.
    #[default]
    Block,
    /// The block's time plus the author's own clock, for ops that carry one.
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

impl Theme {
    /// The `data-theme` attribute; empty follows the system.
    pub fn attr(self) -> &'static str {
        match self {
            Self::System => "",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Prefs {
    pub names: Names,
    /// Peer id (64 hex) to the nickname this computer gives it.
    pub nicknames: BTreeMap<String, String>,
    /// Writes that cost at most this (tGLM, decimal) skip the cost dialog; `None` always
    /// shows it.
    pub skip_under: Option<String>,
    pub fonts: Fonts,
    pub merged: Merged,
    pub times: Times,
    pub theme: Theme,
}

fn is_nid(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// A nickname as stored: trimmed, 1 to [`MAX_NICK`] characters, no control characters.
pub fn nickname(s: &str) -> Result<String, String> {
    let t = s.trim();
    if t.is_empty() || t.chars().count() > MAX_NICK || t.chars().any(char::is_control) {
        return Err(format!(
            "a nickname is 1 to {MAX_NICK} characters, without control characters"
        ));
    }
    Ok(t.to_owned())
}

/// The small-write threshold in wei: more than zero and at most [`MAX_SKIP`].
pub fn threshold(s: &str) -> Result<U256, String> {
    let wei = glm(s.trim())?;
    let max = glm(MAX_SKIP)?;
    if wei == U256::ZERO || wei > max {
        return Err(format!(
            "the threshold must be more than 0 and at most {MAX_SKIP} tGLM"
        ));
    }
    Ok(wei)
}

impl Prefs {
    /// Checks what serde cannot: nickname keys and values, and the threshold.
    pub fn validate(&self) -> Result<(), String> {
        for (nid, nick) in &self.nicknames {
            if !is_nid(nid) {
                return Err(format!("{nid:?} is not a peer id"));
            }
            nickname(nick)?;
        }
        self.skip_under.as_deref().map(threshold).transpose()?;
        Ok(())
    }

    pub fn skip_wei(&self) -> Option<U256> {
        self.skip_under.as_deref().and_then(|s| threshold(s).ok())
    }

    /// How `nid` is shown, given the alias it published.
    pub fn name(&self, nid: &str, alias: Option<&str>) -> String {
        let nick = self.nicknames.get(nid).map(String::as_str);
        let alias = alias.filter(|a| !a.is_empty());
        let shown = match (self.names, nick, alias) {
            (Names::Alias, _, Some(a)) => a.to_owned(),
            (Names::Nickname, Some(n), _) => n.to_owned(),
            (Names::Nickname, None, Some(a)) => a.to_owned(),
            (Names::Both, Some(n), Some(a)) if n != a => format!("{n} ({a})"),
            (Names::Both, Some(n), _) => n.to_owned(),
            (Names::Both, None, Some(a)) => a.to_owned(),
            _ => String::new(),
        };
        if shown.is_empty() {
            crate::pages::short(nid)
        } else {
            shown
        }
    }
}

/// Parses and validates a prefs file.
pub fn parse(bytes: &[u8]) -> Result<Prefs, String> {
    let p: Prefs = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    p.validate()?;
    Ok(p)
}

/// The skip rule: a write skips the dialog only if the user opted in, it moves no funds
/// besides fees, costs at most the threshold, it can be paid, it does not change who can
/// read the repo and the RPC quota is not paused. Everything else shows the dialog.
pub fn skips(p: &Prefs, e: &Estimate, quota_paused: bool) -> bool {
    let Some(limit) = p.skip_wei() else {
        return false;
    };
    !quota_paused && !e.regroups && e.value == U256::ZERO && e.cost <= limit && e.covered()
}

/// The current prefs, shared by every request; `save` swaps them in without a restart.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    cur: RwLock<(Arc<Prefs>, Option<String>)>,
}

impl Store {
    pub fn load(home: &Path) -> Self {
        let path = home.join(FILE);
        let cur = match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Prefs::default(), None),
            Err(e) => (Prefs::default(), Some(e.to_string())),
            Ok(b) => match parse(&b) {
                Ok(p) => (p, None),
                Err(e) => (Prefs::default(), Some(e)),
            },
        };
        let notice = cur.1.map(|e| {
            format!(
                "Your settings file ui/prefs.json could not be used ({e}), so the defaults \
                 apply. Saving in Settings replaces it."
            )
        });
        Self {
            path,
            cur: RwLock::new((Arc::new(cur.0), notice)),
        }
    }

    pub fn get(&self) -> Arc<Prefs> {
        self.cur
            .read()
            .map(|c| c.0.clone())
            .unwrap_or_else(|e| e.into_inner().0.clone())
    }

    /// Why the defaults are in use, until a save.
    pub fn notice(&self) -> Option<String> {
        self.cur.read().ok().and_then(|c| c.1.clone())
    }

    /// Validates, writes the file atomically (mode 0600) and applies it.
    pub fn save(&self, p: Prefs) -> Result<(), String> {
        p.validate()?;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        charter_sync::state::write_json(&self.path, &p).map_err(|e| e.to_string())?;
        let mut c = self
            .cur
            .write()
            .map_err(|_| "settings lock poisoned".to_owned())?;
        *c = (Arc::new(p), None);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn est(cost: u64, balance: u64, value: u64) -> Estimate {
        Estimate {
            cost: U256::from(cost),
            balance: U256::from(balance),
            value: U256::from(value),
            ..Estimate::default()
        }
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        assert_eq!(parse(b"{}").expect("test"), Prefs::default());
        let d = Prefs::default();
        assert_eq!(
            (d.names, d.fonts, d.merged, d.times, d.theme),
            (
                Names::Both,
                Fonts::Embedded,
                Merged::Infer,
                Times::Block,
                Theme::System
            )
        );
        assert!(d.skip_under.is_none());
    }

    #[test]
    fn unknown_fields_values_and_bad_nicknames_are_refused() {
        assert!(parse(b"{\"colour\":\"red\"}").is_err());
        assert!(parse(b"{\"theme\":\"sepia\"}").is_err());
        assert!(parse(b"{\"nicknames\":{\"zz\":\"bob\"}}").is_err());
        let ctl = format!("{{\"nicknames\":{{\"{NID}\":\"a\\u0007b\"}}}}");
        assert!(parse(ctl.as_bytes()).is_err());
        assert!(parse(b"{\"skip_under\":\"0\"}").is_err());
        assert!(parse(b"{\"skip_under\":\"0.5\"}").is_err());
        assert!(parse(b"{\"skip_under\":\"abc\"}").is_err());
        assert!(parse(b"not json").is_err());
    }

    #[test]
    fn a_full_file_round_trips() {
        let p = Prefs {
            names: Names::Alias,
            nicknames: [(NID.to_owned(), "Bob".to_owned())].into(),
            skip_under: Some("0.005".into()),
            fonts: Fonts::System,
            merged: Merged::Ops,
            times: Times::Both,
            theme: Theme::Dark,
        };
        assert_eq!(
            parse(&serde_json::to_vec(&p).expect("test")).expect("test"),
            p
        );
    }

    #[test]
    fn names_follow_the_setting_and_fall_back_to_the_short_id() {
        let mut p = Prefs::default();
        assert_eq!(p.name(NID, None), "1111111");
        assert_eq!(p.name(NID, Some("ravi")), "ravi");
        p.nicknames.insert(NID.into(), "Ravi K".into());
        assert_eq!(p.name(NID, Some("ravi")), "Ravi K (ravi)");
        assert_eq!(p.name(NID, Some("Ravi K")), "Ravi K");
        p.names = Names::Alias;
        assert_eq!(p.name(NID, Some("ravi")), "ravi");
        assert_eq!(p.name(NID, None), "1111111");
        p.names = Names::Nickname;
        assert_eq!(p.name(NID, Some("ravi")), "Ravi K");
    }

    #[test]
    fn small_writes_skip_only_when_opted_in_cheap_funded_and_moving_nothing() {
        let wei = |s| glm(s).expect("test").to::<u64>();
        let mut p = Prefs::default();
        let cheap = est(wei("0.002"), wei("1"), 0);
        assert!(!skips(&p, &cheap, false), "off by default");
        p.skip_under = Some("0.005".into());
        assert!(skips(&p, &cheap, false));
        assert!(
            skips(&p, &est(wei("0.005"), wei("1"), 0), false),
            "at the limit"
        );
        assert!(!skips(&p, &est(wei("0.006"), wei("1"), 0), false), "over");
        assert!(
            !skips(&p, &est(wei("0.002"), wei("0.001"), 0), false),
            "short"
        );
        assert!(
            !skips(&p, &est(wei("0.002"), wei("1"), 5), false),
            "moves funds"
        );
        assert!(!skips(&p, &cheap, true), "quota paused");
        let regroups = Estimate {
            regroups: true,
            ..cheap.clone()
        };
        assert!(!skips(&p, &regroups, false), "a group removal always asks");
        let top_up = Estimate {
            from_wallet: U256::from(wei("0.5")),
            wallet: U256::from(wei("0.1")),
            ..cheap.clone()
        };
        assert!(
            !skips(&p, &top_up, false),
            "the wallet cannot send the top-up"
        );
    }

    #[test]
    fn the_store_falls_back_with_a_notice_and_a_save_applies_and_persists() {
        let home = tempfile::tempdir().expect("test");
        let s = Store::load(home.path());
        assert_eq!(((*s.get()).clone(), s.notice()), (Prefs::default(), None));
        std::fs::create_dir_all(home.path().join("ui")).expect("test");
        std::fs::write(home.path().join(FILE), b"{\"theme\":\"sepia\"}").expect("test");
        let s = Store::load(home.path());
        assert_eq!(*s.get(), Prefs::default());
        assert!(s.notice().expect("test").contains("defaults"));
        let p = Prefs {
            theme: Theme::Dark,
            ..Prefs::default()
        };
        s.save(p.clone()).expect("test");
        assert_eq!(((*s.get()).clone(), s.notice()), (p.clone(), None));
        assert_eq!(*Store::load(home.path()).get(), p);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.path().join(FILE))
                .expect("test")
                .permissions();
            assert_eq!(mode.mode() & 0o777, 0o600);
        }
        let bad = Prefs {
            skip_under: Some("9".into()),
            ..Prefs::default()
        };
        assert!(s.save(bad).is_err());
        assert_eq!(*s.get(), p, "a refused save changes nothing");
    }
}
