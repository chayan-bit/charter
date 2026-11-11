//! Design section 8.1: envelope, size caps, attributes, error codes.

mod common;

use charter_core::*;
use common::*;

fn root() -> IdBody {
    root_body(vec![alice().nid], 1)
}

#[test]
fn envelope_round_trips_and_verifies() {
    let (bytes, _) = sign(alice(), &root());
    let env = Envelope::decode(&bytes).expect("decode");
    assert_eq!(env.kind, Kind::Id);
    assert_eq!(env.signer, alice().nid);
    assert_eq!(env.encode(), bytes);
    assert!(env.verify(&alice().pk).is_ok());
    assert!(env.verify(&peers()[1].pk).is_err());
}

/// Envelope byte offsets: [0]=array(6) [1]=v [2]=kind [3]=enc [4..6]=bstr(32) hdr.
#[test]
fn kind_a_signature_never_verifies_as_kind_b() {
    let (bytes, _) = sign(alice(), &refs_body(1));
    let mut relabelled = bytes.clone();
    assert_eq!(relabelled[2], Kind::Refs.code());
    relabelled[2] = Kind::Anchor.code();
    let env = Envelope::decode(&relabelled).expect("still canonical");
    assert!(env.verify(&alice().pk).is_err());
}

#[test]
fn flipping_any_signed_field_fails() {
    let (bytes, _) = sign(alice(), &refs_body(1));
    let env = Envelope::decode(&bytes).expect("decode");
    let mut body = env.clone();
    body.body[3] ^= 1;
    assert!(body.verify(&alice().pk).is_err());
    let mut signer = env.clone();
    signer.signer[0] ^= 1;
    assert!(signer.verify(&alice().pk).is_err());
    for i in 0..env.body.len() {
        let mut e = env.clone();
        e.body[i] ^= 0x80;
        assert!(e.verify(&alice().pk).is_err(), "body byte {i}");
    }
}

#[test]
fn unknown_envelope_version_or_enc_is_unsupported() {
    let (bytes, _) = sign(alice(), &refs_body(1));
    let mut enc1 = bytes.clone();
    enc1[3] = 0x01;
    assert!(matches!(
        Envelope::decode(&enc1),
        Err(FormatError::Unsupported { .. })
    ));
    let mut v2 = bytes;
    v2[1] = 0x02;
    assert_eq!(
        Envelope::decode(&v2).map_err(|e| e.code()),
        Err(ErrorCode::UnsupportedVersion)
    );
}

#[test]
fn manifest_of_1600_chunks_fits_under_the_body_cap() {
    let body = manifest_body(1600).encode();
    for sig_len in [charter_crypto::SIG_LEN, 4627, 4741] {
        assert!(body.len() <= max_body_len(sig_len), "sig_len {sig_len}");
    }
    let (env, _) = sign(alice(), &manifest_body(1600));
    assert!(env.len() <= AG_BODY_MAX);
}

fn refs_of_len(target: usize) -> RefsBody {
    let base = RefsBody {
        rid: RID,
        object_hash: 1,
        refs: vec![],
    };
    let mut refs = vec![];
    let mut i = 0u32;
    loop {
        let name = format!("refs/heads/b{i:06}");
        refs.push(RefEntry {
            name,
            oid: vec![0xab; 20],
        });
        let len = RefsBody {
            refs: refs.clone(),
            ..base.clone()
        }
        .encode()
        .len();
        if len > target {
            refs.pop();
            break;
        }
        i += 1;
    }
    // pad the last name until the body is exactly `target`
    loop {
        let len = RefsBody {
            refs: refs.clone(),
            ..base.clone()
        }
        .encode()
        .len();
        if len == target {
            return RefsBody { refs, ..base };
        }
        let last = refs.last_mut().expect("non-empty");
        last.name.push('x');
        assert!(len < target, "overshot at {len}");
    }
}

#[test]
fn refs_at_the_cap_encode_and_one_byte_more_fails() {
    let cap = max_body_len(charter_crypto::SIG_LEN);
    let at = refs_of_len(cap);
    assert!(Envelope::sign(&alice().sk, Kind::Refs, at.encode()).is_ok());
    let over = refs_of_len(cap + 1);
    assert_eq!(
        Envelope::sign(&alice().sk, Kind::Refs, over.encode()).map_err(|e| e.code()),
        Err(ErrorCode::RefsTooLarge)
    );
}

fn check_rules_and_roundtrip<B: Body>(body: &B) {
    let (obs, _) = observe(alice(), body, addr(1), 10, 0);
    check_arkiv_rules(&obs.attributes).expect("arkiv rules");
    open::<B>(&obs).expect("attributes match body");
    for a in &obs.attributes {
        if let AttrValue::Bytes(_) = a.value {
            assert!(!a.indexed(), "{} must not be indexed", a.name);
        }
    }
}

