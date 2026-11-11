//! Write forms to [`Draft`]s. Fields are untrusted: ids must be 64 hex digits, commits
//! hex oids, text non-empty where the op needs it; payload shapes follow docs/p3 and the
//! demo scripts (`chart op post` payloads), so the CLI and programs read them unchanged.

use std::collections::HashMap;

use charter_core::Rid;
use charter_sync::ids::{hex32, unhex32};
use charter_sync::release::{Publish, is_oid};
use charter_sync::units::glm;
use serde_json::{Value, json};

use super::{Draft, Obj, Step};

pub const ISSUE: &str = "xyz.arkiv.issue";
pub const PATCH: &str = "xyz.arkiv.patch";
pub const REVIEW: &str = "xyz.arkiv.review";
pub const BOUNTY: &str = "xyz.arkiv.bounty";
/// Op titles stay short; bodies may use the whole payload.
const TITLE_MAX: usize = 256;

pub type Fields = HashMap<String, String>;

fn get<'a>(f: &'a Fields, k: &str) -> &'a str {
    f.get(k).map_or("", |v| v.trim())
}

fn need<'a>(f: &'a Fields, k: &str) -> Result<&'a str, String> {
    match get(f, k) {
        "" => Err(format!("{k} is required")),
        v => Ok(v),
    }
}

fn title(f: &Fields) -> Result<&str, String> {
    let t = need(f, "title")?;
    match t.chars().count() <= TITLE_MAX {
        true => Ok(t),
        false => Err(format!("title is longer than {TITLE_MAX} characters")),
    }
}

/// A 64-hex op id field, kept as hex for the payload.
fn id(f: &Fields, k: &str) -> Result<(charter_core::Hash32, String), String> {
    let v = need(f, k)?.to_ascii_lowercase();
    let h = unhex32(&v).map_err(|_| format!("{k} must be a 64 hex digit id"))?;
    Ok((h, v))
}

fn oid<'a>(f: &'a Fields, k: &str) -> Result<&'a str, String> {
    let v = need(f, k)?;
    match is_oid(v) {
        true => Ok(v),
        false => Err(format!("{k} must be a lowercase hex commit id")),
    }
}

fn op(type_name: &'static str, action: &'static str, object: Obj, payload: Value) -> Step {
    Step::Op {
        type_name,
        action,
        object,
        payload,
    }
}

