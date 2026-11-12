//! Labels, assignees and a milestone on an issue or patch (issue #40, docs/p3/cob-ops.md
//! "Labels, assignees and milestones"), folded in Arkiv order from the create payload and
//! `label`, `unlabel`, `assign`, `unassign` and `milestone` ops. Payloads come from the chain,
//! so every value is checked here and a bad one is dropped, never shown.

use std::collections::BTreeSet;

use serde_json::Value;

/// Values one op may name; the rest of a longer list is ignored.
pub const MAX_PER_OP: usize = 16;
/// Characters of a label.
pub const LABEL_MAX: usize = 48;
/// Characters of a milestone.
pub const MILESTONE_MAX: usize = 64;
/// Labels or assignees one thread keeps; later additions past it are ignored.
const MAX_KEPT: usize = 64;

/// Ops this module folds.
pub const ACTIONS: [&str; 5] = ["label", "unlabel", "assign", "unassign", "milestone"];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Triage {
    pub labels: BTreeSet<String>,
    /// Peer ids, hex.
    pub assignees: BTreeSet<String>,
    pub milestone: Option<String>,
}

/// A label or milestone: trimmed, 1 to `max` characters, no control characters.
pub fn clean(s: &str, max: usize) -> Option<String> {
    let t = s.trim();
    let ok = !t.is_empty() && t.chars().count() <= max && !t.chars().any(char::is_control);
    ok.then(|| t.to_owned())
}

/// 64 lowercase hex digits.
pub fn is_nid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The valid strings of `v[key]`, at most [`MAX_PER_OP`].
fn list(v: &Value, key: &str, ok: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let items = v[key].as_array().map(Vec::as_slice).unwrap_or_default();
    items
        .iter()
        .take(MAX_PER_OP)
        .filter_map(|x| ok(x.as_str()?))
        .collect()
}

pub fn labels(v: &Value) -> Vec<String> {
    list(v, "labels", |s| clean(s, LABEL_MAX))
}

pub fn assignees(v: &Value) -> Vec<String> {
    list(v, "assignees", |s| is_nid(s).then(|| s.to_owned()))
}

fn add(set: &mut BTreeSet<String>, items: Vec<String>) {
    for i in items {
        if set.len() < MAX_KEPT {
            set.insert(i);
        }
    }
}

/// What a `milestone` op sets: `Some(None)` clears (`null` or ""), `Some(Some(m))` sets,
/// and `None` (an invalid value) changes nothing.
pub fn milestone(p: &Value) -> Option<Option<String>> {
    match &p["milestone"] {
        Value::Null => Some(None),
        Value::String(s) if s.trim().is_empty() => Some(None),
        Value::String(s) => clean(s, MILESTONE_MAX).map(Some),
        _ => None,
    }
}

impl Triage {
    /// The initial values a create payload carries.
    pub fn of_create(p: &Value) -> Self {
        let mut t = Self::default();
        add(&mut t.labels, labels(p));
        add(&mut t.assignees, assignees(p));
        t.milestone = p["milestone"]
            .as_str()
            .and_then(|m| clean(m, MILESTONE_MAX));
        t
    }

    /// Applies one follow-up op; any other action changes nothing.
    pub fn apply(&mut self, action: &str, p: &Value) {
        match action {
            "label" => add(&mut self.labels, labels(p)),
            "unlabel" => labels(p).iter().for_each(|l| _ = self.labels.remove(l)),
            "assign" => add(&mut self.assignees, assignees(p)),
            "unassign" => assignees(p)
                .iter()
                .for_each(|a| _ = self.assignees.remove(a)),
            "milestone" => {
                if let Some(m) = milestone(p) {
                    self.milestone = m;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ops_fold_in_order_and_bad_values_are_dropped() {
        let a = "ab".repeat(32);
        let mut t = Triage::of_create(&json!({
            "labels": ["bug", " ui ", "", "x".repeat(49), 7, "a\u{7}b"],
            "assignees": [a, "nope", "AB".repeat(32)],
            "milestone": "v1",
        }));
        assert_eq!(t.labels, ["bug", "ui"].map(String::from).into());
        assert_eq!(t.assignees, [a.clone()].into());
        assert_eq!(t.milestone.as_deref(), Some("v1"));
        t.apply("label", &json!({"labels": ["docs", "bug"]}));
        t.apply("unlabel", &json!({"labels": ["ui"]}));
        t.apply("unassign", &json!({"assignees": [a]}));
        t.apply("milestone", &json!({"milestone": 3}));
        assert_eq!(t.milestone.as_deref(), Some("v1"), "a bad value keeps it");
        t.apply("milestone", &json!({"milestone": null}));
        t.apply("close", &json!({"labels": ["ignored"]}));
        assert_eq!(t.labels, ["bug", "docs"].map(String::from).into());
        assert!(t.assignees.is_empty() && t.milestone.is_none());
    }

    #[test]
    fn lists_are_capped_per_op_and_per_thread() {
        let many: Vec<String> = (0..40).map(|i| format!("l{i}")).collect();
        let mut t = Triage::default();
        t.apply("label", &json!({ "labels": many }));
        assert_eq!(t.labels.len(), MAX_PER_OP);
        for i in 0..10 {
            let batch: Vec<String> = (0..16).map(|j| format!("m{i}-{j}")).collect();
            t.apply("label", &json!({ "labels": batch }));
        }
        assert_eq!(t.labels.len(), MAX_KEPT);
    }
}
