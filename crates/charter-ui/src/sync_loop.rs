//! The one background sync loop (docs/p6/ui.md 4.2, 4.5). A round takes the repo lock with
//! `Repo::try_open` (skipping the round if a push holds it), runs the fetch sync and
//! `cob::list_since`, writes `view.json` and `cobs.json`, and drops the lock, then
//! broadcasts what changed ([`Event`]) to the pages' event streams. [`crate::pace`] decides
//! when a round is worth its RPC quota. It runs on its own thread and runtime, so git
//! subprocesses never stall page handlers.

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
use crate::head_watch::Watch;

use crate::last::Last;
use crate::pace::Pacer;

/// A page request keeps its repo on screen (and the UI open) this long without a stream.
pub const OPEN_TTL: Duration = Duration::from_secs(60);
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
    pub error: Option<String>,
    /// Rounds skipped because a push or fetch held the lock.
    pub busy: u64,
    /// When a probe last found the repo unchanged since `view.json` (ms).
    pub checked_ms: u64,
    /// Background rounds are paused for the RPC quota until then.
    pub paused: Option<Instant>,
    /// When a round or probe last succeeded (ms).
    pub ok_ms: u64,
}

/// Which pages are open, for [`crate::pace`].
#[derive(Debug, Default)]
struct Activity {
    /// Open event streams per repo.
    viewers: BTreeMap<Rid, usize>,
    /// Last page request per repo.
    seen: BTreeMap<Rid, Instant>,
    /// Last page request of any kind.
    touched: Option<Instant>,
    pause: Option<Instant>,
}

/// State shared between the loop and the page handlers.
#[derive(Debug)]
pub struct Shared {
    rounds: Mutex<BTreeMap<Rid, Round>>,
    focus: Mutex<Option<Rid>>,
    wanted: Mutex<BTreeSet<Rid>>,
    activity: Mutex<Activity>,
    events: tokio::sync::broadcast::Sender<Event>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            rounds: Mutex::default(),
            focus: Mutex::default(),
            wanted: Mutex::default(),
            activity: Mutex::default(),
            events: tokio::sync::broadcast::channel(EVENTS_BUFFER).0,
        }
    }
}

/// An open event stream of `rid`; dropping it closes it for the loop.
#[derive(Debug)]
pub struct Viewer {
    shared: Arc<Shared>,
    rid: Rid,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.shared.act(|a| {
            if let Some(n) = a.viewers.get_mut(&self.rid) {
                *n = n.saturating_sub(1);
            }
        });
    }
}

fn recent(t: Option<&Instant>, now: Instant) -> bool {
    t.is_some_and(|t| now.saturating_duration_since(*t) < OPEN_TTL)
}

impl Shared {
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Tells every open event stream; nobody listening is fine.
    pub fn emit(&self, rid: Rid, kind: Kind) {
        let _ = self.events.send(Event { rid, kind });
    }

    fn act<T: Default>(&self, f: impl FnOnce(&mut Activity) -> T) -> T {
        self.activity
            .lock()
            .map(|mut a| f(&mut a))
            .unwrap_or_default()
    }

    /// A page of `rid` was requested: it is on screen.
    pub fn focus(&self, rid: Rid) {
        if let Ok(mut f) = self.focus.lock() {
            *f = Some(rid);
        }
        let now = Instant::now();
        self.act(|a| {
            a.seen.insert(rid, now);
            a.touched = Some(now);
        });
    }

    /// A page not about one repo (the index) was requested.
    pub fn touch(&self) {
        self.act(|a| a.touched = Some(Instant::now()));
    }

    pub fn viewer(self: &Arc<Self>, rid: Rid) -> Viewer {
        self.act(|a| *a.viewers.entry(rid).or_default() += 1);
        Viewer {
            shared: self.clone(),
            rid,
        }
    }

