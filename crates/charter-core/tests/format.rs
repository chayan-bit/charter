//! Design section 8.1: canonical CBOR and body field rules.

mod common;

use charter_core::*;
use common::*;
use minicbor::Encoder;

fn root() -> IdBody {
    root_body(vec![alice().nid], 1)
}

#[test]
fn every_body_round_trips() {
    fn rt<B: Body + PartialEq + std::fmt::Debug>(b: B) {
        assert_eq!(B::decode(&b.encode()).expect("decode"), b);
    }
    rt(peer_body(alice()));
    rt(bind_body(0, ZERO, addr(1), addr(2)));
    rt(root());
    rt(IdAccBody {
        rid: RID,
        rev: h32(3),
    });
    rt(refs_body(7));
    rt(anchor_body(0, ZERO, h32(1), h32(2), 10));
    rt(manifest_body(3));
}

fn map(entries: &[(u32, u64)]) -> Vec<u8> {
    let mut e = Encoder::new(Vec::new());
    e.map(entries.len() as u64).expect("enc");
    for (k, v) in entries {
        e.u32(*k).expect("enc").u64(*v).expect("enc");
    }
    e.into_writer()
}

fn idacc_bytes(extra: impl FnOnce(&mut Encoder<Vec<u8>>)) -> Vec<u8> {
    let mut e = Encoder::new(Vec::new());
    extra(&mut e);
    e.into_writer()
}

#[test]
fn canonical_idacc_decodes() {
    let good = IdAccBody {
        rid: RID,
        rev: h32(3),
    }
    .encode();
    let rebuilt = idacc_bytes(|e| {
        e.map(2)
            .expect("e")
            .u8(1)
            .expect("e")
            .bytes(&RID)
            .expect("e");
        e.u8(2).expect("e").bytes(&h32(3)).expect("e");
    });
    assert_eq!(good, rebuilt);
    assert!(IdAccBody::decode(&rebuilt).is_ok());
}

#[test]
fn rejects_non_shortest_integer() {
    let mut b = IdAccBody {
        rid: RID,
        rev: h32(3),
    }
    .encode();
    // map(2) key 1 -> encode key 1 as 0x18 0x01
    assert_eq!(b[1], 0x01);
    b.splice(1..2, [0x18, 0x01]);
    assert!(IdAccBody::decode(&b).is_err());
}

#[test]
fn rejects_indefinite_length() {
    let b = idacc_bytes(|e| {
        e.begin_map()
            .expect("e")
            .u8(1)
            .expect("e")
            .bytes(&RID)
            .expect("e");
        e.u8(2)
            .expect("e")
            .bytes(&h32(3))
            .expect("e")
            .end()
            .expect("e");
    });
    assert!(IdAccBody::decode(&b).is_err());
}

#[test]
fn rejects_unsorted_keys() {
    let b = idacc_bytes(|e| {
        e.map(2)
            .expect("e")
            .u8(2)
            .expect("e")
            .bytes(&h32(3))
            .expect("e");
        e.u8(1).expect("e").bytes(&RID).expect("e");
    });
    assert!(IdAccBody::decode(&b).is_err());
}

#[test]
fn rejects_duplicate_keys() {
    let b = idacc_bytes(|e| {
        e.map(3)
            .expect("e")
            .u8(1)
            .expect("e")
            .bytes(&RID)
            .expect("e");
        e.u8(2).expect("e").bytes(&h32(3)).expect("e");
        e.u8(2).expect("e").bytes(&h32(4)).expect("e");
    });
    assert!(IdAccBody::decode(&b).is_err());
}

#[test]
fn rejects_unknown_keys() {
    let b = idacc_bytes(|e| {
        e.map(3)
            .expect("e")
            .u8(1)
            .expect("e")
            .bytes(&RID)
            .expect("e");
        e.u8(2).expect("e").bytes(&h32(3)).expect("e");
        e.u8(9).expect("e").u8(0).expect("e");
    });
    assert!(IdAccBody::decode(&b).is_err());
}

#[test]
fn rejects_trailing_bytes() {
    let mut b = IdAccBody {
        rid: RID,
        rev: h32(3),
    }
    .encode();
    b.push(0x00);
    assert_eq!(IdAccBody::decode(&b), Err(FormatError::TrailingBytes));
}

