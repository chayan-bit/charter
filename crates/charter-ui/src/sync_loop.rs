//! The one background sync loop (docs/p6/ui.md 4.2, 4.5). A round takes the repo lock with
//! `Repo::try_open` (skipping the round if a push holds it), runs the fetch sync and
//! `cob::list_since`, writes `view.json` and `cobs.json`, and drops the lock, then
//! broadcasts what changed ([`Event`]) to the pages' event streams. The repo on screen is
//! due at each new block from an `eth_subscribe newHeads` socket, or every 2 s while no
//! socket is live (the fake backend, or a dropped subscription); the others every 30 s.
//! It runs on its own thread and runtime, so git subprocesses never stall page handlers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use charter_core::Rid;
use charter_store::Arkiv;
use charter_store::heads::{self, WsConfig};
use charter_sync::ids::{hex32, unhex32};
use charter_sync::{Config, Keys, Repo, Want, cob};

use crate::UiError;
use crate::cache::{self, Cobs, RepoView};

pub const FOCUS_EVERY: Duration = Duration::from_secs(2);
pub const OTHERS_EVERY: Duration = Duration::from_secs(30);
/// While heads arrive, the repo on screen still syncs at least this often.
pub const HEADS_GAP: Duration = Duration::from_secs(10);
/// A subscription silent this long is treated as dropped (blocks come every 2 s).
const HEADS_STALL: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(250);
/// Events a slow page may fall behind by before it is told to refresh everything.
const EVENTS_BUFFER: usize = 256;

/// What a round found, for the pages showing `rid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub rid: Rid,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Heads moved (refs or a peer's anchored head).
    Push,
    /// New cob ops (issues, comments, patches, reviews, releases).
    Op,
    /// A round ran (sync time, errors, skipped rounds).
    Sync,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Push => "push",
            Kind::Op => "op",
            Kind::Sync => "sync",
        }
    }
}

/// What the last round of one repo did (kept in memory; `view.json` keeps the sync time).
#[derive(Debug, Clone, Default)]
pub struct Round {
    pub tried: Option<Instant>,
    pub error: Option<String>,
    /// Rounds skipped because a push or fetch held the lock.
    pub busy: u64,
}

/// State shared between the loop and the page handlers.
#[derive(Debug)]
pub struct Shared {
    rounds: Mutex<BTreeMap<Rid, Round>>,
    focus: Mutex<Option<Rid>>,
    wanted: Mutex<BTreeSet<Rid>>,
    events: tokio::sync::broadcast::Sender<Event>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            rounds: Mutex::default(),
            focus: Mutex::default(),
            wanted: Mutex::default(),
            events: tokio::sync::broadcast::channel(EVENTS_BUFFER).0,
        }
    }
}

impl Shared {
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Tells every open event stream; nobody listening is fine.
    pub fn emit(&self, rid: Rid, kind: Kind) {
        let _ = self.events.send(Event { rid, kind });
    }

    pub fn focus(&self, rid: Rid) {
        if let Ok(mut f) = self.focus.lock() {
            *f = Some(rid);
        }
    }

    /// Sync `rid` even before its cache directory exists (`chart ui <rid>`).
    pub fn want(&self, rid: Rid) {
        if let Ok(mut w) = self.wanted.lock() {
            w.insert(rid);
        }
    }

    pub fn round(&self, rid: &Rid) -> Round {
        self.rounds
            .lock()
            .ok()
            .and_then(|r| r.get(rid).cloned())
            .unwrap_or_default()
    }

    fn record(&self, rid: Rid, f: impl FnOnce(&mut Round)) {
        if let Ok(mut r) = self.rounds.lock() {
            f(r.entry(rid).or_default());
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Repos with a cache directory under `home`, by rid.
pub fn cached_repos(home: &Path) -> Vec<Rid> {
    let Ok(rd) = std::fs::read_dir(home.join("repos")) else {
        return vec![];
    };
    let mut v: Vec<Rid> = rd
        .filter_map(|e| unhex32(&e.ok()?.file_name().to_string_lossy()).ok())
        .collect();
    v.sort();
    v
}

/// What one round did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Synced(Changes),
    Busy,
}

/// What a synced round found new.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changes {
    /// Canonical refs or a peer's anchored head differ from the last `view.json`.
    pub pushed: bool,
    pub ops: usize,
}

impl Outcome {
    /// The events this outcome makes, `sync` last.
    pub fn kinds(r: &Result<Outcome, UiError>) -> Vec<Kind> {
        let mut v = vec![];
        if let Ok(Outcome::Synced(c)) = r {
            if c.pushed {
                v.push(Kind::Push);
            }
            if c.ops > 0 {
                v.push(Kind::Op);
            }
        }
        v.push(Kind::Sync);
        v
    }
}

/// One round for `rid`: private repos go through the member keyring, as in the CLI.
pub async fn round<A: Arkiv>(a: &A, cfg: &Config, rid: &Rid) -> Result<Outcome, UiError> {
    let keys = Keys::load(&cfg.home)?;
    match charter_private::ring_for(a, cfg, keys.as_ref(), rid).await? {
        None => locked_round(a, cfg, rid).await,
        Some(ring) => locked_round(&charter_private::Sealed::new(a, ring), cfg, rid).await,
    }
}

async fn locked_round<A: Arkiv>(a: &A, cfg: &Config, rid: &Rid) -> Result<Outcome, UiError> {
    let Some(repo) = Repo::try_open(&cfg.home, rid)? else {
        return Ok(Outcome::Busy);
    };
    let view = charter_sync::sync(a, &repo, &Want::default()).await?;
    let cobs_path = repo.dir.join(cache::COBS);
    let old: Cobs = cache::read(&cobs_path)?.unwrap_or_default();
    let since = cob::list_since(a, rid, old.next_block(), &old.ops()?).await?;
    cache::write(&cobs_path, &old.merged(&since.ops, since.last_block))?;
    let view_path = repo.dir.join(cache::VIEW);
    let was: Option<RepoView> = cache::read(&view_path)?;
    let now = RepoView::of(&view, now_ms());
    let pushed = was.is_none_or(|w| w.canonical != now.canonical || w.peers != now.peers);
    cache::write(&view_path, &now)?;
    Ok(Outcome::Synced(Changes {
        pushed,
        ops: since.ops.len(),
    }))
}

/// What paces the repo on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pace {
    /// No `newHeads` socket: every [`FOCUS_EVERY`].
    Poll,
    /// A live socket: at each new block (`fresh`), and at least every [`HEADS_GAP`].
    Heads { fresh: bool },
}

