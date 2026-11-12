//! `chart init [--private]` and `chart member ...` (P2 private repos,
//! docs/p2/private-repos.md).

use charter_core::Nid;
use charter_private::{Role, Sealed};
use charter_store::Arkiv;
use charter_sync::identity::{self, RepoMeta};
use charter_sync::ids::{hex32, parse_id, parse_url, to_text, unhex32};
use charter_sync::{Config, Keys};
use clap::Subcommand;

use crate::Result;
use crate::cmd::InitArgs;

#[derive(Debug, Subcommand)]
pub enum MemberCmd {
    /// Publish a fresh MLS key package and print its id for an admin.
    Keypackage,
    /// Admin only: add the owner of a key package with a role (admin, write or read), or
    /// with --device another device of an existing member (its role is kept).
    Add {
        #[arg(long)]
        repo: String,
        #[arg(long = "key-package")]
        key_package: String,
        #[arg(long, default_value = "write", conflicts_with = "device")]
        role: Role,
        #[arg(long)]
        device: bool,
    },
    /// Admin only: remove a member from all of its devices, which rotates the keys.
    Remove {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        member: String,
    },
    /// Members, roles and device (MLS leaf) counts of a private repo.
    List {
        #[arg(long)]
        repo: String,
    },
    /// Admin only: remove from the group the MLS leaf of every device a valid revoke record
    /// names, in one commit (a paid tx; prints its cost). Syncing never does this on its own.
    Sync {
        #[arg(long)]
        repo: String,
    },
}

fn rid_of(repo: &str) -> Result<charter_core::Rid> {
    Ok(parse_url(repo.strip_prefix("arkiv::").unwrap_or(repo))?.rid)
}

pub async fn init_cmd<A: Arkiv>(a: &A, cfg: &Config, i: &InitArgs) -> Result<()> {
    let keys = Keys::require(&cfg.home)?;
    let meta = RepoMeta {
        name: i.name.clone(),
        description: i.description.clone(),
        default_branch: i.default_branch.clone(),
        lang: i.lang.clone(),
        license: i.license.clone(),
    };
    let rid = match i.private {
        true => charter_private::init(a, cfg, &keys, &meta).await?,
        false => identity::init(a, cfg, &keys, &meta).await?,
    };
    println!("rid: {}", to_text(&rid));
    println!("url: arkiv::{}", to_text(&rid));
    Ok(())
}

pub async fn member_cmd<A: Arkiv>(a: &A, cfg: &Config, cmd: &MemberCmd) -> Result<()> {
    let keys = Keys::require(&cfg.home)?;
    match cmd {
        MemberCmd::Keypackage => {
            let id = charter_private::publish_key_package(a, cfg, &keys).await?;
            println!("key package: {}", hex32(&id));
            println!("nid:         {}", to_text(&keys.nid()));
        }
        MemberCmd::Add {
            repo,
            key_package,
            role,
            device,
        } => {
            let rid = rid_of(repo)?;
            let kp = unhex32(key_package)?;
            if *device {
                let nid = charter_private::add_device(a, cfg, &keys, &rid, &kp).await?;
                println!("added a device of {}", to_text(&nid));
            } else {
                let nid = charter_private::add_member(a, cfg, &keys, &rid, &kp, *role).await?;
                println!("added {} as {role:?}", to_text(&nid));
            }
        }
        MemberCmd::Remove { repo, member } => {
            let (rid, nid): (_, Nid) = (rid_of(repo)?, parse_id(member)?);
            charter_private::remove_member(a, cfg, &keys, &rid, &nid).await?;
            println!(
                "removed {} from every device; the keys rotated",
                to_text(&nid)
            );
        }
        MemberCmd::List { repo } => {
            let (epoch, list) = charter_private::members(a, cfg, &keys, &rid_of(repo)?).await?;
            println!("epoch {epoch}");
            for (nid, role, leaves, unbound) in &list {
                let s = if *leaves == 1 { "" } else { "s" };
                let legacy = match unbound {
                    0 => String::new(),
                    n => format!(" ({n} made before 0.6.0, not revocable by device)"),
                };
                println!("{} {role:?} {leaves} device{s}{legacy}", to_text(nid));
            }
            if list.iter().any(|m| m.3 > 0) {
                println!(
                    "to make a member's devices revocable one by one: `chart member remove`, \
                     then `chart member keypackage` on each device and `chart member add` \
                     (the first) and `chart member add --device` (the rest)"
                );
            }
        }
        MemberCmd::Sync { repo } => {
            let rid = rid_of(repo)?;
            let n = remove_revoked(a, cfg, &keys, &rid).await?;
            if n == 0 {
                println!("nothing to remove: no revoked device holds an MLS leaf");
            }
        }
    }
    Ok(())
}

/// Admin only: removes the MLS leaf of every revoked device ([`charter_private::revoked`])
/// and prints what it removed and what it cost (the drop in the wallet and the repo's
/// control address). Returns how many leaves it removed.
pub async fn remove_revoked<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &charter_core::Rid,
) -> Result<usize> {
    let held = || async {
        let (w, c) = (keys.wallet().address(), keys.control_address(rid));
        Ok::<_, crate::CliError>(a.balance(w).await? + a.balance(c).await?)
    };
    let before = held().await?;
    let n = charter_private::revoked::remove_revoked(a, cfg, keys, rid).await?;
    if n > 0 {
        let spent = before.saturating_sub(held().await?);
        let (epoch, _) = charter_private::members(a, cfg, keys, rid).await?;
        println!(
            "removed {n} revoked device leaf(s) in one commit (epoch {epoch}); cost {}",
            crate::units::fmt_glm(spent)
        );
    }
    Ok(n)
}

/// Serves the remote helper over the plain backend, or over [`Sealed`] for a private repo
/// (pushing then needs the write role).
pub async fn serve<A: Arkiv>(a: &A, cfg: Config, url: charter_sync::ids::RemoteUrl) -> Result<()> {
    let keys = Keys::load(&cfg.home)?;
    let ring = charter_private::ring_for(a, &cfg, keys.as_ref(), &url.rid).await?;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    match ring {
        None => {
            crate::helper::Session::new(a, cfg, url)
                .serve(&mut stdin.lock(), &mut out)
                .await
        }
        Some(ring) => {
            let gate = keys
                .as_ref()
                .map(|k| ring.require(&k.nid(), Role::Write))
                .and_then(|r| r.err());
            let sealed = Sealed::new(a, ring).caching(&cfg.home);
            crate::helper::Session::new(&sealed, cfg, url)
                .deny_push(gate)
                .serve(&mut stdin.lock(), &mut out)
                .await
        }
    }
}