fn lines(f: &Fields, k: &str) -> Vec<String> {
    get(f, k)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `back` if it is a page of this repo, else the repo home.
fn back(rid: &Rid, f: &Fields) -> String {
    let home = format!("/r/{}", hex32(rid));
    let b = get(f, "back");
    let inside = b == home || b.strip_prefix(&home).is_some_and(|r| r.starts_with('/'));
    match inside && !b.contains("//") && !b.contains(['\\', '\r', '\n']) {
        true => b.to_owned(),
        false => home,
    }
}

fn steps(rid: &Rid, f: &Fields) -> Result<(String, Vec<Step>), String> {
    Ok(match get(f, "kind") {
        "remove-revoked" => ("Remove revoked devices".into(), vec![Step::RemoveRevoked]),
        "issue" => {
            let p = json!({"title": title(f)?, "body": get(f, "body")});
            ("Open issue".into(), vec![op(ISSUE, "create", Obj::New, p)])
        }
        "comment" => {
            let (issue, _) = id(f, "issue")?;
            let p = json!({"body": need(f, "body")?});
            (
                "Comment".into(),
                vec![op(ISSUE, "comment", Obj::Id(issue), p)],
            )
        }
        "review" => review(f)?,
        "merge" => {
            let (patch, hex) = id(f, "patch")?;
            let mut p = json!({"patch": hex});
            if !get(f, "closes").is_empty() {
                p["closes"] = json!(id(f, "closes")?.1);
            }
            (
                "Mark patch merged".into(),
                vec![op(PATCH, "merged", Obj::Id(patch), p)],
            )
        }
        "claim" => {
            let (_, issue) = id(f, "issue")?;
            let mut p = json!({"title": title(f)?, "head": oid(f, "head")?, "closes": issue});
            if !get(f, "base").is_empty() {
                p["base"] = json!(oid(f, "base")?);
            }
            (
                "Claim bounty".into(),
                vec![op(PATCH, "create", Obj::New, p)],
            )
        }
        "fund" => fund(f)?,
        "release" => {
            let notes = Some(get(f, "notes").to_owned()).filter(|n| !n.is_empty());
            let p = Publish {
                rid: *rid,
                tag: need(f, "tag")?.to_owned(),
                commit: oid(f, "commit")?.to_owned(),
                artifacts: lines(f, "artifacts"),
                locations: lines(f, "locations"),
                notes,
            };
            if p.artifacts.is_empty() {
                return Err("a release needs at least one artifact".into());
            }
            (format!("Publish release {}", p.tag), vec![Step::Release(p)])
        }
        k => return Err(format!("unknown write {k:?}")),
    })
}

/// A review object, then its verdict (`approve` at a head, or a `comment`).
fn review(f: &Fields) -> Result<(String, Vec<Step>), String> {
    let (_, patch) = id(f, "patch")?;
    let create = op(REVIEW, "create", Obj::New, json!({"patch": patch}));
    let body = get(f, "body");
    let (label, verdict) = match get(f, "verdict") {
        "approve" => {
            let mut p = json!({"patch": patch, "head": oid(f, "head")?});
            if !body.is_empty() {
                p["body"] = json!(body);
            }
            ("Approve patch", op(REVIEW, "approve", Obj::Prev, p))
        }
        "comment" => {
            let p = json!({"patch": patch, "body": need(f, "body")?});
            ("Review patch", op(REVIEW, "comment", Obj::Prev, p))
        }
        v => return Err(format!("unknown verdict {v:?}")),
    };
    Ok((label.into(), vec![create, verdict]))
}

/// A pledge that raises the issue's bounty (`bounty.rhai` counts `pledge` ops; a follow-up
/// op needs an object, so a bounty object is created for it), and the escrow deposit that
/// funds it under the purpose the program pays from.
fn fund(f: &Fields) -> Result<(String, Vec<Step>), String> {
    let (_, issue) = id(f, "issue")?;
    let wei = glm(need(f, "amount")?)?;
    // Programs fold in rhai, whose integers are i64.
    let n = i64::try_from(wei).map_err(|_| "amount is too large for the bounty program")?;
    if n == 0 {
        return Err("amount must be more than zero".into());
    }
    let purpose = format!("bounty:{issue}");
    let create = op(BOUNTY, "create", Obj::New, json!({"issue": issue}));
    let pledge = op(
        BOUNTY,
        "pledge",
        Obj::Prev,
        json!({"issue": issue, "amount": n}),
    );
    let deposit = Step::Deposit {
        amount: wei,
        purpose,
    };
    Ok(("Fund bounty".into(), vec![create, pledge, deposit]))
}

/// The draft a write form describes.
pub fn draft(rid: &Rid, f: &Fields) -> Result<Draft, String> {
    let (label, steps) = steps(rid, f)?;
    Ok(Draft {
        rid: *rid,
        label,
        back: back(rid, f),
        steps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(kv: &[(&str, &str)]) -> Fields {
        kv.iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    const RID: Rid = [7; 32];

    #[test]
    fn a_review_is_a_create_then_an_approve_of_that_review() {
        let p = "ab".repeat(32);
        let h = "c".repeat(40);
        let f = fields(&[
            ("kind", "review"),
            ("patch", &p),
            ("verdict", "approve"),
            ("head", &h),
        ]);
        let d = draft(&RID, &f).expect("draft");
        assert_eq!(d.label, "Approve patch");
        assert_eq!(
            d.steps[0],
            op(REVIEW, "create", Obj::New, json!({"patch": p}))
        );
        let want = op(REVIEW, "approve", Obj::Prev, json!({"patch": p, "head": h}));
        assert_eq!(d.steps[1], want);
    }

    #[test]
    fn funding_pledges_and_deposits_the_same_wei() {
        let i = "01".repeat(32);
        let d = draft(
            &RID,
            &fields(&[("kind", "fund"), ("issue", &i), ("amount", "0.5")]),
        )
        .expect("draft");
        let wei = 500_000_000_000_000_000_i64;
        let pledge = json!({"issue": i, "amount": wei});
        let create = op(BOUNTY, "create", Obj::New, json!({"issue": i}));
        assert_eq!(
            d.steps[..2],
            [create, op(BOUNTY, "pledge", Obj::Prev, pledge)]
        );
        let purpose = format!("bounty:{i}");
        assert!(matches!(&d.steps[2], Step::Deposit { amount, purpose: p }
            if *amount == charter_store::U256::from(wei) && *p == purpose));
        assert!(
            draft(
                &RID,
                &fields(&[("kind", "fund"), ("issue", &i), ("amount", "0")])
            )
            .is_err()
        );
        let big = fields(&[("kind", "fund"), ("issue", &i), ("amount", "10")]);
        assert!(draft(&RID, &big).is_err(), "over i64 wei");
    }

    #[test]
    fn bad_fields_are_refused_and_back_stays_in_the_repo() {
        let home = format!("/r/{}", hex32(&RID));
        for f in [
            fields(&[("kind", "issue")]),
            fields(&[("kind", "comment"), ("issue", "xyz"), ("body", "b")]),
            fields(&[
                ("kind", "claim"),
                ("issue", &"0".repeat(64)),
                ("title", "t"),
                ("head", "HEAD"),
            ]),
            fields(&[
                ("kind", "release"),
                ("tag", "v1"),
                ("commit", &"a".repeat(40)),
            ]),
            fields(&[("kind", "drop-table")]),
        ] {
            assert!(draft(&RID, &f).is_err(), "{f:?}");
        }
        let issue = |b: &str| {
            let f = fields(&[("kind", "issue"), ("title", "t"), ("back", b)]);
            draft(&RID, &f).expect("draft").back
        };
        assert_eq!(issue(&format!("{home}/issues")), format!("{home}/issues"));
        for evil in [
            "https://evil.example",
            "//evil.example",
            "/r/other",
            &format!("{home}x"),
            &format!("{home}//evil"),
        ] {
            assert_eq!(issue(evil), home, "{evil}");
        }
    }
}
