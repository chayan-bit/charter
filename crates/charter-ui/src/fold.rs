//! Issues, patches, reviews and releases folded from the cached verified ops. Who may do
//! what is the pinned program's business (its state is shown on the program page); this
//! fold only groups each object's ops and reads their payloads for display.

use std::collections::BTreeMap;

use charter_core::VerifiedOp;
use charter_sync::cob::json::to_json;
use charter_sync::ids::hex32;
use serde_json::Value;

use crate::cache::RepoView;
use crate::markdown;

pub const ISSUE: &str = "xyz.arkiv.issue";
pub const PATCH: &str = "xyz.arkiv.patch";
pub const REVIEW: &str = "xyz.arkiv.review";
pub const RELEASE: &str = "xyz.arkiv.release";

/// One op of a thread.
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub author: String,
    pub action: String,
    /// Sanitized HTML of the payload's `body` (markdown).
    pub body: Option<String>,
    /// Other payload fields as `(name, text)`; shown escaped, in mono.
    pub fields: Vec<(String, String)>,
    pub ts_ms: u64,
    pub block: u64,
    /// `(kind, verified)` of each proof attachment the fold checked.
    pub proofs: Vec<(String, Option<bool>)>,
}

/// An issue or a patch.
#[derive(Debug, Clone)]
pub struct Thread {
    pub id: String,
    pub title: String,
    pub state: &'static str,
    pub author: String,
    pub ts_ms: u64,
    pub block: u64,
    pub entries: Vec<Entry>,
    /// Patch heads in order: the create op's `head`, then each `revise` / `update`.
    pub revisions: Vec<String>,
    pub reviews: Vec<Entry>,
    /// Shown merged because the default branch contains its head, with no merge op.
    pub inferred: bool,
}

