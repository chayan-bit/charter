//! charter-cli: the `git-remote-arkiv` helper and the `chart` tool (design 4.1, 7.3).

/// Evaluates `$body` with `$b` bound to the backend `$a`, or to a `Sealed` view of it when
/// `$rid` is a private repo this machine's identity can read: the remote helper's rule, so
/// ops, pins and folds of a private repo are sealed and blinded like its pushes.
macro_rules! for_repo {
    ($a:expr, $cfg:expr, $rid:expr, |$b:ident| $body:expr) => {{
        let keys = charter_sync::Keys::load(&$cfg.home)?;
        match charter_private::ring_for($a, $cfg, keys.as_ref(), $rid).await? {
            None => {
                let $b = $a;
                $body
            }
            Some(ring) => {
                let $b = &charter_private::Sealed::new($a, ring);
                $body
            }
        }
    }};
}

pub mod backend;
pub mod cmd;
pub mod escrow;
pub mod helper;
pub mod member;
pub mod op;
pub mod program;
pub mod release;
pub mod timelock;
pub mod units;
pub mod zk;

use charter_core::ErrorCode;
use charter_store::StoreError;
use charter_sync::SyncError;

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Sync(#[from] SyncError),
    #[error(transparent)]
    Timelock(#[from] charter_timelock::TimelockError),
    /// `escrow audit` found violations.
    #[error("{0}")]
    Flagged(String),
}

impl From<charter_sync::release::ReleaseError> for CliError {
    fn from(e: charter_sync::release::ReleaseError) -> Self {
        match e {
            charter_sync::release::ReleaseError::Usage(m) => Self::Usage(m),
            charter_sync::release::ReleaseError::Sync(e) => Self::Sync(e),
        }
    }
}

impl From<StoreError> for CliError {
    fn from(e: StoreError) -> Self {
        Self::Sync(e.into())
    }
}

impl CliError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Usage(_) | Self::Timelock(_) | Self::Flagged(_) => None,
            Self::Sync(e) => Some(e.code()),
        }
    }

    /// `chart` exit status: 1 verification, 2 usage, 3 transport, 4 funds.
    pub fn exit_code(&self) -> i32 {
        use ErrorCode::*;
        if let Self::Timelock(e) = self {
            return match e {
                charter_timelock::TimelockError::Usage(_) => 2,
                charter_timelock::TimelockError::Fetch(_) => 3,
                _ => 1,
            };
        }
        if let Self::Flagged(_) = self {
            return 1;
        }
        match self.code() {
            None => 2,
            Some(InsufficientFunds) => 4,
            Some(Rpc | RateLimited | InconsistentView | Paging | WrongChain) => 3,
            Some(_) => 1,
        }
    }

    /// One line: `<CODE>: <message>` (or `usage: <message>`).
    pub fn line(&self) -> String {
        let msg = self.to_string().replace('\n', " ");
        if let Self::Timelock(e) = self {
            return format!("{}: {msg}", e.code());
        }
        match self.code() {
            None if matches!(self, Self::Flagged(_)) => format!("flagged: {msg}"),
            None => format!("usage: {msg}"),
            Some(c) => format!("{}: {msg}", c.as_str()),
        }
    }
}

pub type Result<T, E = CliError> = std::result::Result<T, E>;

/// Runs `f` on a fresh multi-threaded runtime.
pub fn block_on<F: std::future::Future>(f: F) -> std::result::Result<F::Output, CliError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::Sync(SyncError::io("tokio runtime", e)))?;
    Ok(rt.block_on(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_design_table() {
        let c = |code| CliError::Sync(SyncError::coded(code, "x"));
        assert_eq!(CliError::Usage("u".into()).exit_code(), 2);
        assert_eq!(c(ErrorCode::BadSig).exit_code(), 1);
        assert_eq!(c(ErrorCode::Rpc).exit_code(), 3);
        assert_eq!(c(ErrorCode::InsufficientFunds).exit_code(), 4);
        assert_eq!(c(ErrorCode::NotLive).line(), "E_NOT_LIVE: x");
        assert_eq!(CliError::Usage("a\nb".into()).line(), "usage: a b");
        let flagged = CliError::Flagged("2 flags".into());
        assert_eq!(
            (flagged.exit_code(), flagged.line().as_str()),
            (1, "flagged: 2 flags")
        );
        assert_eq!(
            CliError::from(StoreError::Rpc("down".into())).exit_code(),
            3
        );
    }
}
