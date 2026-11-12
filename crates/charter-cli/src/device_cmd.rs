//! `chart device list` and `chart device revoke` (docs/p5/head-model.md section 15).

use charter_core::{Nid, Rid};
use charter_private::revoked::LeafState;
use charter_store::Arkiv;
use charter_sync::ids::{parse_id, to_text};
use charter_sync::revoke::{self, DeviceInfo};
use charter_sync::{Config, Keys, Repo, Want};
use clap::Subcommand;

use crate::cmd::rid_of;
use crate::{CliError, Result};

#[derive(Debug, Subcommand)]
pub enum DeviceCmd {
    /// Every device delegated by this machine's identity in the repo.
    List { repo: String },
    /// Revoke a device of this identity: its later heads are ignored (needs `keys/`). In a
    /// private repo an admin then removes the device's MLS leaf (`chart member sync`, a
    /// paid commit); when this identity is an admin, this command does it at once.
    Revoke {
        repo: String,
        /// The device nid, as `chart device list` prints it.
        device: String,
    },
}

impl DeviceCmd {
    pub fn rid(&self) -> Result<Rid> {
        match self {
            DeviceCmd::List { repo } | DeviceCmd::Revoke { repo, .. } => rid_of(repo),
        }
    }
}

fn line(d: &DeviceInfo, me: &Nid) -> String {
    let hex20 = |a: &[u8; 20]| format!("0x{}", hex::encode(a));
    let seq = d.seq.map_or("no head".into(), |s| format!("head seq {s}"));
    let state = match (d.revoked, d.nid == *me) {
        (Some(Some(s)), _) => format!("revoked at seq {s}"),
        (Some(None), _) => "revoked before any push".into(),
        (None, true) => "this machine".into(),
        (None, false) => "active".into(),
    };
    format!(
        "{}  control {}  bulk {}  {seq}  {state}",
        to_text(&d.nid),
        hex20(&d.control),
        hex20(&d.bulk)
    )
}

pub async fn run<A: Arkiv>(a: &A, cfg: &Config, cmd: &DeviceCmd) -> Result<()> {
    let rid = cmd.rid()?;
    let keys = Keys::require(&cfg.home)?;
    let me = charter_sync::device::device_keys(&cfg.home)?.nid();
    let repo = Repo::open(&cfg.home, &rid)?;
    match cmd {
        DeviceCmd::List { .. } => {
            // Its own identity too, as revoke does: a member that is not a delegate is
            // not synced otherwise, and its devices would not be listed.
            let want = Want {
                must: [keys.nid()].into(),
                ..Want::default()
            };
            let view = Box::pin(charter_sync::sync(a, &repo, &want)).await?;
            let devices = revoke::list(&view, &keys.nid());
            if devices.is_empty() {
                println!("no devices: this identity has not pushed to the repo");
            }
            for d in &devices {
                println!("{}", line(d, &me));
            }
        }
        DeviceCmd::Revoke { device, .. } => {
            let nid = parse_id(device).map_err(|_| {
                CliError::Usage(format!(
                    "{device}: not a device nid (see `chart device list`)"
                ))
            })?;
            let out = Box::pin(revoke::revoke_device(a, cfg, &keys, &repo, &nid, &me)).await?;
            let r = &out.record;
            if out.from_mark {
                println!(
                    "the repo fails the rollback check on this device: cut at the head this machine last verified"
                );
            }
            match r.h == charter_core::ZERO {
                true => println!("revoked {device}: it never pushed, nothing of it stays"),
                false => println!(
                    "revoked {device}: its heads up to seq {} stay valid, later ones are ignored",
                    r.seq
                ),
            }
            println!("{STOLEN}");
        }
    }
    Ok(())
}

/// What a device revoke cannot do, said after every revoke, public or private.
const STOLEN: &str = "if that machine had this identity's `keys/` directory, revoking its \
     device does not stop it: it still holds your identity, so it can sign as you, revoke your \
     other devices and read what you can; recovery is removing this identity (`chart member \
     remove` in a private repo, a new delegate set in a public one) and starting a new \
     identity on a clean machine (docs/p2/private-repos.md, \"Lost or stolen machine\")";

/// After a revoke in a private repo: an admin removes the device's leaf at once (one paid
/// commit, its cost printed); then says what the revoked device can still decrypt.
pub async fn private_note<A: Arkiv>(a: &A, cfg: &Config, cmd: &DeviceCmd) -> Result<()> {
    let DeviceCmd::Revoke { device, .. } = cmd else {
        return Ok(());
    };
    let rid = cmd.rid()?;
    let keys = Keys::require(&cfg.home)?;
    let nid =
        parse_id(device).map_err(|_| CliError::Usage(format!("{device}: not a device nid")))?;
    crate::member::remove_revoked(a, cfg, &keys, &rid).await?;
    let state = charter_private::revoked::leaf_state(a, cfg, &keys, &rid, &nid).await?;
    println!("{}", note(state, &to_text(&keys.nid())));
    Ok(())
}

fn note(state: LeafState, me: &str) -> String {
    match state {
        LeafState::Removed { epoch } => format!(
            "private repo: no MLS leaf names this device any more, so it cannot decrypt \
             anything sealed from epoch {epoch} on; what was sealed before stays readable to it"
        ),
        LeafState::Pending => "private repo: its writes after the cut are refused now, but it \
             can still decrypt new rows until an admin removes its MLS leaf with `chart member \
             sync` (one paid commit; syncing alone never does); what is sealed before that \
             stays readable to it"
            .into(),
        LeafState::Legacy => format!(
            "private repo: this identity's MLS leaves predate device binding (charter 0.6.0), \
             so no leaf names this device and it can still decrypt new rows; to cut it off, an \
             admin runs `chart member remove --member {me}`, then each remaining device runs \
             `chart member keypackage` and the admin adds the first with `chart member add` \
             and the rest with `chart member add --device`"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_leaf_state_says_what_the_device_can_still_read() {
        assert!(note(LeafState::Removed { epoch: 7 }, "me").contains("from epoch 7 on"));
        assert!(note(LeafState::Pending, "me").contains("until an admin removes its MLS leaf"));
        assert!(STOLEN.contains("still holds your identity"));
        let legacy = note(LeafState::Legacy, "zMe");
        assert!(
            legacy.contains("chart member remove --member zMe"),
            "{legacy}"
        );
    }
}