impl Thread {
    pub fn comments(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.action == "comment")
            .count()
    }

    pub fn proofs(&self) -> usize {
        self.entries.iter().map(|e| e.proofs.len()).sum()
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn entry(o: &VerifiedOp, p: &Value, view: Option<&RepoView>) -> Entry {
    let id = hex32(&o.id);
    let checks = view
        .and_then(|v| v.program.as_ref())
        .and_then(|p| p.proofs.get(&id));
    let proofs = p["proofs"]
        .as_array()
        .map(|a| {
            a.iter()
                .enumerate()
                .map(|(i, x)| {
                    (
                        text(&x["kind"]),
                        checks.and_then(|c| c.get(i)).map(|c| c.ok),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let fields = p
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "title" | "body" | "proofs"))
                .map(|(k, v)| (k.clone(), text(v)))
                .collect()
        })
        .unwrap_or_default();
    Entry {
        author: hex32(&o.author),
        action: o.body.action.clone(),
        body: p["body"].as_str().map(markdown::render),
        fields,
        ts_ms: o.body.ts_ms,
        block: o.arkiv_order.block,
        proofs,
        id,
    }
}

fn payload(o: &VerifiedOp) -> Value {
    to_json(&o.body.payload).unwrap_or(Value::Null)
}

fn apply(t: &mut Thread, o: &VerifiedOp, p: &Value) {
    t.state = match (o.body.action.as_str(), t.state) {
        ("close", _) => "closed",
        ("merged" | "merge", _) => "merged",
        ("reopen", _) => "open",
        (_, s) => s,
    };
    if matches!(o.body.action.as_str(), "revise" | "update")
        && let Some(h) = p["head"].as_str()
    {
        t.revisions.push(h.to_owned());
    }
}

/// Threads of `type_name`, newest first, with reviews attached to patches.
pub fn threads(ops: &[VerifiedOp], type_name: &str, view: Option<&RepoView>) -> Vec<Thread> {
    let mut by_obj: BTreeMap<[u8; 32], Thread> = BTreeMap::new();
    let mut order = vec![];
    for o in ops.iter().filter(|o| o.body.type_name == type_name) {
        let p = payload(o);
        if o.body.is_create() {
            order.push(o.id);
            by_obj.insert(o.id, new_thread(o, &p, view));
        } else if let Some(t) = by_obj.get_mut(&o.object()) {
            apply(t, o, &p);
            t.entries.push(entry(o, &p, view));
        }
    }
    if type_name == PATCH {
        attach_reviews(ops, &mut by_obj, view);
    }
    order
        .iter()
        .rev()
        .filter_map(|id| by_obj.remove(id))
        .collect()
}

fn new_thread(o: &VerifiedOp, p: &Value, view: Option<&RepoView>) -> Thread {
    Thread {
        id: hex32(&o.id),
        title: p["title"].as_str().unwrap_or("(untitled)").to_owned(),
        state: "open",
        author: hex32(&o.author),
        ts_ms: o.body.ts_ms,
        block: o.arkiv_order.block,
        entries: vec![entry(o, p, view)],
        revisions: p["head"].as_str().map(str::to_owned).into_iter().collect(),
        reviews: vec![],
        inferred: false,
    }
}

/// Review ops name their patch in `payload.patch` (create and every follow-up).
fn attach_reviews(
    ops: &[VerifiedOp],
    patches: &mut BTreeMap<[u8; 32], Thread>,
    view: Option<&RepoView>,
) {
    for o in ops.iter().filter(|o| o.body.type_name == REVIEW) {
        let p = payload(o);
        let target = p["patch"]
            .as_str()
            .and_then(|s| charter_sync::ids::unhex32(s).ok());
        if let Some(t) = target.and_then(|h| patches.get_mut(&h)) {
            t.reviews.push(entry(o, &p, view));
        }
    }
}

/// Whether the pinned program accepts a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Valid,
    Invalid(Vec<String>),
    /// No program pinned, or the program does not track this tag.
    Unchecked,
}

#[derive(Debug, Clone)]
pub struct Artifact {
    pub name: String,
    pub target: String,
    pub sha256: String,
    pub size: String,
    pub locations: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Release {
    pub id: String,
    pub tag: String,
    pub commit: String,
    pub notes: Option<String>,
    pub author: String,
    pub block: u64,
    pub revisions: usize,
    pub artifacts: Vec<Artifact>,
    pub verdict: Verdict,
}

fn artifacts(p: &Value) -> Vec<Artifact> {
    let list = p["artifacts"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    list.iter()
        .map(|a| Artifact {
            name: text(&a["name"]),
            target: a["target"].as_str().unwrap_or("").to_owned(),
            sha256: text(&a["sha256"]),
            size: text(&a["size"]),
            locations: a["locations"]
                .as_array()
                .map(|l| l.iter().map(text).collect())
                .unwrap_or_default(),
        })
        .collect()
}

fn verdict(view: Option<&RepoView>, tag: &str) -> Verdict {
    let r = view
        .and_then(|v| v.program.as_ref())
        .map(|p| &p.state["releases"][tag]);
    match r {
        Some(r) if r.is_object() && r["valid"] == true => Verdict::Valid,
        Some(r) if r.is_object() => Verdict::Invalid(
            r["missing"]
                .as_array()
                .map(|m| m.iter().map(text).collect())
                .unwrap_or_default(),
        ),
        _ => Verdict::Unchecked,
    }
}

/// Releases, newest first; a `revise` op replaces the artifacts (and notes) it carries.
pub fn releases(ops: &[VerifiedOp], view: Option<&RepoView>) -> Vec<Release> {
    let mut out: Vec<Release> = vec![];
    for o in ops.iter().filter(|o| o.body.type_name == RELEASE) {
        let p = payload(o);
        let obj = hex32(&o.object());
        if o.body.is_create() {
            let tag = text(&p["tag"]);
            out.push(Release {
                id: obj,
                verdict: verdict(view, &tag),
                tag,
                commit: text(&p["commit"]),
                notes: p["notes"].as_str().map(markdown::render),
                author: hex32(&o.author),
                block: o.arkiv_order.block,
                revisions: 0,
                artifacts: artifacts(&p),
            });
        } else if let Some(r) = out.iter_mut().find(|r| r.id == obj) {
            r.revisions += 1;
            if p["artifacts"].is_array() {
                r.artifacts = artifacts(&p);
            }
            if let Some(n) = p["notes"].as_str() {
                r.notes = Some(markdown::render(n));
            }
        }
    }
    out.reverse();
    out
}
