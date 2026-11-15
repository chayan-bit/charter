use super::{Body, cbor_codec, require};
use crate::attrs::Attribute;
use crate::types::{Hash32, Nid, Rid, ZERO};
use crate::{FormatError, Kind, field};

pub const MAX_DELEGATES: usize = 16;
/// `ag.v` of an archived identity revision (key 14 present): a client that predates the
/// archive flag stops on it with `E_UNSUPPORTED_VERSION` instead of skipping the revision.
pub const ID_ARCHIVED_LAYOUT_VERSION: u64 = 2;

/// `id`: an identity revision; the root has `rev = 0`.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct IdBody {
    /// Absent iff `rev = 0`.
    #[cbor(n(1), with = "minicbor::bytes")]
    pub rid: Option<Rid>,
    #[n(2)]
    pub rev: u64,
    /// `h` of revision `rev - 1`; zero iff `rev = 0`.
    #[cbor(n(3), with = "minicbor::bytes")]
    pub parent: Hash32,
    #[n(4)]
    pub name: String,
    #[n(5)]
    pub description: String,
    #[n(6)]
    pub default_branch: String,
    /// Strictly ascending nids, 1 to 16.
    #[cbor(n(7), with = "crate::cbor::hashes")]
    pub delegates: Vec<Nid>,
    #[n(8)]
    pub threshold: u64,
    /// 0 public; 1 private (P2: every entity of the repo is sealed, see charter-private).
    #[n(9)]
    pub visibility: u64,
    #[n(10)]
    pub lang: Option<String>,
    #[n(11)]
    pub license: Option<String>,
    /// Required iff `rev = 0`.
    #[cbor(n(12), with = "minicbor::bytes")]
    pub nonce: Option<[u8; 16]>,
    /// Repo program pinned by hash (docs/p3/programs.md). Omitted from the CBOR when absent,
    /// so bodies without a pin encode exactly as before.
    #[n(13)]
    pub program: Option<ProgramPin>,
    /// The repo is archived (issue #63; advisory, honest clients stop writing). Only `Some(true)` is valid: an
    /// unarchived revision omits key 14, so it encodes exactly as before.
    #[n(14)]
    pub archived: Option<bool>,
}

/// `{ 1: engine tag, 2: H("program", source) }`.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct ProgramPin {
    #[n(1)]
    pub engine: String,
    #[cbor(n(2), with = "minicbor::bytes")]
    pub hash: Hash32,
}

fn name_ok(n: &str) -> bool {
    (1..=64).contains(&n.len())
        && n.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-')
        })
}

fn branch_ok(b: &str) -> bool {
    (1..=100).contains(&b.len())
        && !b.contains('/')
        && gix_validate::reference::name(format!("refs/heads/{b}").as_bytes().into()).is_ok()
}

fn lang_ok(l: &str) -> bool {
    (1..=32).contains(&l.len()) && !l.bytes().any(|c| c.is_ascii_uppercase())
}

fn license_ok(l: &str) -> bool {
    (1..=64).contains(&l.len())
        && l.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'+'))
}

impl IdBody {
    pub fn is_root(&self) -> bool {
        self.rev == 0
    }

    pub fn is_archived(&self) -> bool {
        self.archived == Some(true)
    }

    fn validate_root_fields(&self) -> Result<(), FormatError> {
        let root = self.is_root();
        require(
            self.rid.is_none() == root,
            field("id", "rid", "absent iff rev = 0"),
        )?;
        require(
            (self.parent == ZERO) == root,
            field("id", "parent", "zero iff rev = 0"),
        )?;
        require(
            self.nonce.is_some() == root,
            field("id", "nonce", "required iff rev = 0"),
        )
    }

    fn validate_delegates(&self) -> Result<(), FormatError> {
        let d = &self.delegates;
        require(
            (1..=MAX_DELEGATES).contains(&d.len()),
            field("id", "delegates", "1 to 16 required"),
        )?;
        require(
            d.windows(2).all(|w| w[0] < w[1]),
            field("id", "delegates", "must be strictly ascending"),
        )?;
        let n = d.len() as u64;
        require(
            (1..=n).contains(&self.threshold),
            field("id", "threshold", "must be 1..len(delegates)"),
        )
    }
}

impl Body for IdBody {
    const KIND: Kind = Kind::Id;
    const AG_V_MAX: u64 = ID_ARCHIVED_LAYOUT_VERSION;
    cbor_codec!();

    fn validate(&self) -> Result<(), FormatError> {
        self.validate_root_fields()?;
        require(
            name_ok(&self.name),
            field("id", "name", "1..64 of [a-z0-9._-]"),
        )?;
        require(
            self.description.len() <= 256,
            field("id", "description", "over 256 B"),
        )?;
        require(
            branch_ok(&self.default_branch),
            field("id", "default_branch", "not a valid ref component"),
        )?;
        self.validate_delegates()?;
        require(
            matches!(self.visibility, 0 | 1),
            field("id", "visibility", "unknown value"),
        )?;
        require(
            self.lang.as_deref().is_none_or(lang_ok),
            field("id", "lang", "1..32 B lowercase"),
        )?;
        require(
            self.license.as_deref().is_none_or(license_ok),
            field("id", "license", "1..64 B SPDX id"),
        )?;
        require(
            self.program
                .as_ref()
                .is_none_or(|p| (1..=32).contains(&p.engine.len())),
            field("id", "program", "engine tag 1..32 B"),
        )?;
        require(
            self.archived != Some(false),
            field("id", "archived", "absent or true"),
        )
    }

    fn ag_v(&self) -> u64 {
        match self.is_archived() {
            true => ID_ARCHIVED_LAYOUT_VERSION,
            false => Self::AG_V,
        }
    }

    fn extra_attributes(&self, signer: &Nid, h: &Hash32) -> Vec<Attribute> {
        let mut v = vec![
            Attribute::b32("ag.rid", self.rid.unwrap_or(*h)),
            Attribute::b32("ag.h", *h),
            Attribute::b32("ag.parent", self.parent),
            Attribute::b32("ag.nid", *signer),
            Attribute::u64("ag.seq", self.rev),
            Attribute::str("ag.name", &self.name),
        ];
        v.extend(self.lang.as_deref().map(|l| Attribute::str("ag.lang", l)));
        v.extend(self.license.as_deref().map(|l| Attribute::str("ag.lic", l)));
        v
    }
}
