//! Shared fixtures: deterministic keys, sample bodies, observed-entity builders.
#![allow(dead_code)]

use charter_core::*;
use charter_crypto::{MasterSeed, PublicKey, SecretKey};
use std::sync::OnceLock;

pub const RID: Hash32 = [0x11; 32];

pub struct Peer {
    pub sk: SecretKey,
    pub pk: PublicKey,
    pub nid: Nid,
}

fn mk_peer(b: u8) -> Peer {
    let sk = MasterSeed::from_bytes([b; 32]).signing_key();
    let pk = sk.public_key();
    let nid = pk.nid();
    Peer { sk, pk, nid }
}

/// Peers sorted by nid so delegate lists are ascending.
pub fn peers() -> &'static [Peer] {
    static P: OnceLock<Vec<Peer>> = OnceLock::new();
    P.get_or_init(|| {
        let mut v: Vec<Peer> = (1u8..=5).map(mk_peer).collect();
        v.sort_by_key(|p| p.nid);
        v
    })
}

pub fn alice() -> &'static Peer {
    &peers()[0]
}

pub fn addr(b: u8) -> Address {
    [b; 20]
}

pub fn h32(b: u8) -> Hash32 {
    [b; 32]
}

/// Signs `body`, returns the envelope bytes and the content hash.
pub fn sign<B: Body>(p: &Peer, body: &B) -> (Vec<u8>, Hash32) {
    let env = Envelope::sign(&p.sk, B::KIND, body.encode()).expect("sign");
    let bytes = env.encode();
    let h = content_hash(B::KIND, &bytes);
    (bytes, h)
}

/// An observed entity carrying exactly the attributes the schema derives.
pub fn observe<B: Body>(
    p: &Peer,
    body: &B,
    creator: Address,
    block: u64,
    tx: u32,
) -> (Observed, Hash32) {
    let (bytes, h) = sign(p, body);
    (observe_bytes::<B>(&bytes, body, creator, block, tx), h)
}

pub fn observe_bytes<B: Body>(
    env_bytes: &[u8],
    body: &B,
    creator: Address,
    block: u64,
    tx: u32,
) -> Observed {
    let env = Envelope::decode(env_bytes).expect("decode");
    let attributes = entity_attributes(&env, env_bytes, body);
    let mut key = [0u8; 32];
    key[..8].copy_from_slice(&block.to_be_bytes());
    key[8..12].copy_from_slice(&tx.to_be_bytes());
    key[12..20].copy_from_slice(&creator[..8]);
    key[20..].copy_from_slice(&content_hash(B::KIND, env_bytes)[..12]);
    Observed {
        key,
        creator,
        block,
        tx,
        expires_at: block + 1_000_000,
        updated_at: block,
        attributes,
    }
}

pub fn peer_body(p: &Peer) -> PeerBody {
    PeerBody {
        alg: p.pk.alg().id(),
        pk: p.pk.as_bytes().to_vec(),
        alias: "alice".into(),
    }
}

pub fn bind_body(seq: u64, prev: Hash32, control: Address, bulk: Address) -> BindBody {
    BindBody {
        rid: RID,
        seq,
        prev,
        control,
        bulk,
    }
}

pub fn root_body(delegates: Vec<Nid>, threshold: u64) -> IdBody {
    IdBody {
        rid: None,
        rev: 0,
        parent: ZERO,
        name: "arkiv-git".into(), // test fixture data, kept byte-identical across the rename
        description: "git on arkiv".into(),
        default_branch: "main".into(),
        delegates,
        threshold,
        visibility: 0,
        lang: Some("rust".into()),
        license: Some("MIT".into()),
        nonce: Some([9u8; 16]),
        program: None,
    }
}

pub fn refs_body(oid_byte: u8) -> RefsBody {
    RefsBody {
        rid: RID,
        object_hash: 1,
        refs: vec![
            RefEntry {
                name: "refs/heads/main".into(),
                oid: vec![oid_byte; 20],
            },
            RefEntry {
                name: "refs/tags/v1".into(),
                oid: vec![oid_byte ^ 0xff; 20],
            },
        ],
    }
}

pub fn anchor_body(seq: u64, prev: Hash32, refs: Hash32, id_head: Hash32, at: u64) -> AnchorBody {
    AnchorBody {
        rid: RID,
        seq,
        prev,
        refs,
        manifests: vec![],
        id_head,
        at,
    }
}

pub fn manifest_body(chunks: usize) -> ManifestBody {
    ManifestBody {
        rid: RID,
        pack_id: h32(0x33),
        trailer: vec![0x44; 20],
        size: (chunks as u64) * CHUNK_SIZE as u64,
        chunk_size: CHUNK_SIZE as u64,
        chunk_hashes: vec![0x55; 32 * chunks],
        requires: vec![],
        pack_kind: 1,
    }
}

pub mod history;
pub mod v040;