    /// `rid` is the last repo requested, and a stream of it is open or it was requested lately.
    pub fn on_screen(&self, rid: &Rid, now: Instant) -> bool {
        let focus = self.focus.lock().ok().and_then(|f| *f);
        focus == Some(*rid)
            && self
                .act(|a| a.viewers.get(rid).is_some_and(|n| *n > 0) || recent(a.seen.get(rid), now))
    }

    /// Any page is open: a stream, or any request lately.
    pub fn any_open(&self, now: Instant) -> bool {
        self.act(|a| a.viewers.values().any(|n| *n > 0) || recent(a.touched.as_ref(), now))
    }

    pub fn pause(&self, until: Option<Instant>) {
        self.act(|a| a.pause = until);
    }

    pub fn paused(&self) -> Option<Instant> {
        self.act(|a| a.pause)
    }

    /// "RPC quota: retry after 3 min" while the quota pause lasts; writes wait for it too.
    pub fn quota_wait(&self) -> Option<String> {
        let t = self.paused().filter(|t| *t > Instant::now())?;
        Some(format!("RPC quota: retry after {} min", mins_until(t)))
    }

    pub fn wanted(&self, rid: &Rid) -> bool {
        self.wanted.lock().is_ok_and(|w| w.contains(rid))
    }

    /// Cached repos and wanted ones, by rid.
    pub fn repos(&self, home: &Path) -> Vec<Rid> {
        let mut all: BTreeSet<Rid> = cached_repos(home).into_iter().collect();
        all.extend(self.wanted.lock().map(|w| w.clone()).unwrap_or_default());
        all.into_iter().collect()
    }

    /// Sync `rid` even before its cache directory exists (`chart ui <rid>`).
    pub fn want(&self, rid: Rid) {
        if let Ok(mut w) = self.wanted.lock() {
            w.insert(rid);
        }
    }

    pub fn round(&self, rid: &Rid) -> Round {
        let r = self.rounds.lock().ok().and_then(|r| r.get(rid).cloned());
        Round {
            paused: self.paused().filter(|t| *t > Instant::now()),
            ..r.unwrap_or_default()
        }
    }

    fn record(&self, rid: Rid, f: impl FnOnce(&mut Round)) {
        if let Ok(mut r) = self.rounds.lock() {
            f(r.entry(rid).or_default());
        }
    }

    /// A probe found nothing new.
    pub fn checked(&self, rid: Rid) {
        self.record(rid, |s| (s.checked_ms, s.ok_ms) = (now_ms(), now_ms()));
    }

    /// For the header: the latest success of any repo, and how many repos failed last.
    pub fn health(&self) -> (Option<u64>, usize) {
        let r = self.rounds.lock().map(|r| r.clone()).unwrap_or_default();
        let ok = r.values().map(|x| x.ok_ms).max().filter(|t| *t > 0);
        (ok, r.values().filter(|x| x.error.is_some()).count())
    }

    pub fn failed(&self, rid: Rid, error: String) {
        self.record(rid, |s| s.error = Some(error));
    }

    pub fn recorded(&self, rid: Rid, r: Result<Outcome, UiError>) {
        self.record(rid, |s| match r {
            Ok(Outcome::Synced(_)) => (s.error, s.ok_ms) = (None, now_ms()),
            Ok(Outcome::Busy) => s.busy += 1,
            Err(e) => s.error = Some(e.to_string()),
        });
    }
}

/// Whole minutes until `t`, at least 1.
pub fn mins_until(t: Instant) -> u64 {
    let s = t.saturating_duration_since(Instant::now()).as_secs();
    s.div_ceil(60).max(1)
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
    Ok(round_watched(a, cfg, rid).await?.0)
}

