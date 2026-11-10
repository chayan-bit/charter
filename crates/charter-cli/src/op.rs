//! `chart op`: post and list collaborative-object ops (docs/p3/cob-ops.md).

use std::collections::HashSet;
use std::path::PathBuf;

use charter_core::{CobOpBody, Hash32, VerifiedOp, ZERO};
use charter_store::Arkiv;
use charter_sync::cob::{self, json};
use charter_sync::ids::{hex32, parse_id, to_text};
use charter_sync::program::proofs::{BLOB_CHUNK, BLOB_TYPE, RISC0, sha256_hex};
use charter_sync::{Config, Keys};
use clap::{Args, Subcommand};
use serde_json::{Value, json};

use crate::cmd::rid_of;
use crate::{CliError, Result};

#[derive(Debug, Subcommand)]
pub enum OpCmd {
    /// Sign and write one op; prints its id.
    Post(PostArgs),
    /// Print the verified ops of a repo in Arkiv creation order.
    List(ListArgs),
    /// Post a receipt as blob ops (skipping chunks already on the repo), then one op whose
    /// payload carries the `proofs` entry that references them; prints that op's id.
    AttachProof(AttachArgs),
}

#[derive(Debug, Args)]
pub struct AttachArgs {
    /// The op to post, e.g. `--type xyz.arkiv.patch --action create`; `proofs` is appended
    /// to its `--payload-json` object.
    #[command(flatten)]
    pub post: PostArgs,
    /// RISC Zero receipt file (`chart zk prove` writes one).
    #[arg(long)]
    pub receipt: PathBuf,
    /// The receipt's claimed public statement, as JSON.
    #[arg(long)]
    pub public: PathBuf,
}

#[derive(Debug, Args)]
pub struct PostArgs {
    #[arg(long)]
    pub repo: String,
    /// Object type, e.g. `xyz.arkiv.issue`.
    #[arg(long = "type")]
    pub type_name: String,
    /// `create` starts a new object; anything else needs `--object`.
    #[arg(long)]
    pub action: String,
    #[arg(long)]
    pub object: Option<String>,
    /// Ids of earlier ops of the same object this op saw.
    #[arg(long = "parent")]
    pub parents: Vec<String>,
    /// Payload as JSON (no floats); stored as canonical CBOR.
    #[arg(long)]
    pub payload_json: String,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    #[arg(long)]
    pub repo: String,
    #[arg(long)]
    pub object: Option<String>,
    #[arg(long = "type")]
    pub type_name: Option<String>,
    /// One JSON array instead of one line per op.
    #[arg(long)]
    pub json: bool,
}

impl OpCmd {
    pub fn rid(&self) -> Result<charter_core::Rid> {
        rid_of(match self {
            Self::Post(p) => &p.repo,
            Self::List(l) => &l.repo,
            Self::AttachProof(x) => &x.post.repo,
        })
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn payload(p: &PostArgs) -> Result<Value> {
    serde_json::from_str(&p.payload_json)
        .map_err(|e| CliError::Usage(format!("--payload-json: {e}")))
}

fn body(p: &PostArgs, v: &Value) -> Result<CobOpBody> {
    Ok(CobOpBody {
        rid: rid_of(&p.repo)?,
        object: p
            .object
            .as_deref()
            .map(parse_id)
            .transpose()?
            .unwrap_or(ZERO),
        type_name: p.type_name.clone(),
        action: p.action.clone(),
        parents: p
            .parents
            .iter()
            .map(|s| parse_id(s))
            .collect::<Result<_, _>>()?,
        payload: json::to_cbor(v)?,
        ts_ms: now_ms(),
    })
}

fn view(o: &VerifiedOp) -> Result<Value> {
    let hexes = |v: &[Hash32]| v.iter().map(hex32).collect::<Vec<_>>();
    Ok(json!({
        "id": hex32(&o.id),
        "object": hex32(&o.object()),
        "author": to_text(&o.author),
        "type": o.body.type_name,
        "action": o.body.action,
        "parents": hexes(&o.body.parents),
        "ts_ms": o.body.ts_ms,
        "block": o.arkiv_order.block,
        "payload": json::to_json(&o.body.payload)?,
    }))
}

pub async fn run<A: Arkiv>(a: &A, cfg: &Config, cmd: &OpCmd) -> Result<()> {
    match cmd {
        OpCmd::Post(p) => {
            let body = body(p, &payload(p)?)?;
            let id = cob::post(a, cfg, &Keys::require(&cfg.home)?, &body).await?;
            println!("{}", hex32(&id));
        }
        OpCmd::List(l) => list(a, l).await?,
        OpCmd::AttachProof(x) => {
            let id = attach_proof(a, cfg, x).await?;
            println!("{}", hex32(&id));
        }
    }
    Ok(())
}

fn read(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))
}