#[test]
fn generated_attributes_obey_arkiv_rules_and_match_body() {
    check_rules_and_roundtrip(&peer_body(alice()));
    check_rules_and_roundtrip(&bind_body(0, ZERO, addr(1), addr(2)));
    check_rules_and_roundtrip(&root());
    check_rules_and_roundtrip(&IdAccBody {
        rid: RID,
        rev: h32(3),
    });
    check_rules_and_roundtrip(&refs_body(1));
    check_rules_and_roundtrip(&anchor_body(0, ZERO, h32(1), h32(2), 10));
    check_rules_and_roundtrip(&manifest_body(1600));
    let chunk = chunk_attributes(&RID, &h32(1), 3, &[1, 2, 3]);
    check_arkiv_rules(&chunk).expect("chunk rules");
}

#[test]
fn tampered_attribute_is_rejected() {
    let (mut obs, _) = observe(
        alice(),
        &anchor_body(0, ZERO, h32(1), h32(2), 10),
        addr(1),
        10,
        0,
    );
    let seq = obs
        .attributes
        .iter_mut()
        .find(|a| a.name == "ag.seq")
        .expect("seq");
    seq.value = AttrValue::U64(7);
    assert!(open::<AnchorBody>(&obs).is_err());
}

#[test]
fn arkiv_rule_violations_are_detected() {
    let a = |n: &str, v: AttrValue| Attribute::new(n, v);
    assert!(check_arkiv_rules(&[a("9bad", AttrValue::U64(1))]).is_err());
    assert!(check_arkiv_rules(&[a(&"x".repeat(33), AttrValue::U64(1))]).is_err());
    assert!(check_arkiv_rules(&[a("s", AttrValue::Str("y".repeat(129)))]).is_err());
    assert!(check_arkiv_rules(&[a("b", AttrValue::U64(1)), a("a", AttrValue::U64(1))]).is_err());
    let many: Vec<_> = (0..33)
        .map(|i| a(&format!("a{i:02}"), AttrValue::U64(1)))
        .collect();
    assert!(check_arkiv_rules(&many).is_err());
    assert!(check_arkiv_rules(&[a("a", AttrValue::Bytes(vec![0; 131_073]))]).is_err());
}

#[test]
fn wrong_kind_attribute_is_rejected() {
    let (obs, _) = observe(alice(), &refs_body(1), addr(1), 10, 0);
    assert!(open::<AnchorBody>(&obs).is_err());
}

#[test]
fn error_codes_match_section_7_3() {
    let all = [
        (ErrorCode::NotLive, "E_NOT_LIVE", false),
        (ErrorCode::ControlNotLive, "E_CONTROL_NOT_LIVE", false),
        (ErrorCode::IdNotLive, "E_ID_NOT_LIVE", false),
        (ErrorCode::StaleHead, "E_STALE_HEAD", false),
        (ErrorCode::StaleIdentity, "E_STALE_IDENTITY", false),
        (ErrorCode::HistoryRewritten, "E_HISTORY_REWRITTEN", false),
        (ErrorCode::ChainGap, "E_CHAIN_GAP", false),
        (ErrorCode::BadSig, "E_BAD_SIG", false),
        (ErrorCode::BadFormat, "E_BAD_FORMAT", false),
        (ErrorCode::BadRefs, "E_BAD_REFS", false),
        (ErrorCode::BadPack, "E_BAD_PACK", false),
        (ErrorCode::MissingObjects, "E_MISSING_OBJECTS", false),
        (ErrorCode::MissingDependency, "E_MISSING_DEPENDENCY", false),
        (
            ErrorCode::UnsupportedVersion,
            "E_UNSUPPORTED_VERSION",
            false,
        ),
        (ErrorCode::Equivocation, "E_EQUIVOCATION", false),
        (ErrorCode::ConcurrentPush, "E_CONCURRENT_PUSH", true),
        (ErrorCode::LostRace, "E_LOST_RACE", true),
        (ErrorCode::UploadIncomplete, "E_UPLOAD_INCOMPLETE", true),
        (ErrorCode::InsufficientFunds, "E_INSUFFICIENT_FUNDS", false),
        (ErrorCode::RefsTooLarge, "E_REFS_TOO_LARGE", false),
        (ErrorCode::Rpc, "E_RPC", true),
        (ErrorCode::RateLimited, "E_RATE_LIMITED", true),
        (ErrorCode::InconsistentView, "E_INCONSISTENT_VIEW", true),
        (ErrorCode::Paging, "E_PAGING", true),
        (ErrorCode::WrongChain, "E_WRONG_CHAIN", false),
        (ErrorCode::ProgramMissing, "E_PROGRAM_MISSING", false),
        (ErrorCode::ProgramRejected, "E_PROGRAM_REJECTED", false),
        (ErrorCode::NotMember, "E_NOT_MEMBER", false),
        (ErrorCode::RoleDenied, "E_ROLE_DENIED", false),
        (ErrorCode::UnsealedPrivate, "E_UNSEALED_PRIVATE", false),
        (ErrorCode::Internal, "E_INTERNAL", false),
    ];
    for (c, s, r) in all {
        assert_eq!(c.as_str(), s);
        assert_eq!(c.retriable(), r, "{s}");
        assert_eq!(c.to_string(), s);
    }
}
