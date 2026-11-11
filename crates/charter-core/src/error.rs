//! Stable error codes (design section 7.3) and the core error enums.

use crate::Kind;
use crate::types::{Address, Hash32, Nid};
use charter_crypto::CryptoError;

/// Every stable code in design section 7.3, shared by all Charter crates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    NotLive,
    ControlNotLive,
    IdNotLive,
    StaleHead,
    StaleIdentity,
    HistoryRewritten,
    ChainGap,
    BadSig,
    BadFormat,
    BadRefs,
    BadPack,
    MissingObjects,
    MissingDependency,
    UnsupportedVersion,
    Equivocation,
    ConcurrentPush,
    LostRace,
    UploadIncomplete,
    InsufficientFunds,
    RefsTooLarge,
    Rpc,
    RateLimited,
    InconsistentView,
    Paging,
    WrongChain,
    ProgramMissing,
    ProgramRejected,
    /// P2: the caller holds no key for a private repo.
    NotMember,
    /// P2: the caller's role in a private repo does not allow the operation.
    RoleDenied,
    /// The identity revoked this device (head-model 15).
    DeviceRevoked,
    Internal,
}

impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        use ErrorCode::*;
        match self {
            NotLive => "E_NOT_LIVE",
            ControlNotLive => "E_CONTROL_NOT_LIVE",
            IdNotLive => "E_ID_NOT_LIVE",
            StaleHead => "E_STALE_HEAD",
            StaleIdentity => "E_STALE_IDENTITY",
            HistoryRewritten => "E_HISTORY_REWRITTEN",
            ChainGap => "E_CHAIN_GAP",
            BadSig => "E_BAD_SIG",
            BadFormat => "E_BAD_FORMAT",
            BadRefs => "E_BAD_REFS",
            BadPack => "E_BAD_PACK",
            MissingObjects => "E_MISSING_OBJECTS",
            MissingDependency => "E_MISSING_DEPENDENCY",
            UnsupportedVersion => "E_UNSUPPORTED_VERSION",
            Equivocation => "E_EQUIVOCATION",
            ConcurrentPush => "E_CONCURRENT_PUSH",
            LostRace => "E_LOST_RACE",
            UploadIncomplete => "E_UPLOAD_INCOMPLETE",
            InsufficientFunds => "E_INSUFFICIENT_FUNDS",
            RefsTooLarge => "E_REFS_TOO_LARGE",
            Rpc => "E_RPC",
            RateLimited => "E_RATE_LIMITED",
            InconsistentView => "E_INCONSISTENT_VIEW",
            Paging => "E_PAGING",
            WrongChain => "E_WRONG_CHAIN",
            ProgramMissing => "E_PROGRAM_MISSING",
            ProgramRejected => "E_PROGRAM_REJECTED",
            NotMember => "E_NOT_MEMBER",
            RoleDenied => "E_ROLE_DENIED",
            DeviceRevoked => "E_DEVICE_REVOKED",
            Internal => "E_INTERNAL",
        }
    }

    /// Whether retrying (after fetch or backoff, per section 7.3) can succeed.
    pub const fn retriable(self) -> bool {
        use ErrorCode::*;
        matches!(
            self,
            ConcurrentPush
                | LostRace
                | UploadIncomplete
                | Rpc
                | RateLimited
                | InconsistentView
                | Paging
        )
    }

    fn from_crypto(e: CryptoError) -> Self {
        match e.code() {
            "E_BAD_SIG" => ErrorCode::BadSig,
            "E_BAD_FORMAT" => ErrorCode::BadFormat,
            _ => ErrorCode::Internal,
        }
    }
}