pub fn due(shared: &Shared, home: &Path, now: Instant, pace: Pace) -> Vec<Rid> {
    let focus = shared.focus.lock().ok().and_then(|f| *f);
    let mut all: BTreeSet<Rid> = cached_repos(home).into_iter().collect();
    all.extend(shared.wanted.lock().map(|w| w.clone()).unwrap_or_default());
    all.into_iter()
        .filter(|rid| {
            let every = match (focus == Some(*rid), pace) {
                (false, _) => OTHERS_EVERY,
                (true, Pace::Poll) => FOCUS_EVERY,
                (true, Pace::Heads { fresh: true }) => Duration::ZERO,
                (true, Pace::Heads { fresh: false }) => HEADS_GAP,
            };
            shared.round(rid).tried.is_none_or(|t| now >= t + every)
        })
        .collect()
}

async fn sync_due<A: Arkiv>(a: &A, cfg: &Config, shared: &Shared, pace: Pace) {
    for rid in due(shared, &cfg.home, Instant::now(), pace) {
        let r = round(a, cfg, &rid).await;
        let kinds = Outcome::kinds(&r);
        shared.record(rid, |s| {
            s.tried = Some(Instant::now());
            match r {
                Ok(Outcome::Synced(_)) => s.error = None,
                Ok(Outcome::Busy) => s.busy += 1,
                Err(e) => {
                    charter_sync::warn(&format!("ui sync {}: {e}", hex32(&rid)));
                    s.error = Some(e.to_string());
                }
            }
        });
        kinds.into_iter().for_each(|k| shared.emit(rid, k));
    }
}

async fn run<A: Arkiv>(a: Arc<A>, cfg: Config, shared: Arc<Shared>, ws: Option<WsConfig>) {
    let mut heads = ws.map(|ws| {
        let (tx, rx) = tokio::sync::watch::channel(None);
        tokio::spawn(follow(ws, tx));
        rx
    });
    let mut seen = None;
    loop {
        let head = heads.as_ref().and_then(|h| *h.borrow());
        let pace = match head {
            None => Pace::Poll,
            Some(_) => Pace::Heads {
                fresh: head != seen,
            },
        };
        seen = head;
        sync_due(a.as_ref(), &cfg, &shared, pace).await;
        match heads.as_mut() {
            Some(h) => {
                if let Ok(Err(_)) = tokio::time::timeout(TICK, h.changed()).await {
                    heads = None; // the follower is gone: poll from now on
                }
            }
            None => tokio::time::sleep(TICK).await,
        }
    }
}

/// Keeps `tx` at the latest announced block, or `None` while no subscription is live;
/// reconnects with backoff up to a minute.
async fn follow(ws: WsConfig, tx: tokio::sync::watch::Sender<Option<u64>>) {
    let mut wait = Duration::from_secs(1);
    loop {
        match heads::subscribe(&ws).await {
            Ok(mut h) => {
                wait = Duration::from_secs(1);
                while let Ok(Ok(Some(n))) = tokio::time::timeout(HEADS_STALL, h.next()).await {
                    tx.send_replace(Some(n));
                }
                charter_sync::warn("ui sync: newHeads subscription dropped; polling meanwhile");
            }
            Err(e) => charter_sync::warn(&format!("ui sync: newHeads: {e}; polling meanwhile")),
        }
        tx.send_replace(None);
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(60));
    }
}

/// Starts the loop on its own thread with a current-thread runtime; `ws` is the
/// `newHeads` socket, if the backend has one.
pub fn spawn<A: Arkiv + 'static>(
    a: Arc<A>,
    cfg: Config,
    shared: Arc<Shared>,
    ws: Option<WsConfig>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("chart-ui-sync".into())
        .spawn(move || rt.block_on(run(a, cfg, shared, ws)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repo_on_screen_syncs_at_each_new_head_or_every_poll() {
        let (s, home) = (Shared::default(), Path::new("/no/charter/home"));
        let (a, b) = ([1; 32], [2; 32]);
        s.want(a);
        s.want(b);
        s.focus(a);
        let t0 = Instant::now();
        s.record(a, |r| r.tried = Some(t0));
        s.record(b, |r| r.tried = Some(t0));
        let at = |secs| t0 + Duration::from_secs(secs);
        let (fresh, quiet) = (Pace::Heads { fresh: true }, Pace::Heads { fresh: false });
        assert_eq!(due(&s, home, at(1), Pace::Poll), Vec::<Rid>::new());
        assert_eq!(due(&s, home, at(2), Pace::Poll), [a]);
        assert_eq!(due(&s, home, t0, fresh), [a], "a new block is due at once");
        assert_eq!(due(&s, home, at(5), quiet), Vec::<Rid>::new());
        assert_eq!(due(&s, home, at(10), quiet), [a]);
        assert_eq!(due(&s, home, at(30), quiet), [a, b]);
    }
}
