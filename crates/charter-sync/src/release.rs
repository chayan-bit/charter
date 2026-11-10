//! Release records (docs/p3/releases.md): hash the artifacts and post one signed
//! `xyz.arkiv.release` op. The op carries only names, hashes, sizes and locations; the
//! bytes live on any mirror. Shared by `chart release publish` and the UI.

use std::time::Duration;

use charter_core::{CobOpBody, Hash32, Rid, ZERO};
use charter_store::Arkiv;
use serde_json::{Value, json};

use crate::cob::{self, json as cbor};
use crate::program::proofs::sha256_hex;
use crate::{Config, Keys, SyncError};

pub const RELEASE_TYPE: &str = "xyz.arkiv.release";

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    /// Bad input: the caller's to fix (`chart` exits 2).
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Sync(#[from] SyncError),
}

pub type Result<T, E = ReleaseError> = std::result::Result<T, E>;

/// What to publish, as `chart release publish` takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publish {
    pub rid: Rid,
    pub tag: String,
    /// The released commit (hex oid).
    pub commit: String,
    /// `<name>=<path-or-url>[@<target>]`; a URL is downloaded to hash it and becomes a location.
    pub artifacts: Vec<String>,
    /// `<name>=<url>`: another place the artifact can be downloaded from.
    pub locations: Vec<String>,
    pub notes: Option<String>,
}

fn usage(s: impl Into<String>) -> ReleaseError {
    ReleaseError::Usage(s.into())
}

/// A name that is safe as a file name in the download directory.
pub fn plain_name(n: &str) -> bool {
    !n.is_empty() && !n.starts_with('.') && !n.contains(['/', '\\', '\0'])
}

fn is_url(s: &str) -> bool {
    ["http://", "https://", "file://"]
        .iter()
        .any(|p| s.starts_with(p))
}

/// `<name>=<source>[@<target>]` as (name, source, target); a target has no `/` or `:`.
fn parse_spec(s: &str) -> Result<(&str, &str, &str)> {
    let (name, rest) = s
        .split_once('=')
        .filter(|(n, r)| plain_name(n) && !r.is_empty())
        .ok_or_else(|| {
            usage(format!(
                "--artifact {s}: want <name>=<path-or-url>[@<target>]"
            ))
        })?;
    Ok(match rest.rsplit_once('@') {
        Some((src, t)) if !src.is_empty() && !t.is_empty() && !t.contains(['/', ':']) => {
            (name, src, t)
        }
        _ => (name, rest, ""),
    })
}

/// The bytes at a `file://`, `http://` or `https://` location.
pub async fn fetch(url: &str) -> std::result::Result<Vec<u8>, String> {
    // shortcut: file:// paths are not percent-decoded and whole artifacts sit in memory;
    // stream to disk if artifacts grow past a few hundred MB.
    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).map_err(|e| e.to_string());
    }
    if !is_url(url) {
        return Err("unsupported location scheme".into());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().await;
    let resp = resp
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.to_string())?;
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| e.to_string())
}

pub fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

async fn artifacts(p: &Publish) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    for spec in &p.artifacts {
        let (name, src, target) = parse_spec(spec)?;
        if out.iter().any(|a| a["name"] == name) {
            return Err(usage(format!("artifact {name} given twice")));
        }
        let (bytes, mut locations) = match is_url(src) {
            true => (
                fetch(src).await.map_err(|e| usage(format!("{src}: {e}")))?,
                vec![src],
            ),
            false => (
                std::fs::read(src).map_err(|e| usage(format!("{src}: {e}")))?,
                vec![],
            ),
        };
        let more = p.locations.iter().filter_map(|l| l.split_once('='));
        locations.extend(more.filter(|(n, _)| *n == name).map(|(_, u)| u));
        if locations.is_empty() {
            return Err(usage(format!(
                "artifact {name} has no location; pass --location {name}=<url>"
            )));
        }
        out.push(json!({
            "name": name, "target": target, "sha256": sha256_hex(&bytes),
            "size": bytes.len(), "locations": locations,
        }));
    }
    for l in &p.locations {
        let name = l.split_once('=').map(|(n, _)| n);
        if !name.is_some_and(|n| out.iter().any(|a| a["name"] == n)) {
            return Err(usage(format!(
                "--location {l}: want <name>=<url> for a given artifact"
            )));
        }
    }
    Ok(out)
}

/// Checks the input and hashes the artifacts: the release op's payload.
pub async fn payload(p: &Publish) -> Result<Value> {
    if p.tag.is_empty() || !is_oid(&p.commit) {
        return Err(usage(
            "--tag must be non-empty and --commit a lowercase hex oid",
        ));
    }
    let mut payload = json!({ "tag": p.tag, "commit": p.commit, "artifacts": artifacts(p).await? });
    if let Some(n) = &p.notes {
        payload["notes"] = json!(n);
    }
    Ok(payload)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The unsigned release op for a [`payload`].
pub fn body(rid: &Rid, payload: &Value) -> Result<CobOpBody> {
    Ok(CobOpBody {
        rid: *rid,
        object: ZERO,
        type_name: RELEASE_TYPE.into(),
        action: "create".into(),
        parents: vec![],
        payload: cbor::to_cbor(payload)?,
        ts_ms: now_ms(),
    })
}

/// Hashes the artifacts and posts the release op; returns its id.
pub async fn publish<A: Arkiv>(a: &A, cfg: &Config, keys: &Keys, p: &Publish) -> Result<Hash32> {
    let body = body(&p.rid, &payload(p).await?)?;
    Ok(cob::post(a, cfg, keys, &body).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_spec_splits_name_source_and_target() {
        let p = |s| parse_spec(s).expect("spec");
        assert_eq!(
            p("a=d/a.tgz@x86_64-unknown-linux-gnu"),
            ("a", "d/a.tgz", "x86_64-unknown-linux-gnu")
        );
        assert_eq!(p("a=https://h/a@b/c"), ("a", "https://h/a@b/c", ""));
        assert_eq!(p("a=https://u@h/a"), ("a", "https://u@h/a", ""));
        assert!(parse_spec("../a=x").is_err() && parse_spec("a=").is_err());
    }

    #[test]
    fn plain_names_cannot_escape_the_download_dir() {
        assert!(plain_name("charter-v0.1.0-x86_64.tar.gz"));
        for n in ["", ".", "..", "a/b", "a\\b", ".hidden"] {
            assert!(!plain_name(n), "{n}");
        }
    }
}
