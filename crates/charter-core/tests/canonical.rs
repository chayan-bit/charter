//! Canonical refs by exact-match quorum (design 3.8).

use charter_core::*;
use std::collections::BTreeMap;

fn nid(b: u8) -> Nid {
    [b; 32]
}

fn identity(delegates: &[u8], threshold: u64) -> IdBody {
    IdBody {
        rid: None,
        rev: 0,
        parent: ZERO,
        name: "r".into(),
        description: String::new(),
        default_branch: "main".into(),
        delegates: delegates.iter().map(|b| nid(*b)).collect(),
        threshold,
        visibility: 0,
        lang: None,
        license: None,
        nonce: Some([0; 16]),
        program: None,
        archived: None,
    }
}

fn refs(entries: &[(&str, u8)]) -> RefsBody {
    RefsBody {
        rid: [0; 32],
        object_hash: 1,
        refs: entries
            .iter()
            .map(|(n, o)| RefEntry {
                name: (*n).into(),
                oid: vec![*o; 20],
            })
            .collect(),
    }
}

fn get<'a>(c: &'a CanonicalRefs, name: &str) -> Option<&'a [u8]> {
    c.refs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, o)| o.as_slice())
}

#[test]
fn quorum_picks_the_agreed_oid() {
    let id = identity(&[1, 2, 3], 2);
    let m = BTreeMap::from([
        (nid(1), refs(&[("refs/heads/main", 0xaa)])),
        (nid(2), refs(&[("refs/heads/main", 0xaa)])),
        (nid(3), refs(&[("refs/heads/main", 0xbb)])),
    ]);
    let c = canonical_refs(&id, &m);
    assert_eq!(get(&c, "refs/heads/main"), Some(&[0xaa; 20][..]));
    assert!(c.no_quorum.is_empty());
}

#[test]
fn tie_omits_the_ref() {
    let id = identity(&[1, 2], 1);
    let m = BTreeMap::from([
        (nid(1), refs(&[("refs/heads/main", 0xaa)])),
        (nid(2), refs(&[("refs/heads/main", 0xbb)])),
    ]);
    let c = canonical_refs(&id, &m);
    assert_eq!(get(&c, "refs/heads/main"), None);
    assert_eq!(c.no_quorum, vec!["refs/heads/main".to_owned()]);
}

#[test]
fn threshold_above_votes_omits_the_ref() {
    let id = identity(&[1, 2, 3], 3);
    let m = BTreeMap::from([
        (nid(1), refs(&[("refs/heads/main", 0xaa)])),
        (nid(2), refs(&[("refs/heads/main", 0xaa)])),
    ]);
    let c = canonical_refs(&id, &m);
    assert_eq!(get(&c, "refs/heads/main"), None);
    assert_eq!(c.no_quorum, vec!["refs/heads/main".to_owned()]);
}

#[test]
fn tags_and_default_branch_only_and_non_delegates_ignored() {
    let id = identity(&[1], 1);
    let m = BTreeMap::from([
        (
            nid(1),
            refs(&[
                ("refs/heads/feature", 0x01),
                ("refs/heads/main", 0x02),
                ("refs/notes/commits", 0x03),
                ("refs/tags/v1", 0x04),
            ]),
        ),
        (nid(9), refs(&[("refs/tags/evil", 0x05)])),
    ]);
    let c = canonical_refs(&id, &m);
    let names: Vec<&str> = c.refs.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["refs/heads/main", "refs/tags/v1"]);
}
