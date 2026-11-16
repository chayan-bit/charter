//! `programs/agent-bounty.rhai`: scoped agent grants, revocation, caps and approvals.

mod common;

use charter_program::{Event, Outcome, ProofCheck};
use common::*;
use serde_json::{Value, json};

const AGENT: u8 = 0xa9;
const ISSUE: u8 = 0x60;
const PATCH: u8 = 0x50;
const GRANT: u8 = 0x30;
const IMAGE: &str = "326fefc7f5cc5aaa99f2691046dfafe0ff62496174f1eb2b93aaa4840854a0ca";

fn grant(order: u64, scopes: &[&str], max_bounty: u64, until: u64) -> Event {
    let payload =
        json!({"agent": hx(AGENT), "scopes": scopes, "max_bounty": max_bounty, "until": until});
    op(order, 0xd1, GRANT, "xyz.arkiv.agent", "grant", payload)
}

fn revoke(order: u64, by: u8) -> Event {
    let payload = json!({"agent": hx(AGENT)});
    op(order, by, GRANT, "xyz.arkiv.agent", "revoke", payload)
}

fn issue(order: u64, bounty: u64) -> Event {
    let payload = json!({"title": "bug", "bounty": bounty});
    op(order, 0xd1, ISSUE, "xyz.arkiv.issue", "create", payload)
}

fn patch(order: u64, author: u8, object: u8, head: u8) -> Event {
    let payload = json!({"head": oid_hex(head), "issue": hx(ISSUE)});
    op(order, author, object, "xyz.arkiv.patch", "create", payload)
}

fn approve(order: u64, by: u8, object: u8) -> Event {
    let payload = json!({"patch": hx(object)});
    op(order, by, 0x51, "xyz.arkiv.review", "approve", payload)
}

fn merged(order: u64, object: u8) -> Event {
    let payload = json!({"patch": hx(object), "closes": hx(ISSUE)});
    op(order, 0xd1, object, "xyz.arkiv.patch", "merged", payload)
}

fn comment(order: u64, author: u8) -> Event {
    op(
        order,
        author,
        0x52,
        "xyz.arkiv.comment",
        "create",
        json!({"body": "hi"}),
    )
}

fn fold(events: Vec<Event>) -> Outcome {
    template("agent-bounty").fold(repo(), events).expect("fold")
}

fn rejected(o: &Outcome) -> Value {
    state(o)["rejected"].clone()
}

/// Grant (cap 100), issue (bounty), agent patch, human approval, merge, main to the head.
fn happy(bounty: u64) -> Vec<Event> {
    vec![
        grant(1, &["patch"], 100, 1000),
        issue(2, bounty),
        patch(3, AGENT, PATCH, 1),
        approve(4, 0xd2, PATCH),
        merged(5, PATCH),
        main(6, None, 1),
    ]
}

#[test]
fn granted_agent_patch_is_merged_and_paid_to_the_agent() {
    let o = fold(happy(100));
    assert_eq!(o.accepted.len(), 1, "{:?}", o.rejected);
    assert_eq!(o.obligations.len(), 1, "{}", o.state_json);
    assert_eq!(o.obligations[0]["to"], hx(AGENT));
    assert_eq!(o.obligations[0]["amount"], 100);
    assert_eq!(rejected(&o), empty());
}

#[test]
fn out_of_scope_agent_op_is_ignored_and_recorded() {
    let o = fold(vec![grant(1, &["patch"], 100, 1000), comment(2, AGENT)]);
    let r = rejected(&o);
    let reason = r[hex::encode([2u8; 32])].as_str().expect("recorded");
    assert!(reason.contains("scope comment not granted"), "{reason}");
    assert!(
        fold(vec![comment(2, AGENT)])
            .state_json
            .contains("\"rejected\":{}")
    );
}

#[test]
fn expired_grant_rejects_later_agent_ops() {
    let mut events = happy(100);
    events[0] = grant(1, &["patch"], 100, 2);
    let o = fold(events);
    let reason = rejected(&o)[hex::encode([3u8; 32])].clone();
    assert!(
        reason
            .as_str()
            .expect("recorded")
            .contains("expired at order 2")
    );
    assert!(o.obligations.is_empty());
}