/// [`round`], plus the heads it accepted for the probe ([`Watch`]; none for a private repo).
pub async fn round_watched<A: Arkiv>(
    a: &A,
    cfg: &Config,
    rid: &Rid,
) -> Result<(Outcome, Option<Watch>), UiError> {
    let keys = Keys::load(&cfg.home)?;
    match charter_private::ring_for(a, cfg, keys.as_ref(), rid).await? {
        None => locked_round(a, cfg, rid, 0).await,
        Some(ring) => {
            // The refresh only counts revoked leaves; removing them is a write the admin
            // confirms (`kind=remove-revoked`), never part of a round.
            let revoked = ring.revoked_leaves;
            let sealed = charter_private::Sealed::new(a, ring).caching(&cfg.home);
            Ok((locked_round(&sealed, cfg, rid, revoked).await?.0, None))
        }
    }
}

async fn locked_round<A: Arkiv>(
    a: &A,
    cfg: &Config,
    rid: &Rid,
    revoked: usize,
) -> Result<(Outcome, Option<Watch>), UiError> {
    let Some(repo) = Repo::try_open(&cfg.home, rid)? else {
        return Ok((Outcome::Busy, None));
    };
    let view = charter_sync::sync(a, &repo, &Want::default()).await?;
    let cobs_path = repo.dir.join(cache::COBS);
    let old: Cobs = cache::read(&cobs_path)?.unwrap_or_default();
    let since = cob::list_since(a, rid, old.next_block(), &old.ops()?).await?;
    let cobs = old.merged(&since.ops, since.last_block);
    cache::write(&cobs_path, &cobs)?;
    let view_path = repo.dir.join(cache::VIEW);
    let was: Option<RepoView> = cache::read(&view_path)?;
    let mut now = RepoView::of(&view, now_ms());
    now.revoked_leaves = revoked;
    (now.issues, now.patches) = (
        cobs.opened(crate::fold::ISSUE),
        cobs.opened(crate::fold::PATCH),
    );
    // Aliases learnt in earlier rounds stay; this round's peer lookups refresh them.
    let authors = since.ops.iter().map(|o| o.author);
    let fresh = authors.filter_map(|n| Some((hex32(&n), cob::alias_of(&n)?)));
    now.aliases = was
        .as_ref()
        .map(|w| w.aliases.clone())
        .unwrap_or_default()
        .into_iter()
        .chain(std::mem::take(&mut now.aliases))
        .chain(fresh)
        .collect();
    let pushed = was.is_none_or(|w| w.canonical != now.canonical || w.peers != now.peers);
    index_tips(&repo.dir, &now)?;
    cache::write(&view_path, &now)?;
    let changes = Changes {
        pushed,
        ops: since.ops.len(),
    };
    Ok((Outcome::Synced(changes), Watch::of(&view)))
}

/// Writes `last.json` for the view's ref tips before the view itself, so a page that reads
/// the new view finds every tip indexed; tips already indexed are kept, not walked again.
fn index_tips(dir: &std::path::Path, view: &RepoView) -> Result<(), UiError> {
    let Some(ts) = crate::gitread::open(dir) else {
        return Ok(());
    };
    let git = crate::gitread::local(&ts);
    let path = dir.join(crate::last::FILE);
    // An unreadable index is rebuilt, never fatal to the round.
    let old: Last = cache::read(&path).ok().flatten().unwrap_or_default();
    let tips = view
        .canonical
        .iter()
        .filter_map(|(_, o)| crate::gitread::resolve(&git, None, o));
    let fresh = Last::refresh(&git, old.clone(), tips);
    match fresh == old {
        true => Ok(()),
        false => cache::write(&path, &fresh),
    }
}

async fn run<A: Arkiv>(a: Arc<A>, cfg: Config, shared: Arc<Shared>, ws: Option<WsConfig>) {
    let mut heads = ws.map(|ws| {
        let (tx, rx) = tokio::sync::watch::channel(None);
        tokio::spawn(follow(ws, tx));
        rx
    });
    let mut pacer = Pacer::default();
    loop {
        pacer.head(heads.as_ref().and_then(|h| *h.borrow()), Instant::now());
        pacer.tick(a.as_ref(), &cfg, &shared, Instant::now()).await;
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
