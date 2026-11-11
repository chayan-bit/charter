//! Entity kinds (design section 2.2).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Peer,
    Bind,
    Id,
    IdAcc,
    Refs,
    Anchor,
    Manifest,
    Chunk,
    /// Collaborative-object op (docs/p3/cob-ops.md).
    Cob,
    /// P2 private repos: MLS key packages, commits, welcomes and published epoch keys.
    /// Unsigned like `chunk` (MLS messages carry their own signatures); `ag.t` names the type.
    Mls,
    /// Format v2: an identity's delegation of one device (docs/p5/head-model.md 4.1).
    Device,
    /// Format v2: one device's mutable head (docs/p5/head-model.md 4.2).
    Head,
    /// Format v3: the same head with compact piece runs and pack codecs (head-model 13).
    HeadV3,
    /// Format v4: a v3 head whose refs overflow into a ref table (head-model 14).
    HeadV4,
}

const ALL: [Kind; 14] = [
    Kind::Peer,
    Kind::Bind,
    Kind::Id,
    Kind::IdAcc,
    Kind::Refs,
    Kind::Anchor,
    Kind::Manifest,
    Kind::Chunk,
    Kind::Cob,
    Kind::Mls,
    Kind::Device,
    Kind::Head,
    Kind::HeadV3,
    Kind::HeadV4,
];

impl Kind {
    pub const fn code(self) -> u8 {
        match self {
            Kind::Peer => 1,
            Kind::Bind => 2,
            Kind::Id => 3,
            Kind::IdAcc => 4,
            Kind::Refs => 5,
            Kind::Anchor => 6,
            Kind::Manifest => 7,
            Kind::Chunk => 8,
            Kind::Cob => 9,
            Kind::Mls => 10,
            Kind::Device => 11,
            Kind::Head => 12,
            Kind::HeadV3 => 13,
            Kind::HeadV4 => 14,
        }
    }

    /// The `ag.kind` attribute value, also the tag of the content hash.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Peer => "peer",
            Kind::Bind => "bind",
            Kind::Id => "id",
            Kind::IdAcc => "idacc",
            Kind::Refs => "refs",
            Kind::Anchor => "anchor",
            Kind::Manifest => "manifest",
            Kind::Chunk => "chunk",
            Kind::Cob => "cob",
            Kind::Mls => "mls",
            Kind::Device => "device",
            Kind::Head => "head",
            // The content-hash tag; the entity is still stored as `ag.kind = 'head'`.
            Kind::HeadV3 => "head3",
            Kind::HeadV4 => "head4",
        }
    }

    /// Per-object composite signature context, `arkiv-git/<kind>/v1`
    /// (`docs/p0/pq-signatures.md` section 1 usage rules).
    pub const fn sig_ctx(self) -> &'static [u8] {
        // protocol constant: unchanged by the Charter rename
        match self {
            Kind::Peer => b"arkiv-git/peer/v1",
            Kind::Bind => b"arkiv-git/bind/v1",
            Kind::Id => b"arkiv-git/id/v1",
            Kind::IdAcc => b"arkiv-git/idacc/v1",
            Kind::Refs => b"arkiv-git/refs/v1",
            Kind::Anchor => b"arkiv-git/anchor/v1",
            Kind::Manifest => b"arkiv-git/manifest/v1",
            Kind::Chunk => b"arkiv-git/chunk/v1",
            Kind::Cob => b"arkiv-git/cob/v1",
            Kind::Mls => b"arkiv-git/mls/v1",
            Kind::Device => b"arkiv-git/device/v1",
            Kind::Head => b"arkiv-git/head/v1",
            Kind::HeadV3 => b"arkiv-git/head/v3",
            Kind::HeadV4 => b"arkiv-git/head/v4",
        }
    }

    pub fn from_code(code: u64) -> Option<Kind> {
        ALL.into_iter().find(|k| u64::from(k.code()) == code)
    }

    pub fn from_name(name: &str) -> Option<Kind> {
        ALL.into_iter().find(|k| k.name() == name)
    }

    /// Created by the control address (and kept alive forever) vs the bulk address.
    /// Kinds stored without a signed envelope.
    pub const fn unsigned(self) -> bool {
        matches!(self, Kind::Chunk | Kind::Mls)
    }

    pub const fn plane(self) -> Plane {
        match self {
            Kind::Refs | Kind::Manifest | Kind::Chunk => Plane::Bulk,
            _ => Plane::Control,
        }
    }
}

/// Which per-repo EVM address creates a kind (design section 3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plane {
    Control,
    Bulk,
}
