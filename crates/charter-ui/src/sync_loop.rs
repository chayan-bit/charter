//! The one background sync loop (docs/p6/ui.md 4.2). A round takes the repo lock with
//! `Repo::try_open` (skipping the round if a push holds it), runs the fetch sync and
//! `cob::list_since`, writes `view.json` and `cobs.json`, and drops the lock. The repo on
//! screen is due every 2 s, the others every 30 s. It runs on its own thread and runtime,
//! so git subprocesses in a round never stall page handlers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use charter_core::Rid;
use charter_store::Arkiv;
use charter_sync::ids::{hex32, unhex32};
use charter_sync::{Config, Keys, Repo, Want, cob};

use crate::UiError;
use crate::cache::{self, Cobs, RepoView};

pub const FOCUS_EVERY: Duration = Duration::from_secs(2);
pub const OTHERS_EVERY: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(250);

/// What the last round of one repo did (kept in memory; `view.json` keeps the sync time).
#[derive(Debug, Clone, Default)]
pub struct Round {
    pub tried: Option<Instant>,
    pub error: Option<String>,
    /// Rounds skipped because a push or fetch held the lock.
    pub busy: u64,
}

/// State shared between the loop and the page handlers.
#[derive(Debug, Default)]
pub struct Shared {
    rounds: Mutex<BTreeMap<Rid, Round>>,
    focus: Mutex<Option<Rid>>,
    wanted: Mutex<BTreeSet<Rid>>,
}

impl Shared {
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
    Synced,
    Busy,
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
    cache::write(&repo.dir.join(cache::VIEW), &RepoView::of(&view, now_ms()))?;
    Ok(Outcome::Synced)
}

fn due(shared: &Shared, home: &Path, now: Instant) -> Vec<Rid> {
    let focus = shared.focus.lock().ok().and_then(|f| *f);
    let mut all: BTreeSet<Rid> = cached_repos(home).into_iter().collect();
    all.extend(shared.wanted.lock().map(|w| w.clone()).unwrap_or_default());
    all.into_iter()
        .filter(|rid| {
            let every = if focus == Some(*rid) {
                FOCUS_EVERY
            } else {
                OTHERS_EVERY
            };
            shared.round(rid).tried.is_none_or(|t| now >= t + every)
        })
        .collect()
}

async fn run<A: Arkiv>(a: Arc<A>, cfg: Config, shared: Arc<Shared>) {
    loop {
        for rid in due(&shared, &cfg.home, Instant::now()) {
            let r = round(a.as_ref(), &cfg, &rid).await;
            shared.record(rid, |s| {
                s.tried = Some(Instant::now());
                match r {
                    Ok(Outcome::Synced) => s.error = None,
                    Ok(Outcome::Busy) => s.busy += 1,
                    Err(e) => {
                        charter_sync::warn(&format!("ui sync {}: {e}", hex32(&rid)));
                        s.error = Some(e.to_string());
                    }
                }
            });
        }
        tokio::time::sleep(TICK).await;
    }
}

/// Starts the loop on its own thread with a current-thread runtime.
pub fn spawn<A: Arkiv + 'static>(
    a: Arc<A>,
    cfg: Config,
    shared: Arc<Shared>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::Builder::new()
        .name("chart-ui-sync".into())
        .spawn(move || rt.block_on(run(a, cfg, shared)))
}
