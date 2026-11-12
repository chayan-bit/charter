//! The Verified seal on every repo page (docs/p6/ui-v2.md 3.6): one word, a plain sentence
//! in its popover, and what the last sync round verified, with the block or entity each
//! fact came from, behind Technical details.
//!
//! Rabdi's explorer documents only `/address/` and `/tx/` paths, and these facts are
//! blocks and entity keys, so they are shown as ids in mono (full value in `title`).

use super::ago;
use crate::cache::RepoView;
use crate::sync_loop::{Round, mins_until};

/// Panel states: `verified`, `pending` (seen, not yet synced), `unverified`, `equivocation`.
#[derive(Debug, Clone)]
pub struct Verify {
    pub state: &'static str,
    pub word: &'static str,
    /// Under the word: `on this computer, 12 s ago`.
    pub sub: String,
    /// The popover's heading and its plain sentence.
    pub heading: &'static str,
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
    /// `(seq, signed body h (v1 anchor or v2 head), block, entity key)`.
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
        let (state, word, heading, summary) = status(v, &peers, equivocations.len());
        let synced = ago(v.synced_ms.max(round.checked_ms));
        Self {
            state,
            word,
            sub: match state {
                "verified" => format!("on this computer, {synced}"),
                "equivocation" => "don't merge or release".into(),
                _ => "no signed history yet".into(),
            },
            heading,
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
            synced,
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
            word: "Checking",
            sub: "first copy".into(),
            heading: "Checking the first copy",
            summary: "This computer is fetching the repo from Arkiv and checking every \
                      signature. Nothing is shown as verified until that finishes."
                .into(),
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
}

/// "paused: RPC quota, resumes in 3 min" while background rounds wait for the quota.
fn pause(round: &Round) -> Option<String> {
    let m = mins_until(round.paused?);
    Some(format!("paused: RPC quota, resumes in {m} min"))
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

/// `(state, word, heading, sentence)`: equivocation wins, then no signed head, then verified.
fn status(
    v: &RepoView,
    peers: &[PeerRow],
    equivocations: usize,
) -> (&'static str, &'static str, &'static str, String) {
    let anchored = peers.iter().filter(|p| p.head.is_some()).count();
    if equivocations > 0 {
        return (
            "equivocation",
            "Conflicting history",
            "Two versions were signed",
            "A key of this repo signed two different versions at the same step. Everything \
             else checked out, but don't merge or release until a maintainer explains which \
             one is right."
                .into(),
        );
    }
    if anchored == 0 {
        return (
            "unverified",
            "Not verified",
            "Nothing signed to check yet",
            "This computer found the repo on Arkiv, but none of its peers has published a \
             signed history yet."
                .into(),
        );
    }
    let n = v.delegates.len();
    let who = match n {
        1 => "its maintainer".to_owned(),
        n => format!("its {n} maintainers"),
    };
    let summary = format!(
        "This computer rebuilt the history from Arkiv and checked every signature. It is \
         signed by {who}, and no one has published a conflicting version."
    );
    (
        "verified",
        "Verified",
        "Checked here, not taken on trust",
        summary,
    )
}
