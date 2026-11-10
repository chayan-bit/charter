//! The verification panel on every repo page (docs/p6/ui.md 4.3, ui-design.md 5): what the
//! last sync round verified, each fact with the block or entity it came from.
//!
//! Rabdi's explorer documents only `/address/` and `/tx/` paths, and these facts are
//! blocks and entity keys, so they are shown as ids in mono (full value in `title`).

use super::{Repo, ago, short};

/// Panel states: `verified`, `pending` (seen, not yet synced), `unverified`, `equivocation`.
#[derive(Debug, Clone)]
pub struct Verify {
    pub state: &'static str,
    pub word: &'static str,
    pub summary: String,
    pub open: bool,
    pub revisions: usize,
    /// Head revision: `(rev, h, block, entity key)`.
    pub head_rev: Option<(u64, String, u64, String)>,
    pub delegates: Vec<String>,
    pub threshold: u64,
    pub peers: Vec<PeerRow>,
    pub equivocations: Vec<EquivRow>,
    pub synced: String,
    pub head_block: u64,
    pub error: Option<String>,
    pub busy: u64,
}

#[derive(Debug, Clone)]
pub struct PeerRow {
    pub nid: String,
    pub delegate: bool,
    /// `(seq, anchor h, block, entity key)`.
    pub head: Option<(u64, String, u64, String)>,
}

#[derive(Debug, Clone)]
pub struct EquivRow {
    pub kind: String,
    pub nid: String,
    pub seq: u64,
    pub keys: [String; 2],
}

impl Verify {
    pub fn new(r: &Repo) -> Self {
        let round = &r.round;
        let Some(v) = &r.view else {
            return Self::unsynced(round.error.clone(), round.busy);
        };
        let peers: Vec<PeerRow> = v
            .peers
            .iter()
            .map(|p| PeerRow {
                nid: p.nid.clone(),
                delegate: p.delegate,
                head: p
                    .head
                    .as_ref()
                    .map(|h| (h.seq, h.anchor.clone(), h.block, h.key.clone())),
            })
            .collect();
        let equivocations: Vec<EquivRow> = v
            .equivocations
            .iter()
            .map(|e| EquivRow {
                kind: e.kind.clone(),
                nid: e.nid.clone(),
                seq: e.seq,
                keys: e.keys.clone(),
            })
            .collect();
        let anchored = peers.iter().filter(|p| p.head.is_some()).count();
        let (state, word, summary) = match (equivocations.len(), anchored) {
            (n, _) if n > 0 => (
                "equivocation",
                "Equivocation",
                format!("{n} conflicting heads"),
            ),
            (_, 0) => (
                "unverified",
                "Unverified",
                "no peer has an anchored head".into(),
            ),
            (_, n) => (
                "verified",
                "Verified",
                format!(
                    "identity rev {}, {n} of {} peers anchored, signatures valid",
                    v.revisions.last().map_or(0, |x| x.rev),
                    peers.len()
                ),
            ),
        };
        Self {
            state,
            word,
            summary,
            open: state == "equivocation",
            revisions: v.revisions.len(),
            head_rev: v
                .revisions
                .last()
                .map(|x| (x.rev, x.h.clone(), x.block, x.key.clone())),
            delegates: v.delegates.clone(),
            threshold: v.threshold,
            peers,
            equivocations,
            synced: ago(v.synced_ms),
            head_block: v.head_block,
            error: round.error.clone(),
            busy: round.busy,
        }
    }

    fn unsynced(error: Option<String>, busy: u64) -> Self {
        Self {
            state: "pending",
            word: "Pending",
            summary: "not verified by chart ui yet; the first sync round is running".into(),
            open: error.is_some(),
            revisions: 0,
            head_rev: None,
            delegates: vec![],
            threshold: 0,
            peers: vec![],
            equivocations: vec![],
            synced: "never".into(),
            head_block: 0,
            error,
            busy,
        }
    }

    pub fn short(&self, h: &str) -> String {
        short(h)
    }
}
