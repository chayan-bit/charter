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
            for (nid, role, leaves) in list {
                let s = if leaves == 1 { "" } else { "s" };
                println!("{} {role:?} {leaves} device{s}", to_text(&nid));
            }
        }
    }
    Ok(())
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
            let sealed = Sealed::new(a, ring);
            crate::helper::Session::new(&sealed, cfg, url)
                .deny_push(gate)
                .serve(&mut stdin.lock(), &mut out)
                .await
        }
    }
}
