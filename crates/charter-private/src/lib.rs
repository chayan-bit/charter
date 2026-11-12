//! charter-private: P2 private repos (docs/p2/private-repos.md).
//!
//! Members agree on keys through an MLS group on the X-Wing hybrid KEM ([`mls`]). Each
//! epoch's exporter secret seals every entity of the repo ([`seal`], [`entity`]) through
//! the [`Sealed`] Arkiv adapter, so push, clone and core verification run unchanged.
//! Group operations are Arkiv entities ([`group`], [`join`]); removing a member rotates the key.
//! [`auth`] ties MLS state to composite identity signatures (docs/p2/private-repos.md).

pub mod arkiv;
pub mod auth;
pub mod entity;
pub mod group;
pub mod join;
pub mod keyring;
mod lock;
pub mod mls;
pub mod msg;
pub mod revoked;
pub mod seal;
pub mod select;
mod taken;

pub use arkiv::Sealed;
pub use group::{
    Grant, add_device, add_member, export_epoch_key, init, member_ring, members, publish_epoch_key,
    publish_epoch_keys, publish_key_package, remove_member,
};
pub use join::{refresh, ring_for};
pub use keyring::{Keyring, Role};
pub use mls::SUITE;
pub use select::{ForRepo, for_repo, for_repo_cached};