#[test]
fn revoke_stops_later_ops_but_earlier_ones_still_count() {
    let mut events = happy(100);
    events.insert(1, revoke(2, 0xd2)); // order 2 sits before the patch at order 3
    let o = fold(events);
    assert!(o.obligations.is_empty(), "{}", o.state_json);
    assert!(rejected(&o).to_string().contains("revoked at order 2"));

    let mut late = happy(100);
    late.push(revoke(7, 0xd1));
    late.push(patch(8, AGENT, 0x53, 2));
    let o = fold(late);
    assert_eq!(
        o.obligations.len(),
        1,
        "the patch before the revoke is paid"
    );
    assert!(state(&o)["patches"].get(hx(0x53)).is_none());
}

#[test]
fn revoke_by_a_stranger_is_ignored() {
    let mut events = happy(100);
    events.insert(1, revoke(2, 0x77));
    let o = fold(events);
    assert_eq!(o.obligations.len(), 1);
    assert!(
        rejected(&o)
            .to_string()
            .contains("granter or a human delegate")
    );
}

#[test]
fn grant_by_a_non_delegate_confers_nothing() {
    let payload = json!({"agent": hx(AGENT), "scopes": ["patch"], "max_bounty": 1, "until": 9});
    let o = fold(vec![op(
        1,
        0x77,
        GRANT,
        "xyz.arkiv.agent",
        "grant",
        payload,
    )]);
    assert_eq!(state(&o)["agents"], empty());
    assert!(rejected(&o).to_string().contains("only a human delegate"));
}

#[test]
fn bounty_above_the_cap_gets_no_pay_obligation() {
    let o = fold(happy(150));
    assert_eq!(o.accepted.len(), 1);
    assert!(o.obligations.is_empty(), "{:?}", o.obligations);
    let key = format!("bounty:{}", hx(ISSUE));
    assert!(
        rejected(&o)[key]
            .as_str()
            .expect("recorded")
            .contains("cap 100")
    );
}

#[test]
fn agent_self_approval_does_not_count() {
    let mut events = happy(100);
    events[3] = approve(4, AGENT, PATCH);
    events[0] = grant(1, &["patch", "review"], 100, 1000);
    let o = fold(events);
    assert!(o.accepted.is_empty());
    assert!(o.rejected[0].1.contains("needs a human delegate approval"));
    assert!(rejected(&o).to_string().contains("not a human delegate"));
}

#[test]
fn agent_listed_as_delegate_still_cannot_approve_or_grant() {
    let mut r = repo();
    r.delegates.push(id(AGENT));
    let mut events = happy(100);
    events[0] = grant(1, &["patch", "review", "agent"], 100, 1000);
    events[3] = approve(4, AGENT, PATCH);
    let o = template("agent-bounty").fold(r, events).expect("fold");
    assert!(o.accepted.is_empty(), "{:?}", o.accepted);
}

fn proof(ok: bool) -> ProofCheck {
    ProofCheck {
        kind: "risc0".into(),
        image_id: IMAGE.into(),
        ok,
        public: json!({"spec_hash": "ab"}),
    }
}

fn with_proof(mut e: Event, p: ProofCheck) -> Event {
    if let Event::Op(o) = &mut e {
        o.proofs.push(p);
    }
    e
}

#[test]
fn proof_policy_records_or_requires_a_valid_proof() {
    let mut events = happy(100);
    events[2] = with_proof(patch(3, AGENT, PATCH, 1), proof(true));
    let o = fold(events);
    assert_eq!(state(&o)["patches"][hx(PATCH)]["proven"], true);

    let src = std::fs::read_to_string(format!(
        "{}/../../programs/agent-bounty.rhai",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("source");
    let strict = load(&src.replace("    \"optional\"\n", "    \"required\"\n"));
    let mut events = happy(100);
    events[2] = with_proof(patch(3, AGENT, PATCH, 1), proof(false));
    let o = strict.fold(repo(), events).expect("fold");
    assert!(o.obligations.is_empty());
    assert!(rejected(&o).to_string().contains("needs a matching proof"));
}

#[test]
fn human_patches_keep_plain_bounty_rules() {
    let events = vec![
        issue(1, 500),
        patch(2, 5, PATCH, 1),
        merged(3, PATCH),
        main(4, None, 1),
    ];
    let o = fold(events);
    assert_eq!(o.obligations[0]["to"], hx(5));
    assert_eq!(o.obligations[0]["amount"], 500);
}

#[test]
fn fold_is_deterministic() {
    let mut events = happy(150);
    events.push(comment(7, AGENT));
    let (a, b) = (fold(events.clone()), fold(events));
    assert_eq!(a.state_json, b.state_json);
    assert_eq!(a.obligations, b.obligations);
}
