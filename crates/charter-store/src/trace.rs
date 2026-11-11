//! `CHARTER_TRACE=1`: one stderr line per RPC call, git spawn, signature and push or fetch
//! phase, each stamped with its wall-clock start (`@unix_ms`), so lines from the git
//! helper and `chart` processes of one command can be merged into one timeline. Off by
//! default; stderr only, because stdout belongs to the git remote-helper protocol.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Whether `CHARTER_TRACE` is set and non-empty (read once per process).
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CHARTER_TRACE").is_some_and(|v| !v.is_empty()))
}

/// Milliseconds since the Unix epoch.
pub fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default()
}

/// A timed region; prints `chart-trace: span <label> @<start> <ms> ms` when dropped.
pub struct Span {
    label: Option<String>,
    start_unix: u128,
    started: Instant,
}

/// Starts a [`Span`]; free when tracing is off (the label closure never runs).
pub fn span(label: impl FnOnce() -> String) -> Span {
    Span {
        label: enabled().then(label),
        start_unix: unix_ms(),
        started: Instant::now(),
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if let Some(l) = &self.label {
            eprintln!(
                "chart-trace: span {l} @{} {} ms",
                self.start_unix,
                self.started.elapsed().as_millis()
            );
        }
    }
}
