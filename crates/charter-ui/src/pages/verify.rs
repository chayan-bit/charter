//! The verification panel on every repo page (docs/p6/ui.md 4.3, ui-design.md 5): what the
//! last sync round verified, each fact with the block or entity it came from.
//!
//! Rabdi's explorer documents only `/address/` and `/tx/` paths, and these facts are
//! blocks and entity keys, so they are shown as ids in mono (full value in `title`).

use std::time::Instant;

use super::{ago, short};
use crate::cache::RepoView;
use crate::sync_loop::Round;

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
    /// Age of the last round, or of the last probe that found nothing new.
    pub synced: String,
    pub pause: Option<String>,
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
    pub fn new(view: Option<&RepoView>, round: &Round) -> Self {
        let Some(v) = view else {
            return Self::unsynced(round);
        };
        let peers = peer_rows(v);
        let equivocations = equiv_rows(v);
        let (state, word, summary) = status(v, &peers, equivocations.len());
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
            synced: ago(v.synced_ms.max(round.checked_ms)),
            pause: pause(round),
            head_block: v.head_block,
            error: round.error.clone(),
            busy: round.busy,
        }
    }

    fn unsynced(round: &Round) -> Self {
        let error = round.error.clone();
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
            pause: pause(round),
            head_block: 0,
            error,
            busy: round.busy,
        }
    }

    pub fn short(&self, h: &str) -> String {
        short(h)
    }
}

/// "paused: RPC quota, resumes in 3 min" while background rounds wait for the quota.
fn pause(round: &Round) -> Option<String> {
    let s = round
        .paused?
        .saturating_duration_since(Instant::now())
        .as_secs();
    Some(format!(
        "paused: RPC quota, resumes in {} min",
        s.div_ceil(60).max(1)
    ))
}

fn peer_rows(v: &RepoView) -> Vec<PeerRow> {
    v.peers
        .iter()
        .map(|p| PeerRow {
            nid: p.nid.clone(),
            delegate: p.delegate,
            head: p
                .head
                .as_ref()
                .map(|h| (h.seq, h.anchor.clone(), h.block, h.key.clone())),
        })
        .collect()
}

fn equiv_rows(v: &RepoView) -> Vec<EquivRow> {
    v.equivocations
        .iter()
        .map(|e| EquivRow {
            kind: e.kind.clone(),
            nid: e.nid.clone(),
            seq: e.seq,
            keys: e.keys.clone(),
        })
        .collect()
}

/// `(state, word, summary)`: equivocation wins, then no anchored peer, then verified.
fn status(
    v: &RepoView,
    peers: &[PeerRow],
    equivocations: usize,
) -> (&'static str, &'static str, String) {
    let anchored = peers.iter().filter(|p| p.head.is_some()).count();
    if equivocations > 0 {
        return (
            "equivocation",
            "Equivocation",
            format!("{equivocations} conflicting heads"),
        );
    }
    if anchored == 0 {
        return (
            "unverified",
            "Unverified",
            "no peer has an anchored head".into(),
        );
    }
    let rev = v.revisions.last().map_or(0, |x| x.rev);
    let summary = format!(
        "identity rev {rev}, {anchored} of {} peers anchored, signatures valid",
        peers.len()
    );
    ("verified", "Verified", summary)
}
