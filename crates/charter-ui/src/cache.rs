//! The UI's per-repo files next to `state.json` (docs/p6/ui.md 4.1): `cobs.json`, the
//! verified ops in Arkiv order with the last block seen, and `view.json`, the verified view
//! a sync round committed. Both are written by temp file and rename, so a page reading
//! them without the repo lock sees the old file or the new one, never a torn one.

use std::collections::BTreeMap;
use std::path::Path;

use charter_core::{CobOpBody, Hash32, Position, VerifiedOp};
use charter_program::{Event, RefUpdate};
use charter_sync::ids::{hex32, unhex32};
use charter_sync::{View, listing};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::UiError;

pub const COBS: &str = "cobs.json";
pub const VIEW: &str = "view.json";
const VERSION: u32 = 1;

/// One verified op, every field kept so it converts back to the same [`VerifiedOp`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpJson {
    pub id: String,
    pub author: String,
    pub rid: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub object: String,
    pub parents: Vec<String>,
    pub action: String,
    /// Canonical CBOR, hex.
    pub payload: String,
    pub ts_ms: u64,
    pub block: u64,
    pub tx: u32,
    pub key: String,
}

impl OpJson {
    pub fn of(o: &VerifiedOp) -> Self {
        let b = &o.body;
        Self {
            id: hex32(&o.id),
            author: hex32(&o.author),
            rid: hex32(&b.rid),
            type_name: b.type_name.clone(),
            object: hex32(&b.object),
            parents: b.parents.iter().map(hex32).collect(),
            action: b.action.clone(),
            payload: hex::encode(&b.payload),
            ts_ms: b.ts_ms,
            block: o.arkiv_order.block,
            tx: o.arkiv_order.tx,
            key: hex32(&o.arkiv_order.key),
        }
    }

    pub fn op(&self) -> Result<VerifiedOp, UiError> {
        let parents = self
            .parents
            .iter()
            .map(|p| unhex32(p))
            .collect::<Result<Vec<Hash32>, _>>()?;
        let payload = hex::decode(&self.payload).map_err(|e| UiError::Cache(e.to_string()))?;
        Ok(VerifiedOp {
            id: unhex32(&self.id)?,
            author: unhex32(&self.author)?,
            body: CobOpBody {
                rid: unhex32(&self.rid)?,
                type_name: self.type_name.clone(),
                object: unhex32(&self.object)?,
                parents,
                action: self.action.clone(),
                payload,
                ts_ms: self.ts_ms,
            },
            arkiv_order: Position {
                block: self.block,
                tx: self.tx,
                key: unhex32(&self.key)?,
            },
        })
    }
}

/// `cobs.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cobs {
    pub v: u32,
    pub last_block: Option<u64>,
    pub ops: Vec<OpJson>,
}

impl Cobs {
    pub fn ops(&self) -> Result<Vec<VerifiedOp>, UiError> {
        self.ops.iter().map(OpJson::op).collect()
    }

    /// The cache after a round: `new` ops appended, the last block raised.
    pub fn merged(&self, new: &[VerifiedOp], last_block: Option<u64>) -> Self {
        Self {
            v: VERSION,
            last_block: self.last_block.max(last_block),
            ops: self
                .ops
                .iter()
                .cloned()
                .chain(new.iter().map(OpJson::of))
                .collect(),
        }
    }