/// `payload` with `entry` appended to its `proofs` array.
fn with_proof(mut payload: Value, entry: Value) -> Result<Value> {
    let obj = payload
        .as_object_mut()
        .ok_or_else(|| CliError::Usage("--payload-json must be a JSON object".into()))?;
    match obj.entry("proofs").or_insert_with(|| json!([])) {
        Value::Array(list) => list.push(entry),
        _ => return Err(CliError::Usage("payload `proofs` must be an array".into())),
    }
    Ok(payload)
}

/// Sha256 hex of every blob already posted to `rid`: blobs are content addressed.
async fn posted_blobs<A: Arkiv>(a: &A, rid: &charter_core::Rid) -> Result<HashSet<String>> {
    let ops = cob::list(a, rid, None, Some(BLOB_TYPE)).await?;
    Ok(ops
        .iter()
        .filter_map(|o| {
            let v = json::to_json(&o.body.payload).ok()?;
            Some(sha256_hex(&hex::decode(v["data"].as_str()?).ok()?))
        })
        .collect())
}

/// Posts the receipt's missing chunks, then the op carrying the proof entry.
pub async fn attach_proof<A: Arkiv>(a: &A, cfg: &Config, x: &AttachArgs) -> Result<Hash32> {
    let receipt = read(&x.receipt)?;
    let public: Value = serde_json::from_slice(&read(&x.public)?)
        .map_err(|e| CliError::Usage(format!("{}: {e}", x.public.display())))?;
    let payload = payload(&x.post)?;
    let (keys, rid) = (Keys::require(&cfg.home)?, rid_of(&x.post.repo)?);
    let posted = posted_blobs(a, &rid).await?;
    let (mut chunks, mut ops, mut new) = (Vec::new(), Vec::new(), Vec::new());
    for c in receipt.chunks(BLOB_CHUNK) {
        let h = sha256_hex(c);
        if !posted.contains(&h) && !new.contains(&h) {
            ops.push(CobOpBody {
                rid,
                object: ZERO,
                type_name: BLOB_TYPE.into(),
                action: "create".into(),
                parents: vec![],
                payload: json::to_cbor(&json!({ "data": hex::encode(c) }))?,
                ts_ms: now_ms(),
            });
            new.push(h.clone());
        }
        chunks.push(h);
    }
    let entry = json!({
        "kind": RISC0, "receipt": sha256_hex(&receipt), "chunks": chunks, "public": public,
    });
    ops.push(body(&x.post, &with_proof(payload, entry)?)?);
    // Blobs, then the op naming them: one nonce pipeline, in order, no wait per receipt.
    let ids = cob::post_many(a, cfg, &keys, &ops).await?;
    for (h, id) in new.iter().zip(&ids) {
        eprintln!("blob {h} in op {}", hex32(id));
    }
    ids.last()
        .copied()
        .ok_or_else(|| CliError::Usage("no op was posted".into()))
}

async fn list<A: Arkiv>(a: &A, l: &ListArgs) -> Result<()> {
    let object = l.object.as_deref().map(parse_id).transpose()?;
    let rid = rid_of(&l.repo)?;
    let ops = cob::list(a, &rid, object.as_ref(), l.type_name.as_deref()).await?;
    let views = ops.iter().map(view).collect::<Result<Vec<_>>>()?;
    if l.json {
        println!("{}", Value::Array(views));
        return Ok(());
    }
    for (o, v) in ops.iter().zip(&views) {
        let b = &o.body;
        let (id, obj) = (hex32(&o.id), hex32(&o.object()));
        let author = to_text(&o.author);
        println!(
            "{id} {author} {} {} {obj} {}",
            b.type_name, b.action, v["payload"]
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_proof_appends_to_the_payload_proofs() {
        let p = with_proof(json!({"head": "h"}), json!(1)).expect("object");
        assert_eq!(p, json!({"head": "h", "proofs": [1]}));
        let p = with_proof(p, json!(2)).expect("object");
        assert_eq!(p["proofs"], json!([1, 2]));
        assert!(with_proof(json!([]), json!(1)).is_err());
        assert!(with_proof(json!({"proofs": 3}), json!(1)).is_err());
    }
}
