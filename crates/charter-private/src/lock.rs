//! One writer of a home's private state at a time. `mls.json` holds every repo's MLS group,
//! so the lock is per home, not per repo: two refreshes of different repos would otherwise
//! each save the whole file and drop the other's epoch.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

use charter_sync::{Result, SyncError};

/// Held while it lives; the OS releases it if the process dies.
pub(crate) struct HomeLock(#[allow(dead_code)] File);

/// How long a wait stays silent before it says why the command is not moving.
const QUIET: Duration = Duration::from_secs(1);

/// How long a command waits for the lock before it gives up with `E_CONCURRENT_PUSH`.
const MAX_WAIT: Duration = Duration::from_secs(600);

/// Waits for `$CHARTER_HOME/private/lock`, saying so on stderr once it has waited [`QUIET`]
/// (so a `git fetch` does not hang silently). Polls instead of blocking, so two tasks of one
/// runtime (the UI's round and a write) never block each other's thread. Not re-entrant:
/// only the public entry points take it, and nothing they call takes it again. Gives up
/// after [`MAX_WAIT`], so a stuck holder never hangs a command forever.
pub(crate) async fn lock(home: &Path) -> Result<HomeLock> {
    lock_within(home, MAX_WAIT).await
}

async fn lock_within(home: &Path, max: Duration) -> Result<HomeLock> {
    let dir = home.join("private");
    let io = |e| SyncError::io(dir.join("lock").display(), e);
    std::fs::create_dir_all(&dir).map_err(io)?;
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))
        .map_err(io)?;
    let (started, mut told) = (std::time::Instant::now(), false);
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(HomeLock(f)),
            Err(TryLockError::WouldBlock) if started.elapsed() >= max => {
                return Err(SyncError::coded(
                    charter_core::ErrorCode::ConcurrentPush,
                    format!(
                        "another charter command on this home held {} for over {max:?}; \
                         try again once it ends",
                        dir.join("lock").display()
                    ),
                ));
            }
            Err(TryLockError::WouldBlock) => {
                if !told && started.elapsed() >= QUIET {
                    told = true;
                    eprintln!("waiting for another charter command on this home...");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(TryLockError::Error(e)) => return Err(io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_holder_waits_until_the_first_drops() {
        let home = tempfile::tempdir().expect("tmp");
        let first = lock(home.path()).await.expect("first");
        let second = tokio::time::timeout(Duration::from_millis(50), lock(home.path())).await;
        assert!(second.is_err(), "taken twice");
        drop(first);
        let again = tokio::time::timeout(Duration::from_secs(5), lock(home.path())).await;
        assert!(again.is_ok_and(|l| l.is_ok()));
    }

    #[tokio::test]
    async fn a_wait_for_a_held_lock_gives_up_with_a_coded_error() {
        let home = tempfile::tempdir().expect("tmp");
        let _first = lock(home.path()).await.expect("first");
        let e = lock_within(home.path(), Duration::from_millis(50))
            .await
            .err()
            .expect("gave up");
        assert_eq!(e.code(), charter_core::ErrorCode::ConcurrentPush, "{e}");
    }
}