impl core::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Encoding and schema errors.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    #[error("CBOR decode: {0}")]
    Cbor(String),
    #[error("trailing bytes after CBOR item")]
    TrailingBytes,
    #[error("CBOR is not in canonical form (or has unknown/duplicate keys)")]
    NonCanonical,
    #[error("unsupported {what} {value}")]
    Unsupported { what: &'static str, value: u64 },
    #[error("{kind}: field {field}: {reason}")]
    Field {
        kind: &'static str,
        field: &'static str,
        reason: &'static str,
    },
    #[error("expected kind {expected}, found {found}")]
    KindMismatch {
        expected: &'static str,
        found: String,
    },
    #[error("attributes do not match the signed entity")]
    AttributeMismatch,
    #[error("attribute rule: {0}")]
    AttributeRule(&'static str),
    #[error("body of {len} B exceeds the {max} B cap")]
    BodyTooLarge { len: usize, max: usize },
    #[error("refs body of {len} B exceeds the {max} B cap")]
    RefsTooLarge { len: usize, max: usize },
    #[error("crypto: {0}")]
    Crypto(#[from] CryptoError),
}

impl FormatError {
    pub fn code(&self) -> ErrorCode {
        match self {
            FormatError::Unsupported { .. } => ErrorCode::UnsupportedVersion,
            FormatError::RefsTooLarge { .. } => ErrorCode::RefsTooLarge,
            FormatError::Crypto(e) => ErrorCode::from_crypto(*e),
            _ => ErrorCode::BadFormat,
        }
    }
}

pub(crate) const fn field(
    kind: &'static str,
    field: &'static str,
    reason: &'static str,
) -> FormatError {
    FormatError::Field {
        kind,
        field,
        reason,
    }
}

/// Which required entity is missing (for `E_NOT_LIVE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    Refs(Hash32),
    Manifest(Hash32),
}

/// Verification errors (design sections 3 and 7.3).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error("signature: {0}")]
    Crypto(#[from] CryptoError),
    #[error("envelope signer is not the expected peer")]
    SignerMismatch,
    #[error("required entity not live: {0:?}")]
    NotLive(Missing),
    #[error(
        "control address {address:02x?} of peer {nid:02x?} created {created} entities, {live} live"
    )]
    ControlNotLive {
        nid: Nid,
        address: Address,
        created: u64,
        live: u64,
    },
    #[error(
        "device {device:02x?} control address {address:02x?} has nonce {nonce}, its head records {recorded}"
    )]
    HeadRollback {
        device: Nid,
        address: Address,
        nonce: u64,
        recorded: u64,
    },
    #[error("device {device:02x?} has {count} valid heads")]
    DuplicateHead { device: Nid, count: usize },
    #[error("identity root or revision not live")]
    IdNotLive,
    #[error("head seq {seq:?} is below the high-water mark seq {mark}")]
    StaleHead { seq: Option<u64>, mark: u64 },
    #[error("identity head rev {rev} is behind the high-water mark")]
    StaleIdentity { rev: u64 },
    #[error("chain no longer contains the marked entity at seq {seq}")]
    HistoryRewritten { seq: u64 },
    #[error("valid {kind:?} at seq {seq} above the reachable head")]
    ChainGap { kind: Kind, seq: u64 },
    #[error("refs entity invalid: {0}")]
    BadRefs(String),
    #[error("manifest requires anchor {anchor:02x?} of peer {nid:02x?}, not verified")]
    MissingDependency { nid: Nid, anchor: Hash32 },
    #[error(
        "entity {key:02x?} from a tracked control address has an unsupported version; upgrade charter"
    )]
    UnsupportedVersion { key: Hash32 },
    #[error("node view inconsistent (entityNonce changed or count above nonce)")]
    InconsistentView,
    #[error("no completeness observation for control address {0:02x?}")]
    MissingCompleteness(Address),
}

impl VerifyError {
    pub fn code(&self) -> ErrorCode {
        use VerifyError::*;
        match self {
            Format(e) => e.code(),
            Crypto(e) => ErrorCode::from_crypto(*e),
            SignerMismatch => ErrorCode::BadSig,
            NotLive(_) => ErrorCode::NotLive,
            ControlNotLive { .. } | HeadRollback { .. } | DuplicateHead { .. } => {
                ErrorCode::ControlNotLive
            }
            IdNotLive => ErrorCode::IdNotLive,
            StaleHead { .. } => ErrorCode::StaleHead,
            StaleIdentity { .. } => ErrorCode::StaleIdentity,
            HistoryRewritten { .. } => ErrorCode::HistoryRewritten,
            ChainGap { .. } => ErrorCode::ChainGap,
            BadRefs(_) => ErrorCode::BadRefs,
            MissingDependency { .. } => ErrorCode::MissingDependency,
            UnsupportedVersion { .. } => ErrorCode::UnsupportedVersion,
            InconsistentView => ErrorCode::InconsistentView,
            MissingCompleteness(_) => ErrorCode::Internal,
        }
    }
}