    /// First block the next round asks for.
    pub fn next_block(&self) -> u64 {
        self.last_block.map_or(0, |b| b + 1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionJson {
    pub rev: u64,
    pub h: String,
    pub block: u64,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadJson {
    pub seq: u64,
    /// Anchor `h`.
    pub anchor: String,
    pub block: u64,
    /// Anchor entity key.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerJson {
    pub nid: String,
    pub delegate: bool,
    pub head: Option<HeadJson>,
    /// `(ref name, oid hex)` of the accepted head.
    pub refs: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EquivocationJson {
    pub kind: String,
    pub nid: String,
    pub seq: u64,
    pub keys: [String; 2],
}

/// One ref move the program accepted or rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveJson {
    pub author: String,
    #[serde(rename = "ref")]
    pub name: String,
    pub old: String,
    pub new: String,
    pub reason: Option<String>,
    pub order: u64,
}

/// One proof attachment as the fold saw it (no receipt bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofJson {
    pub kind: String,
    pub ok: bool,
}

/// The pinned program's fold outcome from the round (charter-sync's own fold).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgramJson {
    pub engine: String,
    pub hash: String,
    pub state: serde_json::Value,
    /// Accepted and rejected ref moves in Arkiv order.
    pub moves: Vec<MoveJson>,
    pub obligations: Vec<serde_json::Value>,
    pub logs: Vec<String>,
    /// Proof checks by op id, in payload order.
    pub proofs: BTreeMap<String, Vec<ProofJson>>,
}

/// `view.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoView {
    pub v: u32,
    pub rid: String,
    pub name: String,
    pub description: String,
    pub default_branch: String,
    pub delegates: Vec<String>,
    pub threshold: u64,
    pub private: bool,
    pub revisions: Vec<RevisionJson>,
    pub chain_id: u64,
    pub head_block: u64,
    /// Unix milliseconds of the round that wrote this file.
    pub synced_ms: u64,
    /// Canonical refs (program heads when a program is pinned).
    pub canonical: Vec<(String, String)>,
    pub peers: Vec<PeerJson>,
    pub equivocations: Vec<EquivocationJson>,
    pub program: Option<ProgramJson>,
    /// Peer id to the alias of its peer document, for peers and op authors seen so far.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// Issues and patches opened, counted when the round writes `cobs.json`, so the repo
    /// list never reads every repo's ops. Zero until the first round after an upgrade.
    #[serde(default)]
    pub issues: usize,
    #[serde(default)]
    pub patches: usize,
    /// A private repo, as this home's admin sees it: leaves of revoked devices still in the
    /// group, which the repo page offers to remove as a priced write. Zero otherwise.
    #[serde(default)]
    pub revoked_leaves: usize,
}

impl Cobs {
    /// How many objects of `type_name` were opened.
    pub fn opened(&self, type_name: &str) -> usize {
        self.ops
            .iter()
            .filter(|o| o.type_name == type_name && o.action == "create")
            .count()
    }
}

impl RepoView {
    pub fn of(view: &View, synced_ms: u64) -> Self {
        let id = &view.identity.head().body;
        let delegates = view.delegates();
        Self {
            v: VERSION,
            rid: hex32(&view.rid),
            name: id.name.clone(),
            description: id.description.clone(),
            default_branch: id.default_branch.clone(),
            delegates: delegates.iter().map(hex32).collect(),
            threshold: id.threshold,
            private: id.visibility != 0,
            revisions: view.identity.revisions.iter().map(revision).collect(),
            chain_id: view.chain_id,
            head_block: view.head_block,
            synced_ms,
            canonical: listing(view, None).refs,
            peers: view
                .peers
                .iter()
                .map(|(nid, pv)| peer(nid, delegates.contains(nid), pv))
                .collect(),
            equivocations: view.equivocations.iter().map(equivocation).collect(),
            program: program(view),
            aliases: view
                .peers
                .keys()
                .filter_map(|n| Some((hex32(n), charter_sync::cob::alias_of(n)?)))
                .collect(),
            issues: 0,
            patches: 0,
            revoked_leaves: 0,
        }
    }
}

fn revision(r: &charter_core::Revision) -> RevisionJson {
    RevisionJson {
        rev: r.body.rev,
        h: hex32(&r.h),
        block: r.position.block,
        key: hex32(&r.position.key),
    }
}

fn peer(nid: &Hash32, delegate: bool, pv: &charter_sync::view::PeerView) -> PeerJson {
    // Format v2: the most recently updated device head stands for the identity.
    let v2 = pv
        .v2
        .iter()
        .flat_map(|v| v.heads())
        .max_by_key(|(_, h)| h.updated_at)
        .map(|(_, h)| HeadJson {
            seq: h.body.seq,
            anchor: hex32(&h.h),
            block: h.updated_at,
            key: hex32(&h.key),
        });
    let v1 = pv.head.as_ref().map(|h| HeadJson {
        seq: h.anchor.body.seq,
        anchor: hex32(&h.anchor.h),
        block: h.anchor.position.block,
        key: hex32(&h.anchor.position.key),
    });
    PeerJson {
        nid: hex32(nid),
        delegate,
        head: v2.or(v1),
        refs: pv
            .refs_body()
            .map(|r| {
                r.refs
                    .iter()
                    .map(|e| (e.name.clone(), hex::encode(&e.oid)))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn program(view: &View) -> Option<ProgramJson> {
    let e = view.program.as_ref()?;
    let pin = view.identity.head().body.program.as_ref()?;
    let o = &e.outcome;
    let mv = |u: &RefUpdate, reason: Option<&String>| MoveJson {
        author: hex32(&u.author),
        name: u.name.clone(),
        old: hex::encode(&u.old),
        new: hex::encode(&u.new),
        reason: reason.cloned(),
        order: u.arkiv_order,
    };
    let mut moves: Vec<MoveJson> = o.accepted.iter().map(|u| mv(u, None)).collect();
    moves.extend(o.rejected.iter().map(|(u, r)| mv(u, Some(r))));
    moves.sort_by_key(|m| m.order);
    let proofs = e.events.iter().filter_map(|ev| match ev {
        Event::Op(op) if !op.proofs.is_empty() => Some((
            hex::encode(op.id),
            op.proofs
                .iter()
                .map(|p| ProofJson {
                    kind: p.kind.clone(),
                    ok: p.ok,
                })
                .collect(),
        )),
        _ => None,
    });
    Some(ProgramJson {
        engine: e.engine.clone(),
        hash: hex32(&pin.hash),
        state: serde_json::from_str(&o.state_json).unwrap_or(serde_json::Value::Null),
        moves,
        obligations: o.obligations.clone(),
        logs: o.logs.clone(),
        proofs: proofs.collect(),
    })
}

fn equivocation(e: &charter_core::Equivocation) -> EquivocationJson {
    EquivocationJson {
        kind: e.kind.name().to_owned(),
        nid: hex32(&e.nid),
        seq: e.seq,
        keys: [hex32(&e.keys[0]), hex32(&e.keys[1])],
    }
}

/// Reads `path`; a missing file is `None`.
pub fn read<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, UiError> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b)
            .map(Some)
            .map_err(|e| UiError::Cache(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(UiError::Cache(format!("{}: {e}", path.display()))),
    }
}

/// Temp file, fsync, rename (the same writer as `state.json`).
pub fn write<T: Serialize>(path: &Path, v: &T) -> Result<(), UiError> {
    Ok(charter_sync::state::write_json(path, v)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(n: u8, block: u64) -> VerifiedOp {
        VerifiedOp {
            id: [n; 32],
            author: [9; 32],
            body: CobOpBody {
                rid: [1; 32],
                type_name: "xyz.arkiv.issue".into(),
                object: [0; 32],
                parents: vec![[3; 32]],
                action: "create".into(),
                payload: vec![0xf6],
                ts_ms: 5,
            },
            arkiv_order: Position {
                block,
                tx: 2,
                key: [n + 1; 32],
            },
        }
    }

    #[test]
    fn ops_round_trip_through_cobs_json_and_merge_in_order() {
        let d = tempfile::tempdir().expect("tmp");
        let p = d.path().join(COBS);
        assert_eq!(read::<Cobs>(&p).expect("missing"), None);
        let c = Cobs::default().merged(&[op(1, 4)], Some(6));
        assert_eq!(c.next_block(), 7);
        let c = c.merged(&[op(2, 7)], Some(7));
        write(&p, &c).expect("write");
        let back: Cobs = read(&p).expect("read").expect("some");
        assert_eq!(back.ops().expect("ops"), vec![op(1, 4), op(2, 7)]);
        assert_eq!(
            (back.last_block, Cobs::default().next_block()),
            (Some(7), 0)
        );
        std::fs::write(&p, b"{").expect("torn");
        assert!(read::<Cobs>(&p).is_err());
    }
}