#[test]
fn rejects_floats_and_tags() {
    let a = anchor_body(5, h32(9), h32(1), h32(2), 10).encode();
    // seq (key 2) holds 0x05; replace with half-float 0xf9 0x45 0x00 (5.0)
    let pos = a.windows(2).position(|w| w == [0x02, 0x05]).expect("seq");
    let mut float = a.clone();
    float.splice(pos + 1..pos + 2, [0xf9, 0x45, 0x00]);
    assert!(AnchorBody::decode(&float).is_err());
    let mut tagged = a.clone();
    tagged.insert(pos + 1, 0xc0);
    assert!(AnchorBody::decode(&tagged).is_err());
    let mut tagged_root = a;
    tagged_root.insert(0, 0xd8);
    tagged_root.insert(1, 0x18);
    assert!(AnchorBody::decode(&tagged_root).is_err());
}

#[test]
fn rejects_wrong_bstr_size() {
    let b = idacc_bytes(|e| {
        e.map(2)
            .expect("e")
            .u8(1)
            .expect("e")
            .bytes(&[0u8; 31])
            .expect("e");
        e.u8(2).expect("e").bytes(&h32(3)).expect("e");
    });
    assert!(IdAccBody::decode(&b).is_err());
    assert!(IdAccBody::decode(&map(&[(1, 1), (2, 2)])).is_err());
}

#[test]
fn field_rules_are_enforced() {
    let mut r = refs_body(1);
    r.refs.swap(0, 1);
    assert!(RefsBody::decode(&r.encode()).is_err(), "unsorted refs");
    let mut r = refs_body(1);
    r.refs[0].name = "refs/rad/sigrefs".into();
    assert!(RefsBody::decode(&r.encode()).is_err(), "refs/rad rejected");
    let mut r = refs_body(1);
    r.refs[0].oid = vec![1; 32];
    assert!(
        RefsBody::decode(&r.encode()).is_err(),
        "oid length vs hash kind"
    );
    let mut r = refs_body(1);
    r.refs[0].name = "refs/heads/a..b".into();
    assert!(RefsBody::decode(&r.encode()).is_err(), "gix-validate");

    let mut id = root();
    id.name = "Arkiv".into();
    assert!(IdBody::decode(&id.encode()).is_err(), "uppercase name");
    let mut id = root();
    id.nonce = None;
    assert!(IdBody::decode(&id.encode()).is_err(), "root needs nonce");
    let mut id = root();
    id.threshold = 2;
    assert!(
        IdBody::decode(&id.encode()).is_err(),
        "threshold > delegates"
    );
    let mut id = root();
    id.delegates = vec![h32(2), h32(1)];
    assert!(IdBody::decode(&id.encode()).is_err(), "delegates ascending");
    let mut id = root();
    id.rev = 1;
    assert!(
        IdBody::decode(&id.encode()).is_err(),
        "rev 1 needs rid and parent"
    );

    let mut m = manifest_body(2);
    m.chunk_hashes.pop();
    assert!(
        ManifestBody::decode(&m.encode()).is_err(),
        "chunk hash count"
    );
    let mut m = manifest_body(2);
    m.pack_kind = 2;
    assert!(
        ManifestBody::decode(&m.encode()).is_err(),
        "thin packs reserved"
    );
    let mut m = manifest_body(2);
    m.pack_kind = 3;
    assert!(
        ManifestBody::decode(&m.encode()).is_ok(),
        "format v3: a brotli stream of a self-contained pack"
    );
    m.pack_kind = 4;
    assert!(ManifestBody::decode(&m.encode()).is_err(), "unknown kind");

    let b = bind_body(1, ZERO, addr(1), addr(2));
    assert!(BindBody::decode(&b.encode()).is_err(), "seq 1 needs prev");
    let a = anchor_body(0, h32(1), h32(1), h32(2), 0);
    assert!(AnchorBody::decode(&a.encode()).is_err(), "seq 0 prev zero");

    let mut p = peer_body(alice());
    p.pk.pop();
    assert!(PeerBody::decode(&p.encode()).is_err(), "pk length");
}

#[test]
fn identity_round_trips_with_and_without_program_pin() {
    let plain = root();
    let pinned = IdBody {
        program: Some(ProgramPin {
            engine: "rhai/1".into(),
            hash: h32(5),
        }),
        ..root()
    };
    assert_eq!(IdBody::decode(&plain.encode()).expect("plain"), plain);
    assert_eq!(IdBody::decode(&pinned.encode()).expect("pinned"), pinned);
    // No pin means no key 13: the encoding is a strict prefix-compatible extension.
    let (a, b) = (plain.encode(), pinned.encode());
    assert!(b.len() > a.len());
    assert_eq!(a[1..], b[1..a.len()], "fields 1..12 encode identically");
    let empty = IdBody {
        program: Some(ProgramPin {
            engine: String::new(),
            hash: h32(5),
        }),
        ..root()
    };
    assert!(empty.validate().is_err());
}
