# P1 design: git-remote-arkiv, identity, signed refs, anchors, search

Status: design spec, input to TDD implementation.
Date: 2026-11-06.
Inputs: `docs/p0/README.md`, `docs/p0/arkiv-limits.md`, `docs/p0/heartwood-reuse.md`, `docs/p0/openmls-xwing.md`, `spike/arkiv-measure/`, Arkiv node source at `2a3526dd861bf01c284d232d8ea4f880e24fd526` (v1.0.1), heartwood at `e6fe5207`.
Labels as in P0: MEASURED (observed on Rabdi), SOURCE-VERIFIED (read in code), DERIVED (computed), ASSUMED (must be checked by a P1 test).

Since v0.4.0 git data moves in format v2 (`docs/p5/head-model.md`): per-device mutable heads, a kept-alive snapshot and short-lived pieces read back from block history.
This spec stays the reference for identity, binds, completeness, collaboration ops and the v1 anchors that v0.4.0 still reads; sections 3.6, 4.2 and 4.3 describe format v1, which v0.4.0 no longer writes.

## 0. Scope, gate and fixed decisions

P1 delivers a `git-remote-arkiv` helper, a `chart` CLI and a Rust client library.
P1 covers identity documents, per-peer signed refs, on-chain ref anchors, pack storage, lifetime keeping and search, for public repos only.
P1 gate: (a) a 100 MiB repo pushed to Rabdi clones end to end into an empty machine state and passes `git fsck --full`; (b) a replayed old ref (old anchor and refs bytes re-posted, or the newest anchor withheld or expired) is rejected.

Fixed decisions this spec builds on (not reopened):

- Arkiv is trusted only for ordering, availability and timestamps; clients verify every signature.
- All data lives on Arkiv, packs included, in 64 KiB chunks.
- Signatures are ML-DSA from day one; this spec treats them as an abstract `Sig` / `PublicKey` with sizes as parameters, budgeted at pk 1,952 B and sig 3,309 B (ML-DSA-65), with ML-DSA-87 (2,592 / 4,627 B) checked as the worst case.
- P2 adds encryption and keyed-hash IDs only at the Arkiv boundary, so every format here carries a version, a ciphertext-capable payload, and identifier attributes typed so they can later hold an HMAC.
- Every public-repo entity is created with `permissionlessExtension`.
- History is append-only entities: no `atBlock`, no `NOT`, no `!=` in queries.
- Rust reaches Arkiv through `arkiv-bindings` (git dependency pinned by commit) plus alloy.
- EVM-first designs are out.

## 1. What P0 did not know, found while writing this spec

These change the design relative to `heartwood-reuse.md` section 8 and are load-bearing.

1. One 64 KiB chunk per transaction, not several.
   The public RPC accepts a raw signed tx of at most 131,030 B (MEASURED, `arkiv-limits.md` section 1), and two 64 KiB payloads alone are 131,072 B.
   The 16.78M gas cap is therefore never the binding limit for chunk writes; the tx byte cap is.
2. `eth_getLogs` over block ranges does not work on Rabdi today (MEASURED 2026-11-06).
   A 1,000-block range returns `-32603 golemdb segment_range_of(transactions): NotImplemented`; single recent blocks and some windows under about 500 blocks near head work, and some single older blocks fail with the same error.
   `eth_getBlockReceipts` is not available.
   So no rule in P1 may depend on scanning logs, and the anchor ordering key comes from query order (section 3.2), not from log positions.
3. `entityNonce(address)` works through `eth_call` to `0x4400000000000000000000000000000000000044` (selector `0x36917bfd`; MEASURED: returned 314 for a sample address whose tx count was 315).
   It counts every entity an address ever created, live or not, which gives an exact completeness check without logs or history (section 3.3).
4. Anonymous `arkiv_query` is budget-limited: a single query in this session was refused with `ANON_COST_LIMITED` (MEASURED).
   A 100 MiB clone needs about 110 queries, so P1 requires an access key (`CHARTER_ACCESS_KEY`) for real use and for the gate.
5. EIP-1559 base fee: blocks above half of the 30M gas limit raise the base fee by up to 12.5 percent per block (`arkiv-limits.md` section 3).
   One pusher filling every block for 146 blocks would multiply the fee by about 1.125^146, so the writer paces itself at 40 percent of block gas (section 4.6).
6. heartwood formats are Ed25519- and SHA-1-bound (`NodeId`, sigrefs `signature` blob, `RepoId`, identity `Doc::sign`).
   With ML-DSA they cannot be reused byte for byte, so P1 ports their semantics (canonical refs payload, root binding, parent pointer, majority identity acceptance, threshold canonical refs) into its own formats and depends on no heartwood crate (open question Q2).
7. `permissionlessExtension` on anchors (decided) defeats P0's "monotone expiry" defence, because anyone can keep an old anchor alive after the head expires.
   P1 replaces that defence with the creator rule plus the `entityNonce` completeness check (section 3.3).
8. The node workspace pins `rust-version = 1.97` and alloy `1.6` (primitives, sol-types) and `2.5` (provider, contract); the Charter workspace must match to avoid duplicate alloy types.

## 2. Entities

### 2.1 Common conventions

Encoding: canonical CBOR, RFC 8949 section 4.2.1 core deterministic encoding (definite lengths, shortest integer and length forms, map keys sorted bytewise, which for small unsigned integer keys means ascending), integer map keys, no floats, no tags.
Decoders must reject non-canonical input by re-encoding the decoded value and comparing bytes, and must reject unknown map keys and trailing bytes.
Why CBOR over a hand-rolled length-prefixed format:

- Signed bytes must be unambiguous, and deterministic CBOR gives that with a published spec and a re-encode check, while a custom TLV needs its own spec and fuzzing.
- Optional and new fields (P2 key epochs, P3 COB fields) slot in as new integer keys under a new format version without redesigning layouts.
- Web and script clients get mature decoders (`cborg` in JS, `cbor2` in Python) with no custom parser.
- Large hash lists ride as one `bstr` (32 x n bytes), so CBOR adds 3 bytes of framing, not 1 byte per item.
- Rust crate: `minicbor` with `derive` (explicit `#[n(k)]` indices, emits maps in declaration order, so declaring keys ascending yields canonical output); the round-trip test in section 7.1 enforces canonicality rather than trusting the crate.

Hashes: SHA-256 everywhere.

- Raw byte blobs (a pack, a chunk) are hashed plainly: `pack_id = SHA-256(pack bytes)`, `chunk_hash = SHA-256(chunk bytes)`.
- Structured objects are hashed with a tag: `H(tag, x) = SHA-256("arkiv-git/v1/" || tag || 0x00 || x)`.
- The content hash of a signed entity is `h = H(kind_name, envelope_bytes)`, where `kind_name` is the string in the `ag.kind` attribute.

Identifiers:

- Peer id: `nid = H("nid", u16be(alg) || pk)`, 32 bytes, where `alg` is the signature algorithm id from the registry that `docs/p0/pq-signatures.md` will define.
- Repo id: `rid = h` of the root identity revision, that is `H("id", root envelope bytes)`, 32 bytes.
- Text form for both: `z` + base58btc of the 32 bytes (Radicle style); the CLI also accepts 64 lowercase hex.
- Remote URLs: `arkiv://<rid>` (canonical refs) and `arkiv://<rid>/<nid>` (one peer's refs).

Signed envelope (every kind except `chunk`):

```cddl
Envelope = [
  v:      1,            ; envelope format version
  kind:   uint,         ; kind code, table 2.2
  enc:    uint,         ; 0 = body is plaintext canonical CBOR; 1.. reserved for P2 ciphertext
  signer: bstr .size 32,; nid
  body:   bstr,         ; canonical CBOR of the kind's Body when enc = 0
  sig:    bstr,         ; Sig over SigMsg
]
SigMsg = "arkiv-git/v1/sig" || 0x00 || u8(kind) || u8(enc) || signer || body
```

- Domain separation: the fixed prefix separates Charter from every other use of the key, and `kind` inside `SigMsg` separates kinds, so an anchor signature never verifies as a refs signature.
- If the chosen scheme takes a FIPS 204 context string, pass `ctx = "arkiv-git/v1"` in addition; the message stays as above.
- The signature covers `body` as stored, so in P2 (ciphertext body) anyone can still check authorship and spam without keys; whether P2 also signs the plaintext is a P2 decision.
- Envelope overhead with ML-DSA-65: 1 + 1 + 1 + 1 + 34 + 3 + 3 + 3,309 = 3,353 B plus the body.
- Cap: every `ag.body` value is at most 65,536 B, so the maximum body is `65,536 - sig_len - 44` B (62,183 B with ML-DSA-65, 60,865 B with ML-DSA-87).

Arkiv-side attributes common to every kind:

| Name | Type | Indexed | Value |
| --- | --- | --- | --- |
| `ag.kind` | `str` | yes | kind name, table 2.2 |
| `ag.v` | `u64` | no | 1 (entity layout version) |
| `ag.body` | `bytes` | no (never indexable) | envelope bytes, or raw chunk bytes |

Rules for attributes:

- Every identifier attribute (`ag.rid`, `ag.nid`, `ag.h`, `ag.pack`, `ag.parent`, `ag.rev`) is `bytes32`, so P2 can replace the value with `HMAC(k_repo, value)` without changing type or index.
- Attributes are hints for queries only; after decoding, a client checks that each attribute equals the value derived from the signed body or envelope, and ignores the entity otherwise.
- Every entity is created with `creationFlags = 3` (bit0 `readonly`, bit1 `permissionlessExtension`), `salt` = 16 random bytes, `expiresAt = 0`, and `minLifetime` from table 2.3.
- Charter never predicts entity keys and never patches, transfers or deletes an entity; every cross-reference is by content hash and found by query.
- An entity from a tracked control address (section 3.1) with `ag.v` or envelope `v` or `enc` the client does not know fails closed with `E_UNSUPPORTED_VERSION`, because silently skipping a newer-format anchor would roll the client back; the same entity from any other creator is ignored.

### 2.2 Kinds

| Code | `ag.kind` | Signed by | Created by (EVM) | Purpose |
| --- | --- | --- | --- | --- |
| 1 | `peer` | the peer itself (proof of possession) | the peer's control address for the repo | ML-DSA public key of a peer |
| 2 | `bind` | the peer | control address (section 3.1) | binds a peer to its per-repo EVM addresses |
| 3 | `id` | proposer | proposer's control address | identity revision (root has rev 0) |
| 4 | `idacc` | accepting delegate | delegate's control address | acceptance of an identity revision |
| 5 | `refs` | the peer | the peer's bulk address | signed refs of one peer |
| 6 | `anchor` | the peer | the peer's control address | chain link fixing one peer's refs head |
| 7 | `manifest` | the pusher | pusher's bulk address | describes one pack and its chunk hashes |
| 8 | `chunk` | nobody (bound by manifest hash) | pusher's bulk address | 64 KiB of pack bytes |
| 9 | `cob` | author | author's control address | reserved for P3, no code in P1 |

#### `peer`

```cddl
PeerBody = {
  1: uint,                 ; alg id
  2: bstr .size (1..4096), ; pk
  3: tstr .size (0..64),   ; alias, display only, never trusted
}
```

- Envelope `signer` must equal `H("nid", u16be(alg) || pk)` and `sig` must verify under `pk`.
- Extra attributes: `ag.nid` (`bytes32`, indexed), `ag.h` (`bytes32`, indexed).
- Size: body about 2,025 B, entity about 5,380 B.
- A peer document is global (no `ag.rid`), but each repo's control address writes its own copy on that peer's first write to the repo, so the completeness rule (section 3.3) covers it; copies are byte-identical and duplicates are harmless.

#### `bind`

```cddl
BindBody = {
  1: bstr .size 32,  ; rid
  2: uint,           ; seq, 0, 1, 2, ...
  3: bstr .size 32,  ; prev = h of bind seq-1, or 32 zero bytes at seq 0
  4: bstr .size 20,  ; control address: creates peer, bind, id, idacc, anchor
  5: bstr .size 20,  ; bulk address: creates refs, manifest, chunk
}
```

- Extra attributes: `ag.rid`, `ag.nid`, `ag.h` (`bytes32`, indexed), `ag.seq` (`u64`, indexed).
- Creator rule: bind seq 0 must be created by its own field 4; bind seq n+1 must be created by the control address of bind seq n.
- Size: body about 120 B, entity about 3,470 B.

#### `id` (identity revision)

```cddl
IdBody = {
  ? 1: bstr .size 32,          ; rid, absent iff rev = 0
  2: uint,                     ; rev
  3: bstr .size 32,            ; parent = h of revision rev-1, zero iff rev = 0
  4: tstr,                     ; name, 1..64 B, [a-z0-9._-], lowercase
  5: tstr .size (0..256),      ; description
  6: tstr .size (1..100),      ; default branch name, valid git ref component
  7: [1*16 bstr .size 32],     ; delegates (nids), strictly ascending
  8: uint,                     ; threshold for canonical refs, 1..len(delegates)
  9: uint,                     ; visibility: 0 public; 1 private (reserved, P2)
  ? 10: tstr .size (1..32),    ; primary language, lowercase (search)
  ? 11: tstr .size (1..64),    ; licence, SPDX id (search)
  ? 12: bstr .size 16,         ; nonce, required iff rev = 0, random
}
```

- Extra attributes: `ag.rid`, `ag.h`, `ag.parent`, `ag.nid` (proposer) as `bytes32` indexed; `ag.seq` (`u64`, indexed, equals rev); for public repos also `ag.name`, `ag.lang`, `ag.lic` (`str`, indexed, equal to fields 4, 10, 11).
- For the root, `ag.rid = ag.h = rid`.
- Size: body at most about 1,170 B (16 delegates), entity at most about 4,520 B.
- Delegates are capped at 16 so a revision plus its accepts stays small; more than 16 is open question Q9.

#### `idacc`

```cddl
IdAccBody = {
  1: bstr .size 32,  ; rid
  2: bstr .size 32,  ; rev hash being accepted
}
```

- Extra attributes: `ag.rid`, `ag.nid`, `ag.rev` (`bytes32`, indexed).
- Size: entity about 3,430 B.

#### `refs` (signed refs)

```cddl
RefsBody = {
  1: bstr .size 32,                 ; rid
  2: uint,                          ; object hash: 1 = SHA-1, 2 = SHA-256
  3: [* [tstr, bstr]],              ; (ref name, oid), strictly ascending by name bytes
}
```

- Ref names must pass `gix-validate` reference-name validation and start with `refs/heads/`, `refs/tags/` or `refs/notes/`; `refs/rad/*`, `refs/arkiv/*` and `HEAD` are rejected.
- OID length must match field 2 (20 or 32 B).
- The signer and rid in the envelope and body replace heartwood's `refs/rad/root`; the anchor chain replaces `refs/rad/sigrefs-parent`.
- Extra attributes: `ag.rid`, `ag.nid`, `ag.h` (`bytes32`, indexed).
- Size: at most 62,183 B of body, about 1,100 refs at 30-byte names; a larger set fails the push with `E_REFS_TOO_LARGE` (open question Q8).

#### `anchor`

```cddl
AnchorBody = {
  1: bstr .size 32,          ; rid
  2: uint,                   ; seq, 0, 1, 2, ...
  3: bstr .size 32,          ; prev = h of anchor seq-1, zero iff seq = 0
  4: bstr .size 32,          ; refs = h of this push's refs entity
  5: [* bstr .size 32],      ; manifests added by this push (h values), may be empty
  6: bstr .size 32,          ; id_head = h of the identity revision the writer verified
  7: uint,                   ; at = head block number seen when signing (search recency)
}
```

- Extra attributes: `ag.rid`, `ag.nid`, `ag.h` (`bytes32`, indexed); `ag.seq`, `ag.at` (`u64`, indexed).
- Size: body about 197 + 34 per manifest B, entity about 3,590 B for one manifest.

#### `manifest`

```cddl
ManifestBody = {
  1: bstr .size 32,          ; rid
  2: bstr .size 32,          ; pack_id = SHA-256(stored pack bytes)
  3: bstr,                   ; git pack trailer (20 B SHA-1 or 32 B SHA-256)
  4: uint,                   ; size in bytes, 1..104,857,600
  5: uint,                   ; chunk size, 65,536 in P1
  6: bstr,                   ; chunk hashes, 32 x ceil(size / chunk size) bytes
  7: [* [bstr .size 32, bstr .size 32]], ; requires: (nid, anchor h) whose packs must be present first
  8: uint,                   ; pack kind: 1 = self-contained git pack; 2 = thin (reserved)
}
```

- Chunk hashes are over stored bytes, so in P2 (ciphertext chunks) a sponsor or seed can verify availability without keys.
- Extra attributes: `ag.rid`, `ag.nid`, `ag.h`, `ag.pack` (`bytes32`, indexed).
- Size: a full 100 MiB pack has 1,600 chunks, so field 6 is 51,200 B and the entity is about 54,700 B, under the 64 KiB body cap even with ML-DSA-87.

#### `chunk`

- No envelope: `ag.body` is the raw stored bytes (plain pack bytes in P1, ciphertext in P2), at most 65,536 B, the last chunk of a pack shorter.
- Extra attributes: `ag.rid`, `ag.pack` (`bytes32`, indexed), `ag.i` (`u64`, indexed, chunk index from 0).
- Integrity: `SHA-256(ag.body)` must equal entry `ag.i` of the manifest's field 6.
- Size: 65,536 B payload, about 66,900 B of calldata with attributes and ABI framing, one per tx.

#### `cob` (P3 placeholder)

- Reserved: kind code 9, `ag.kind = 'cob'`, attributes `ag.rid`, `ag.nid`, `ag.h`, `ag.cob` (`bytes32`, object id), `ag.type` (`str`, e.g. `xyz.radicle.issue`).
- Reserved body shape: `{1: rid, 2: cob type, 3: object id, 4: [parent h...], 5: [op bstr...]}`, signed envelope, created by the author's control address.
- P1 writes and reads no `cob` entities, but the format crate defines the kind code so P3 does not renumber.

### 2.3 Lifetime per kind

| Kind | Create with `minLifetime` | Kept alive by keepalive |
| --- | --- | --- |
| `peer`, `bind`, `id`, `idacc`, `anchor` (control plane) | 7,884,000 blocks (182.5 days, the maximum) | always, every one, forever (required by the completeness rule) |
| `refs` | 1,296,000 blocks (30 days) | only the current head refs of each peer; older ones may expire |
| `manifest`, `chunk` | 1,296,000 blocks (30 days) | every manifest named by any anchor of a verified chain, and all its chunks |

Why 30 days for data: storage gas is linear in lifetime, so a long first lifetime buys nothing, and a 64 KiB chunk at 30 days costs 3.92M gas against 10.1M at the maximum, which matters for push pacing (section 4.6).
Why the maximum for the control plane: the entities are small (about 0.9 to 1.1M gas each at the maximum) and must never lapse.
A test-only `--lifetime` override exists for the gate run (1 day) and the expiry test (100 blocks).

### 2.4 Size and gas budget

Gas figures are DERIVED from the MEASURED points in `arkiv-limits.md` (4 KiB at 1 day 228,644; 64 KiB at 1 day 2,707,122, at 30 days 3,890,917, at the maximum 10,113,997; 40.8 gas per calldata byte; 14e-6 gas per unit-block), plus about 800 units for the extra indexed attributes on a chunk.

| Tx | Sender | Ops | Calldata | Gas |
| --- | --- | --- | --- | --- |
| First write to a repo: `peer` + `bind` (+ root `id` for the founder) | control | 2 or 3 | about 13 KB | about 3.2M at maximum lifetime |
| Chunk | bulk | 1 | about 66.9 KB | 3.92M at 30 days; 2.72M at 1 day |
| Manifest (full 100 MiB pack) | bulk | 1 | about 55.6 KB | about 3.3M at 30 days |
| Refs (typical, under 4 KB) | bulk | 1 | under 5 KB | about 0.25M at 30 days |
| Anchor | control | 1 | about 4.4 KB | about 0.9M at maximum lifetime |
| Identity revision or accept | control | 1 | about 5 KB | about 1M at maximum lifetime |
| Keepalive batch | any | up to 100 | under 15 KB | at most 15M (section 5) |

Every tx stays under the measured raw-tx cap of 131,030 B and the 16,777,216 gas cap; the writer checks both locally before sending (`E_TX_TOO_LARGE`, `E_GAS_CAP` are bugs, not user errors).

## 3. Verification: binds, identity, anchors

### 3.1 Per-repo EVM addresses and the creator rule

Each peer derives three secp256k1 keys from its 32-byte master seed with HKDF-SHA256 (`salt = "arkiv-git/v1/evm"`, `info = role || rid || u32be(index)`, reduced into the curve order, retried with the next counter on zero):

- `wallet` (no rid): holds the user's funds and tops up the other two with plain transfers.
- `control(rid, i)`: creates only control-plane entities of that repo, all of which are kept alive forever.
- `bulk(rid, i)`: creates refs, manifests and chunks of that repo.

The ML-DSA key pair is derived from the same seed (ML-DSA key generation takes a 32-byte seed), so one secret is protected (open question Q3).
Per-repo addresses make the completeness check exact (section 3.3) and give P2 unlinkability across repos for free.

Creator rule (CR), applied to every signed entity of kinds 2 to 7:

- Let `B_0..B_m` be the signer's valid bind chain for this rid (section 3.4).
- Control-plane kinds (`bind` seq > 0, `peer` copies, `id`, `idacc`, `anchor`) must have `$creator = B_i.control` for some `i`, and if `i < m` then `$createdAt(e) <= $createdAt(B_{i+1})`.
- `refs` and `manifest` must have `$creator = B_i.bulk` for some `i`, with the same time rule; chunks are fetched only with `$creator` in the bulk set.
- Entities failing CR are ignored, never counted as evidence.

CR is what makes a third-party re-post of old bytes inert: only the peer's own control key can place an anchor where clients will look, and only an entity that clients will look at can roll them back.

### 3.2 Ordering

The order of two entities is Arkiv creation order: by creating transaction (block, then tx index), oldest first, then by entity key within one transaction (api-spec section 5, "Result ordering", SOURCE-VERIFIED).
P1 reads this order from query results, because `eth_getLogs` ranges fail on Rabdi (section 1, item 2).

- Within one query with no `ORDER BY`, result order is creation order.
- With `ORDER BY ag.seq:u64 ASC`, ties come in creation order (api-spec section 4), so for one seq the candidates arrive first-created first.
- Tooling never puts two anchors, two binds or two identity revisions of one signer in one tx, so the within-tx key order never decides anything that matters; if a tx does contain two, the lower entity key comes first, exactly as the query returns them.
- The brief's (block, tx index, log index) key is the same total order for entities in different transactions; when receipts or logs become reliable the client may cross-check it, but no rule depends on it.
- Single sequencer and `--txpool.enforce-tracked-nonce` mean no reorgs (SOURCE-VERIFIED `kurtosis/arkiv-chain.yaml:20-29`), so one block of inclusion is final (ASSUMED for Rabdi; checked in the funded run).

### 3.3 Completeness check (fail closed on expiry or deletion)

For each control address `C` in a peer's bind chain:

1. `n1 = entityNonce(C)` via `eth_call` at `latest`.
2. `c = count` of the query `$creator = addr(C)` (`select: {count: true}`).
3. `n2 = entityNonce(C)`.
4. If `n1 != n2`, retry up to 3 times, then `E_INCONSISTENT_VIEW`.
5. If `c < n1`, some control-plane entity this peer created for this repo is no longer live (expired, or deleted by the owner key): fail closed with `E_CONTROL_NOT_LIVE { nid, address: C, created: n1, live: c }`.
6. If `c > n1`, the node is inconsistent: `E_INCONSISTENT_VIEW`.

Why this is enough:

- Tooling creates nothing from `C` that is not kept alive forever, so in an honest, maintained repo `c = n1` always.
- Expiry of the head anchor (keepalive lapsed while someone keeps older anchors alive via `permissionlessExtension`) and deletion by the owner key both lower `c` and are caught.
- A newer anchor withheld by a lying watcher is caught only if the same watcher reports `entityNonce` honestly; a watcher that lies about both is outside the trust model (Arkiv trusted for availability), and the local high-water mark still protects every client that saw the newer head.
- Cost: 3 RPC calls per control address per fetch.

### 3.4 Bind chain

For `(rid, nid)`:

1. Query `ag.kind = str('bind') AND ag.rid = bytes32(R) AND ag.nid = bytes32(N) ORDER BY ag.seq:u64 ASC`, keyset-paged (section 4.8).
2. Keep entities whose envelope verifies under `pk(N)`, whose attributes match the body, and whose creator satisfies the bind creator rule (section 2.2).
3. `B_0` is the first with seq 0 and prev zero; `B_{k+1}` is the first with seq k+1 and prev `h(B_k)`.
4. Any valid bind with seq above the chain head: `E_CHAIN_GAP { kind: bind }`.
5. A second valid bind with a taken seq and different bytes: equivocation warning (section 3.7), chain unchanged.

The control set is `{B_i.control}`, the bulk set `{B_i.bulk}`; the completeness check (3.3) runs over the control set.

### 3.5 Identity chain and key rotation

Inputs: one query `ag.rid = bytes32(R) AND (ag.kind = str('id') OR ag.kind = str('idacc'))` in creation order, offset-paged with the count consistency check (section 4.8), plus the root found by `ag.kind = str('id') AND ag.h = bytes32(R)`.
Each entity must verify (signature under the signer's peer document, attribute match, CR against the signer's bind chain, body field rules in section 2.2).

Definitions:

- `maj(D) = floor(len(D) / 2) + 1` for a delegate list `D` (heartwood's identity quorum, `doc.rs:996-998`).
- The acceptors of a revision `X` are the distinct signers among X's own envelope and all valid `idacc` with `rev = h(X)`.
- `Q(X, D)` is the position of the acceptance that first brings `|acceptors(X) ∩ D|` to `maj(D)`, or none.

Evaluation:

1. Root: the first `id` with rev 0, parent zero and `h = R`, whose signer is in its own delegates, and with `Q(root, root.delegates)` defined.
   If the root is not live, fetch fails with `E_ID_NOT_LIVE`.
2. Given head `R_k` effective at position `P_k`, candidates are valid `id` entities with `rev = k + 1` and `parent = h(R_k)`.
   For each candidate `X`, its effective position is `max(P_k, Q(X, R_k.delegates))`.
   `R_{k+1}` is the candidate with the smallest effective position; all other candidates with the same parent are dead forever (no fork can revive).
3. Repeat until no candidate reaches quorum; the last revision is the identity head.
4. Any proposer may publish a revision, but only acceptances from the parent's delegates count.
   A delegate with no bind for the repo gets `peer` and `bind_0` written by `chart id accept` in the same control tx as its `idacc`.
5. High-water mark: `head.rev >= hwm.id.rev` and the chain contains `hwm.id.hash`, else `E_STALE_IDENTITY`.
6. The completeness check (3.3) for every delegate of the head covers identity entities too, so an expired newer revision fails closed.

Key rotation:

- ML-DSA key rotation of a delegate is an identity revision that replaces the old nid with the new one in field 7, accepted by `maj` of the parent's delegates, which includes the old key for a single-delegate repo.
- The new key then writes its own `peer` and `bind` and starts its own anchor chain at seq 0; canonical refs move with the delegate set.
- A compromised delegate key is removed the same way by the remaining majority; in a single-delegate repo a compromised key is a lost repo, which the CLI states when `init` creates a single-delegate identity.
- EVM key rotation (control or bulk) is a new bind (seq + 1) created by the current control key, with no identity revision.

### 3.6 Anchor chain and acceptance

Format v1; a peer with a device delegation (`ag.v = 2` anchor, head-model 4.1) reads its refs from its device heads instead once one has landed.

An anchor entity `e` is valid for `(R, N)` if and only if all of these hold:

1. `ag.kind = 'anchor'`, `ag.v = 1`, `ag.body` decodes as a canonical envelope with `v = 1`, `kind = 6`, `enc = 0`, `signer = N`.
2. `sig` verifies under `pk(N)` from a valid peer document.
3. The body decodes canonically, `body.rid = R`, and `ag.rid`, `ag.nid`, `ag.seq`, `ag.at`, `ag.h` equal the values derived from the body and envelope.
4. CR holds against N's bind chain.
5. `body.id_head` is a revision in the verified identity chain of R.
6. `body.at <= $createdAt(e)`.

Chain:

1. Query `ag.kind = str('anchor') AND ag.rid = bytes32(R) AND ag.nid = bytes32(N) AND ($creator = addr(C0) OR ...) ORDER BY ag.seq:u64 ASC`, keyset-paged by seq.
2. `A_0` is the first valid anchor with seq 0 and prev zero.
3. `A_{k+1}` is the first valid anchor with seq `k + 1` and `prev = h(A_k)`.
4. The head is the last anchor reached.

Accept head `A_s` for N only if all of these hold, else fail as listed:

| Check | Failure |
| --- | --- |
| Completeness (3.3) holds for every control address of N | `E_CONTROL_NOT_LIVE` |
| No valid anchor has seq > s (a live but unreachable higher anchor means a missing link) | `E_CHAIN_GAP` |
| If a high-water mark exists: `s >= hwm.seq` and `h(A_hwm.seq) = hwm.hash` | `E_STALE_HEAD`, `E_HISTORY_REWRITTEN` |
| The refs entity with `h = A_s.refs` is live, valid, signed by N, same rid | `E_NOT_LIVE { refs }`, `E_BAD_REFS` |
| Every manifest named by `A_0..A_s` is live and valid, or already applied locally, and its `requires` are satisfied | `E_NOT_LIVE { manifest }`, `E_MISSING_DEPENDENCY` |
| Every chunk of every pack not yet applied is live with the right hash | `E_NOT_LIVE { chunk, pack, index }` |
| Every assembled pack hashes to `pack_id`, has the declared trailer, and `git index-pack --strict` accepts it | `E_BAD_PACK` |
| Every OID in the head refs is present and connected (`git rev-list --objects --missing=error <new tips> --not <previously verified tips>`) | `E_MISSING_OBJECTS` |

Why a replayed old ref fails:

- A copy of old anchor bytes created by anyone other than N's control key fails CR and is ignored.
- A copy re-posted by N's own control key (a stolen EVM key without the ML-DSA key) has a taken seq and identical bytes, so it is a duplicate that cannot displace the first-created `A_j`, and its refs are not the head's refs.
- Creating `seq = s + 1` needs N's ML-DSA key.
- Withholding or expiring newer anchors trips the completeness check for fresh clones and the high-water mark for every client that saw them.

Incremental fetch: with a high-water mark at seq `w`, the client queries only `ag.seq >= u64(w)`, checks `h(A_w)` against the mark, and extends the chain from there; the completeness check runs on every fetch regardless.

### 3.7 Equivocation

- Two valid anchors (or binds, or identity revisions by one proposer) with the same seq and different bytes, or a valid anchor with seq `k + 1` whose prev is not `h(A_k)`, are evidence that N signed conflicting histories.
- The chain stays first-created-wins, which every client computes identically, so equivocation cannot split views; it can only mean the loser's push is ignored.
- The client prints `warning: E_EQUIVOCATION peer <nid> seq <k>: entities <key1>, <key2>` on every fetch and lists it in `chart status`; it does not exclude the peer, because exclusion would let one bad push of a single-delegate repo make the repo unreadable.
- The expected remedy for an equivocating delegate is an identity revision removing it; P1 publishes no on-chain evidence entity.

### 3.8 Canonical refs

- Inputs: the identity head (delegates `D`, threshold `t`, default branch `b`) and each delegate's accepted head refs.
- For `refs/heads/<b>` and every `refs/tags/*` name: the canonical OID is the OID held at exactly that name by at least `t` delegates, choosing the one with the most votes; a tie or no OID reaching `t` omits the ref and prints `warning: no quorum for <ref>`.
- Exact-match quorum is simpler than heartwood's merge-base quorum; single-delegate repos (the gate case) behave identically (open question Q7).
- A delegate whose chain fails any check aborts the fetch (heartwood does the same for delegates' special refs); a failing non-delegate peer is skipped with a warning and keeps its old local refs.

### 3.9 Local state and the high-water mark

Layout under `$CHARTER_HOME` (default `~/.charter`):

```text
keys/seed                       32 B master seed, mode 0600 (open question Q4)
repos/<rid-hex>/git/            bare repo, heartwood-style namespaces:
                                  refs/namespaces/<nid-hex>/refs/...   (verified, anchored only)
                                  refs/arkiv/staging/<nid-hex>/...     (local, not yet anchored)
repos/<rid-hex>/state.json      high-water marks, applied pack ids, equivocation notes
repos/<rid-hex>/outgoing/       pending.json and pack files of an unfinished push
repos/<rid-hex>/lock            flock held for the whole of a push, fetch or keepalive
```

`state.json` schema (JSON for debuggability, written by temp file, fsync, rename):

```json
{
  "v": 1,
  "chain_id": 7794095,
  "identity": { "rev": 3, "hash": "<hex32>" },
  "peers": { "<nid-hex>": { "seq": 41, "hash": "<hex32>", "block": 291210 } },
  "applied_packs": ["<pack_id hex>"],
  "equivocations": [{ "nid": "<hex>", "seq": 7, "keys": ["<key>", "<key>"] }]
}
```

- Order of a fetch commit: write the new high-water marks first, then update namespace refs in one `git update-ref --stdin` transaction with old-value checks; a crash between the two leaves marks ahead of refs, which is safe because marks only make the client stricter.
- Marks only increase; nothing in P1 lowers them (a reset is `rm -r repos/<rid-hex>`, which deliberately turns the client back into a fresh clone).
- `chain_id` is checked against `eth_chainId` on every run (`E_WRONG_CHAIN`).

## 4. git-remote-arkiv: push and fetch

### 4.1 Helper shape

Per `heartwood-reuse.md` section 4: capabilities `option`, `fetch`, `push` (`list` and `list for-push` are implicit), never `connect` or `stateless-connect`, because Arkiv cannot speak the pack protocol.
Two stages, as in `git-remote-rad`: the helper moves objects between the working repo and the local cache with stock git, and a sync layer moves verified data between the cache and Arkiv.
All git work uses the `git` CLI (`pack-objects`, `index-pack`, `fetch-pack`, `push` to a local path, `update-ref --stdin`, `rev-list`, `for-each-ref`); P1 links no libgit2 and no gix (heartwood shells out for pack transfer for the same speed reason, `CHANGELOG.md:838`).

Protocol exchange (stdin from git, stdout to git, diagnostics on stderr):

```text
> capabilities
< option
< fetch
< push
<
> option progress true          < ok
> option verbosity 1            < ok
> option dry-run true           < ok          (push builds and prices everything, sends nothing)
> list                          (runs the fetch sync, section 4.3)
< <oid> refs/heads/main
< <oid> refs/tags/v1.0
< @refs/heads/main HEAD
<
> fetch <oid> refs/heads/main   (batch, terminated by a blank line)
< (blank line after git fetch-pack from the cache succeeds)
> list for-push                 (runs the fetch sync, then lists the local signer's own namespace)
> push refs/heads/main:refs/heads/main   (batch, terminated by a blank line)
< ok refs/heads/main            (or: error refs/heads/main <E_CODE> <message>)
<
```

- `fetch` transfers objects with `git fetch-pack --stdin <cache>` after setting `uploadpack.allowAnySHA1InWant=true` in the cache, so canonical refs need not exist as real refs there.
- For `arkiv://<rid>/<nid>`, `list` prints that peer's verified refs instead of canonical refs; the sync must verify that peer even when it is not a delegate, and fails if it cannot.
- Pushes always write the local signer's namespace, whatever the URL names.

### 4.2 Push flow

Format v1, replaced in v0.4.0 by head-model section 7.

Preconditions: the repo lock is held, `list for-push` has just run the fetch sync (so the cache, the identity head and the signer's own anchor head are current and verified), and the signer has a valid bind for the repo or this push creates it.

1. Stage objects: `git push --no-verify <cache>/git <src>:refs/arkiv/staging/<nid>/<dst>` for each update; deletions stage nothing.
2. Resume check: if `outgoing/pending.json` exists, apply section 4.4 first.
3. Compute the new refs list: the signer's current verified head refs with the updates applied; build the `refs` envelope and its `h`; reject over the size cap with `E_REFS_TOO_LARGE` before any write.
4. Generate packs in the cache: `git pack-objects --revs --max-pack-size=100m --delta-base-offset <tmp>/pk` with stdin `<staging tips>`, `--not`, `<every ref tip under refs/namespaces/>`.
   - Packs are self-contained (no `--thin`, which only works with `--stdout`); deltas only against objects in the same pack.
   - `--max-pack-size=100m` caps each pack at 1,600 chunks, so one manifest always fits in one entity; a larger push yields several packs.
   - The excluded tips are verified refs of peers whose packs are on Arkiv, so `requires` lists `(nid, h of that peer's current head anchor)` for every peer whose tips were excluded and is not the signer.
   - If no object is new (ref deletion or a ref moved to an existing commit), no pack is produced.
5. Write `pending.json` (fsync) with: ref updates, staging refs, pack paths, pack ids, refs `h`, expected anchor seq `s + 1`, expected prev `h(A_s)`; move pack files to `outgoing/`.
6. Price and fund the push with rolling top-ups (this replaces the earlier rule that topped `control` and `bulk` up front to twice the predicted gas at `2 x base fee`, about 4.4 times the real spend).
   - One up-front check: the wallet plus what `control` and `bulk` already hold must cover the predicted spend (sum of `eth_estimateGas` for every entity, table 2.4, times `base fee + tip`) plus one window's reserve; otherwise fail with `E_INSUFFICIENT_FUNDS` before writing anything.
   - One window's reserve is `min(window, txs) x gas_limit x max_fee` for the largest predicted tx, with `window` the pipeline's in-flight cap (16), `txs` the push's tx count, `gas_limit = estimate x 1.1` and `max_fee = 2 x base fee + tip` (section 4.6).
   - During the push, before each window of sends, the pipeline re-reads the base fee (it can rise while we fill blocks) and, if the sender's balance minus the max cost of its txs in flight cannot cover the next tx, tops it up from `wallet` with one plain transfer covering the rest of the window (its free slots, capped at the jobs still queued) at that tx's size.
   - A top-up the wallet cannot cover stops the push mid-way with `E_INSUFFICIENT_FUNDS`; this is safe because partial uploads resume (section 4.4), so the next push sends only what is missing.
   - A sender therefore never holds more than the actual spend so far plus one window's max-fee reserve, instead of locking several times the cost of every push.
7. First write to this repo by this peer: control tx `[peer, bind_0]`.
8. Chunks: split each pack into 65,536 B chunks; send one `chunk` create per tx from `bulk` through the pipeline (section 4.6); wait for every receipt with `status = 1`.
9. Verify chunks landed: per pack, query chunk metadata (`select key, attributes {ag.i}`, `$creator = addr(bulk)`), check every index 0..n-1 is present; resend missing ones (at most 3 rounds, then `E_UPLOAD_INCOMPLETE`).
10. Manifests: one `manifest` create per tx from `bulk`; wait for receipts.
11. Refs: one `refs` create from `bulk`; wait for the receipt.
12. Anchor gate: re-read the signer's own anchor head from Arkiv; if its `h` is not the expected prev, stop with `E_CONCURRENT_PUSH` (pending state kept, nothing anchored).
13. Anchor: one `anchor` create from `control` with seq `s + 1`, prev `h(A_s)`, refs `h`, manifest `h` list, `id_head = h` of the verified identity head, `at = head block`; wait for the receipt.
14. Post-verify: run the anchor chain algorithm for the signer; if the head is not our anchor (we lost a race to an equivocating copy), report `E_LOST_RACE` and keep pending state.
15. Commit locally: high-water mark to the new anchor, `git update-ref --stdin` moving staging refs into `refs/namespaces/<nid>/` and deleting staging, add pack ids to `applied_packs` after `git index-pack` of our own packs into the cache, delete `pending.json` and pack files.
16. Print `ok <dst>` per update.

Data is always written before the anchor, so an anchor never points at data that did not land, and a crash before step 13 leaves only orphaned refs, manifests and chunks, which expire after 30 days unextended (no delete, since deletion refunds nothing).

### 4.3 Fetch and clone flow

Format v1 peers; v2 peers follow head-model section 5.5.

`git clone arkiv://<rid>` invokes the helper with an empty cache; `git fetch` reuses the cache.

1. Lock, check chain id, load `state.json` (empty for a fresh clone).
2. Identity: fetch the root and all `id` / `idacc` entities (section 3.5); collect signer nids; fetch their peer documents (`ag.kind = str('peer') AND ag.nid = bytes32(N)`) and bind chains (3.4); evaluate the identity head; check the identity mark.
3. Peers to sync: delegates of the identity head, plus peers named in `remote.<name>.arkivPeers` (git config, nid list), plus the transitive closure of `requires` of every manifest found; `--all-peers` (CLI) adds every nid with a bind for the rid.
4. Per peer: bind chain, completeness check over its control set, anchor chain, head acceptance checks that need no pack bytes (refs entity, manifests, chunk index presence via metadata queries).
   Before downloading, compute the minimum `$expiresAt` over all required entities and warn if it is less than 302,400 blocks (7 days) away.
5. Download packs not in `applied_packs`, in anchor order with `requires` first:
   - Query chunk bodies in pages: `ag.kind = str('chunk') AND ag.pack = bytes32(P) AND ($creator = addr(B0) OR ...) AND ag.i >= u64(a) AND ag.i <= u64(a + 15) ORDER BY ag.i:u64`, `select: {key, attributes: {ag.i, ag.body}}`, limit 16.
   - Run up to 8 page queries concurrently; on HTTP 413, a 5xx or a timeout, halve the page size down to 1 (the response size cap is unpublished, `arkiv-limits.md` section 5).
   - For each index take the first returned chunk whose SHA-256 matches the manifest (duplicates from push retries are expected); a missing or mismatching index is `E_NOT_LIVE { chunk }`.
   - Stream chunks into `<cache>/git/objects/pack/tmp-<pack_id>.pack`, check `SHA-256 = pack_id` and the trailer, then `git index-pack --strict --keep=arkiv <file>`.
6. Connectivity check per peer (section 3.6, last row).
7. Commit: marks first, then namespace refs in one `update-ref` transaction, then `applied_packs`.
8. Print canonical refs (or the single peer's refs) to git.

Nothing reaches namespace refs or `list` output before every check in section 3.6 passed for that peer, so git never sees an unverified OID.

### 4.4 Idempotent retry and partial failure

Chain state is the source of truth; `pending.json` only records what this push intends.

| Crash or failure point | What the next `git push` does |
| --- | --- |
| Before any chunk landed | Re-reads pending, uploads everything, continues |
| Half the chunks landed | Step 9 query finds the landed indices; uploads only the missing ones |
| A chunk tx dropped (no receipt after 60 s and the account nonce moved past it) | Treated as missing at step 9; resent |
| Chunks landed, manifest did not | Queries `ag.kind = str('manifest') AND ag.h = bytes32(M) AND $creator = addr(bulk)`; sends if absent |
| Refs not landed | Same, by refs `h` |
| Anchor sent, receipt unknown | Re-reads the own head: if it equals our anchor `h`, finishes step 15; if it is still the expected prev, resends; otherwise `E_CONCURRENT_PUSH` |
| Anchor landed, local commit not done | Re-reads the head, sees our anchor, finishes step 15 |
| New push while pending exists | If the new tips descend from the pending tips, keeps the pending packs and adds one pack excluding the pending tips; otherwise discards pending and prints the wasted cost (orphaned chunks expire) |
| Revert | Decoded against the `IEntityRegistry` errors; `NotOwner`, `TooManyAttributes`, `AttributesNotSorted`, `ExpiryDeadOnArrival` and similar are bugs (`E_INTERNAL`); out-of-funds is `E_INSUFFICIENT_FUNDS` |

Pack generation is not deterministic across runs (threaded delta search), which is why pack files are kept in `outgoing/` until anchored rather than regenerated.

### 4.5 Concurrency between pushers

- Different peers never conflict: each writes only its own namespace, bind chain and anchor chain; agreement on the default branch comes from the canonical-ref quorum, as in Radicle.
- One device is one nid: keys are never copied between devices, so two devices of one person are two peers (two delegates if both should count).
- One device, two concurrent pushes: serialised by the repo `flock`.
- Same nid on two machines (a copied key, unsupported): the anchor gate (step 12) catches most races; a true race produces equivocation, first-created wins, the loser gets `E_LOST_RACE`, and both are reported as `E_EQUIVOCATION` thereafter.
- Nonce safety: each address is used by one process at a time (the lock); the pipeline owns the nonce counter and re-reads `eth_getTransactionCount` on "nonce too low".

### 4.6 Transaction pipeline and pacing

- EIP-1559 tx, `maxFeePerGas = 2 x latest base fee`, priority fee 0 by default (`--tip` to raise), `gas = eth_estimateGas x 1.1` capped at 16,000,000.
- Window: at most 16 unconfirmed txs per sender, the reth default per-sender pool slot count (`arkiv-limits.md` section 4; live value ASSUMED until the funded burst probe).
- Pacing: at most 40 percent of the 30M block gas from this pusher per block, so the base fee stays at its 1 gwei floor (blocks under half full never raise it); that is 4 chunks per block at 1-day lifetime and 3 at 30 days; the pipeline also halves its rate whenever the base fee exceeds 2 gwei.
- Receipts are polled with `eth_getTransactionReceipt`; one included block is treated as final (section 3.2).
- RPC budget: about 4 requests per tx (send, a few receipt polls), far under the 2,000 requests per 10 s anonymous window, but the access key is still required for queries (section 1, item 4).

### 4.7 Push and clone time for 100 MiB

DERIVED from the numbers above:

- Push: 1,600 chunk txs at 4 per block is 400 blocks, about 13.3 minutes at 1-day lifetime, or 533 blocks, about 17.8 minutes, at 30 days; the upload itself is 200 MiB of hex JSON, about 3 minutes at 10 Mbit/s, overlapped with inclusion.
  Estimate: 15 to 20 minutes.
- Clone: about 110 queries (100 chunk pages of 16, the rest control plane) at about 0.5 to 0.7 s each (MEASURED latency), 8 concurrent, plus 200 MiB of hex JSON down (about 34 s at 50 Mbit/s, 84 s at 20 Mbit/s), plus `index-pack` of 100 MiB (about 10 to 20 s).
  Estimate: 1 to 3 minutes.

### 4.8 Query paging rules

- `limit` is at most 100 (MEASURED); P1 never relies on cursors, which the Rabdi SDK does not expose.
- Keyset paging wherever an ordering attribute exists (`ag.seq` for binds, anchors and revisions, `ag.i` for chunks): request `attr >= last` with `ORDER BY attr:u64 ASC`, drop keys already seen; more than 100 entities sharing one value (only possible through mass equivocation) fails with `E_PAGING`.
- Offset paging only for the identity query (`id` plus `idacc` in creation order), with `count` selected on every page; if the count changes between pages, restart, at most 3 times, then `E_INCONSISTENT_VIEW`.
- Query nesting stays within the node's limits: at most 4 levels of `AND`/`OR`, 64 predicates, 8 KiB of text (SOURCE-VERIFIED `crates/arkiv-query/src/limits.rs`); OR lists of addresses or hashes are flat inside one `AND`, so at most 2 levels deep, and are split into several queries above 48 terms.

## 5. Keeping data alive

Who extends what:

- The pushing peer's tooling runs a keepalive pass for its own repo state after every push (cheap when nothing is near expiry).
- Delegates are expected to run `chart keepalive` on a schedule (cron, CI); any sponsor may do the same with only a funded EVM key and no ML-DSA identity, because every entity is `permissionlessExtension`.

`chart keepalive arkiv://<rid> [--horizon 90d] [--renew-below 30d] [--max-spend <tGLM>] [--dry-run] [--evm-key <file>]`:

1. Run the fetch sync for the control plane (identity, binds, anchors, refs, manifests, chunk metadata, no chunk bodies) for delegates and tracked peers, so the keep set is computed from verified state and never extends spam.
2. Keep set:
   - every live entity with `$creator` in any control address of a verified peer (by query, valid or not, because the completeness check counts all of them),
   - the head `refs` entity of each verified peer,
   - every manifest named by any anchor of a verified chain, and for each the first valid chunk per index,
   - peer documents of verified peers.
3. Select entities whose `$expiresAt < head + renew_below` blocks; target `minLifetime = horizon` blocks (an extension counts from the landing block, so a fixed horizon is correct, `arkiv-limits.md` section 2).
4. Batch greedily, control plane first, then refs and manifests, then chunks: at most 100 ops and at most 15,000,000 predicted gas per tx, where predicted gas is `21,000 + sum(21,400 + units x added_blocks x 14e-6)`; confirm each batch with `eth_estimateGas` and split on excess.
5. Send through the same paced pipeline; stop before exceeding `--max-spend`.
6. Report: entities extended, txs, gas, tGLM spent, and the new minimum expiry in days.

Cost for a 100 MiB repo (DERIVED): extending a 64 KiB chunk by 60 days is about 2.47M gas, so about 6 chunks per tx; a year of storage is about 23.8 GLM plus about 0.1 GLM of per-op overhead, matching the 28 GLM per year total in `arkiv-limits.md` (which includes the one-time write).
The control plane costs about 1.4M gas per entity-year, under 0.0015 GLM each.

When data expires anyway:

- Clone and fetch fail closed with a specific code and a message that names the entity, for example:

```text
fatal: chart E_NOT_LIVE: pack 3f1c...a9 chunk 17 of 1600 is not live on Arkiv (expired or deleted).
This repo's keepalive lapsed; the data cannot be recovered through queries.
Peers that cloned earlier still hold it locally and can re-push it.
```

- A lapsed control-plane entity gives `E_CONTROL_NOT_LIVE` (section 3.3), never a silently older head.
- `chart status arkiv://<rid>` prints per-peer head, minimum expiry in days, entities expiring within 14 days, and equivocation notes; `git fetch` warns when the minimum expiry is under 7 days.
- Recovery from block history (create calldata of expired entities) is not attempted in P1.

## 6. Search

Discoverable annotations, written on every `id` revision of a public repo:

| Attribute | Type | Meaning |
| --- | --- | --- |
| `ag.name` | `str`, indexed | repo name, lowercase `[a-z0-9._-]{1,64}` |
| `ag.lang` | `str`, indexed | primary language, lowercase |
| `ag.lic` | `str`, indexed | SPDX licence id, as written by SPDX (`MIT`, `Apache-2.0`) |
| `ag.nid` on `bind` | `bytes32`, indexed | peers that write to a repo, delegates among them |
| `ag.at` on `anchor` | `u64`, indexed | block height seen at push, for recency |

Recency uses `ag.at` because `$createdAt` is returned but neither queryable nor usable in `ORDER BY` (api-spec section 4); `ag.at <= $createdAt` is enforced, so a writer can only understate recency.

Queries (operators all within the MEASURED set: `=`, ranges, `STARTSWITH`, `AND`, `OR`, `ORDER BY` with an explicit type; annotation queries on our own entities are ASSUMED until the first funded run):

```sql
-- exact name
ag.kind = str('id') AND ag.name = str('charter')

-- name prefix
ag.kind = str('id') AND ag.name STARTSWITH str('arkiv')

-- language and licence
ag.kind = str('id') AND ag.lang = str('rust') AND (ag.lic = str('MIT') OR ag.lic = str('Apache-2.0'))

-- repos a peer writes to (then check delegate membership client side)
ag.kind = str('bind') AND ag.nid = bytes32(0x<64 hex>)

-- pushed in the last 7 days, newest first (example head 600000 minus 302400 blocks)
ag.kind = str('anchor') AND ag.at >= u64(297600) ORDER BY ag.at:u64 DESC
```

Resolution:

- Results are hints from permissionless data; every hit is reduced to its rid, deduplicated, and its identity chain verified before display.
- A hit on an old revision whose head no longer matches the predicate (renamed repo, changed licence) is dropped after verification.
- Display: name, description, delegates (short nids), last anchor block, minimum expiry, so dead or spam repos are visible as such.
- `chart search [--name N | --prefix P] [--lang L] [--license S] [--peer NID] [--recent DAYS] [--limit N]`, at most 100 rows per underlying query, paged by keyset on `ag.at` for `--recent`.
- Private repos (P2) carry none of these attributes.

## 7. Rust workspace

### 7.1 Crates

Workspace at the repo root, edition 2024, `rust-version = "1.97"` (matches the node), alloy versions matching the node's lockfile.

| Crate | Kind | Responsibility | Depends on |
| --- | --- | --- | --- |
| `charter-crypto` | lib | `PublicKey`, `Sig`, `Signer`, `Verifier`, algorithm registry with `PK_LEN` / `SIG_LEN`, the ML-DSA backend behind a feature, `H(tag, x)`, seed handling, HKDF derivation of ML-DSA and EVM keys | `sha2`, `hkdf`, `zeroize`, the ML-DSA crate chosen in `pq-signatures.md`, `alloy-signer-local` |
| `charter-core` | lib, no I/O | CBOR body types for every kind, envelope encode / decode / canonical check, size caps, attribute schema per kind (`fn attributes(&Entity) -> Vec<Attribute>`), bind chain, identity evaluation, anchor chain and acceptance, completeness predicate, canonical refs, high-water-mark types, error codes | `charter-crypto`, `minicbor`, `gix-validate`, `thiserror` |
| `charter-store` | lib | `trait Arkiv` (below), `RpcArkiv` over alloy plus `arkiv-bindings` plus `arkiv-rpc-types`, `MemArkiv` fake (feature `fake`), typed query builder, tx pipeline (nonces, window, pacing, receipts, revert decoding), batching under byte and gas caps | `arkiv-bindings`, `arkiv-rpc-types`, `arkiv-query` (fake only), alloy, `tokio` |
| `charter-sync` | lib | git CLI wrapper, cache layout, `state.json`, `pending.json`, push, fetch, keepalive, search orchestration | core, store, crypto |
| `charter-cli` | two bins | `git-remote-arkiv` (helper protocol) and `chart` (`keys init/show`, `init`, `id show/propose/accept`, `keepalive`, `search`, `status`, `fund`) | sync |

Dependency direction: `crypto <- core`, `store` independent of `core`, `core + store + crypto <- sync <- cli`.
`core` is pure and is where the security rules live, so the property tests in section 8 run against it with no async and no fake.

Arkiv dependencies, pinned by commit:

```toml
arkiv-bindings  = { git = "https://github.com/Arkiv-Network/arkiv", rev = "2a3526dd861bf01c284d232d8ea4f880e24fd526" }
arkiv-rpc-types = { git = "https://github.com/Arkiv-Network/arkiv", rev = "2a3526dd861bf01c284d232d8ea4f880e24fd526" }
arkiv-query     = { git = "https://github.com/Arkiv-Network/arkiv", rev = "2a3526dd861bf01c284d232d8ea4f880e24fd526", optional = true }
```

The local clone's `origin` is a fork (`chayan-bit/arkiv`); the first implementation step confirms the commit is reachable upstream (tag `v1.0.1`) and otherwise pins the fork.
Reuse: `arkiv-rpc-types` supplies the `arkiv_query` / `arkiv_getEntity` wire types, and `arkiv-query` lets the fake parse exactly the strings the node parses, so a query the node would reject fails in CI too.

### 7.2 The trait boundary to Arkiv

```rust
pub trait Arkiv: Send + Sync {
    async fn chain_id(&self) -> Result<u64, StoreError>;
    async fn head(&self) -> Result<u64, StoreError>;
    async fn base_fee(&self) -> Result<u128, StoreError>;
    async fn query(&self, q: &Query, opts: &QueryOpts) -> Result<QueryPage, StoreError>;
    async fn entity_nonce(&self, creator: Address) -> Result<u64, StoreError>;
    async fn balance(&self, a: Address) -> Result<U256, StoreError>;
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError>;
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError>;
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError>; // execute(ops) or a plain transfer
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError>;
}
```

- `Query` is a small AST (`Eq`, `Ge`, `Le`, `StartsWith`, `And`, `Or`, system attributes, one `OrderBy` with type) rendered to the Arkiv query string; there is deliberately no `Not` variant.
- `QueryOpts` carries `limit` (at most 100), `offset`, `select` (key, creator, createdAt, expiresAt, named attributes, count).
- `TxPipeline` (window, pacing, nonce, resend) is generic over `Arkiv`, so the fake exercises the same code as Rabdi.

`MemArkiv` must model, each with a test of its own:

- creation order, keys from `keccak256(domain, owner, nonce, salt)`, `entityNonce` per creator,
- attribute rules (32 per entity, names at most 32 B and grammar, `str` at most 128 B, `bytes` never indexed, sorted, `bytes` at most 128 KiB),
- tx caps (131,030 B raw, 16,777,216 gas with the measured gas model), per-sender window of 16,
- expiry by block, extension rules (owner or `permissionlessExtension`, never shorter, max lifetime), `readonly`, owner delete,
- query semantics for the operators used, `limit <= 100`, offset, `ORDER BY` ties in creation order, `NOT` rejected as on GolemDB (conformance Q1), `key` and `bytes32` conflated as on GolemDB (conformance Q2),
- a manual block clock, plus fault injection: drop the next n txs, revert at op k, withhold chosen entities from queries, lie about `entityNonce`, fail page queries above a byte size.

### 7.3 Error model

One `thiserror` enum per crate (`CryptoError`, `FormatError`, `VerifyError`, `StoreError`, `SyncError`), each wrapping the lower one with context, and each variant mapped to a stable code that users, scripts and tests match on.

| Code | Meaning | Retriable |
| --- | --- | --- |
| `E_NOT_LIVE` | a required refs, manifest or chunk entity is not live | no |
| `E_CONTROL_NOT_LIVE` | completeness check failed (expired or deleted control-plane entity) | no |
| `E_ID_NOT_LIVE` | identity root or revision missing | no |
| `E_STALE_HEAD`, `E_STALE_IDENTITY` | Arkiv shows a head below the local high-water mark | no |
| `E_HISTORY_REWRITTEN` | chain no longer contains the marked anchor | no |
| `E_CHAIN_GAP` | a live valid anchor or bind above the reachable head | no |
| `E_BAD_SIG`, `E_BAD_FORMAT`, `E_BAD_REFS`, `E_BAD_PACK`, `E_MISSING_OBJECTS`, `E_MISSING_DEPENDENCY` | verification failures | no |
| `E_UNSUPPORTED_VERSION` | unknown format version from a tracked control address | no, upgrade |
| `E_EQUIVOCATION` | warning only | n/a |
| `E_CONCURRENT_PUSH`, `E_LOST_RACE` | own head moved during a push | yes, after fetch |
| `E_UPLOAD_INCOMPLETE` | chunks still missing after 3 rounds | yes |
| `E_INSUFFICIENT_FUNDS` | address, need, have | after funding |
| `E_REFS_TOO_LARGE` | refs body over the cap | no |
| `E_RPC`, `E_RATE_LIMITED`, `E_INCONSISTENT_VIEW`, `E_PAGING` | transport and node issues | yes, with backoff |
| `E_WRONG_CHAIN` | RPC chain id differs from the repo's | no |
| `E_INTERNAL` | a revert that indicates our bug | no |

Mapping to git: fetch-side errors print `fatal: chart <CODE>: <message>` on stderr and exit 128; push-side errors are reported per ref as `error <dst> <CODE> <message>`; the `chart` CLI exits 1 for verification errors, 2 for usage, 3 for transport, 4 for funds.

## 8. Test plan for the P1 gate

### 8.1 Unit (CI)

- CBOR: round trip for every body; rejection of non-shortest integers, indefinite lengths, unsorted or duplicate keys, unknown keys, trailing bytes, floats, tags.
- Envelope: a signature for kind A never verifies as kind B; flipping any byte of `body`, `signer`, `kind` or `enc` fails; `enc != 0` from a tracked control address gives `E_UNSUPPORTED_VERSION`.
- Size caps: a 1,600-chunk manifest fits under 64 KiB with `SIG_LEN` = 3,309 and 4,627; refs at the cap encode, one byte more fails.
- Attributes: for every kind, generated attributes obey every Arkiv rule (count, name grammar and length, `str` length, `bytes` unindexed, sorted order) and match the body.
- Chunking: split and reassemble at sizes 0, 1, 65,535, 65,536, 65,537 and 100 MiB.
- Key derivation: fixed seed gives fixed nid, wallet, control and bulk addresses (test vectors checked in).
- Query rendering: every query the code emits parses with `arkiv-query` and contains no `NOT` or `!=`.
- Canonical refs: quorum, tie, threshold above votes, tags.

### 8.2 Property tests (CI, `proptest`, against `charter-core` only)

Generator: an honest history for one peer (binds, anchors with random refs, occasional EVM rotation) laid out in creation order, then a random adversarial schedule mixing:

- replay: copies of old anchor or refs bytes created by a foreign address, and by the peer's own control address at a later position,
- fork and equivocation: a second validly signed anchor at a taken seq, or with a wrong prev,
- reorder: delivering query pages in shuffled order (the algorithm must sort by the declared order, not arrival),
- expiry and deletion: removing a random subset of control-plane entities from the live set while `entityNonce` stays at the true count,
- withholding: hiding the newest k anchors from queries with `entityNonce` lying and not lying,
- third-party extension of old anchors after the newest expired.

Properties:

1. Soundness: whenever the algorithm returns a head, that head is a validly signed anchor of the honest signer and every anchor below it in the chain is too.
2. Freshness: with every control entity live, the head is exactly the honest newest anchor, whatever replays and copies were added.
3. Fail closed: if any honest control-plane entity is missing and `entityNonce` is truthful, the result is `E_CONTROL_NOT_LIVE` or `E_CHAIN_GAP`, never a head.
4. Monotonicity: with a high-water mark at seq w, the result is never a head below w.
5. Determinism: any two permutations of delivery give the same head and the same equivocation set.
6. Creator rule: nothing created by an address outside the bind chain ever changes the result.

Identity properties, same style: quorum math, first-quorum-wins among siblings, dead siblings never revive, rotation replaces the delegate set, revisions accepted only by the parent's delegates, and an expired newer revision fails closed.

### 8.3 Integration against `MemArkiv` (CI)

- Push then fresh clone of 8 MiB (default CI) and 100 MiB (nightly), byte-identical refs and `git fsck --full` clean.
- Incremental pushes (10 pushes of small commits) then clone; ref deletion; push with no new objects.
- Partial failure: drop txs at chunk k, crash after the manifest, crash after the anchor send; the next push uploads only what is missing and ends with exactly one new anchor.
- Concurrency: two peers pushing the same repo; one peer racing itself without the lock gives `E_LOST_RACE` plus `E_EQUIVOCATION`.
- Replay gate rehearsal: three pushes A, B, C; a foreign address re-posts A's anchor and refs; the peer's control key re-posts A's anchor; a fresh clone gets C; a clone with a mark at C served a withholding view gets `E_STALE_HEAD`; a fresh clone after C's anchor expired (A and B extended by a third party) gets `E_CONTROL_NOT_LIVE`.
- Expiry: a chunk expires, clone fails with `E_NOT_LIVE` naming pack and index; keepalive before expiry prevents it.
- Keepalive: batches respect 100 ops and 15M gas, spam entities with the repo's `ag.rid` are not extended.
- Pacing: the pipeline never exceeds 40 percent of block gas in the fake's block accounting.
- Paging: more than 100 anchors, chunk pages shrinking after injected 413s, count change mid-paging restarts.

### 8.4 Local node tier (CI, no funds)

- `docker compose -f docker/compose.yml up` from the node repo (single `arkiv-reth --dev` on GolemDB, 2 s blocks; reth `--dev` prefunds the standard dev accounts, ASSUMED until the first run).
- Runs the 8.3 push / clone / replay / expiry scenarios against the real ABI, the real query parser and store, and the real gas and size enforcement; the 100 MiB clone runs here nightly.
- It does not reproduce the public RPC edge (131,030 B and 256 KiB body caps, rate limits, anonymous budget) or Rabdi's `eth_getLogs` behaviour, so it complements Rabdi rather than replacing it.
- Per the machine's compute rules, CI and nightly runs go to the CI runner or `staging-entity`, not the Mac.

### 8.5 Rabdi E2E (needs a funded testnet key and an access key)

Scenario:

1. Generate a deterministic 100 MiB repo (`scripts/gen-repo.sh <seed>`: 200 commits of incompressible blobs, so the pack is about 100 MiB).
2. `chart init`, then `git push --lifetime 1d` (test override) to Rabdi; record wall time, txs, gas, tGLM.
3. In a fresh `$CHARTER_HOME` and empty directory: `git clone arkiv://<rid>`; compare `git for-each-ref` and `git rev-list --all --objects | sha256sum` with the source; `git fsck --full`.
4. Replay: on a small second repo, pushes A, B, C; re-post A's anchor and refs bytes from a foreign funded address and from the peer's own control address; fresh clone must give C; a clone holding the mark at C against a local proxy that hides C must give `E_STALE_HEAD`.
5. Expiry: push D with a 100-block anchor lifetime override, extend A to C from a third address, wait 100 blocks (about 3.5 minutes), fresh clone must give `E_CONTROL_NOT_LIVE`.
6. Search: the gate repo appears in name, prefix, language and recency queries.

Cost estimate (DERIVED, 1 gwei base fee held by pacing):

| Item | Gas | tGLM |
| --- | --- | --- |
| 1,600 chunks at 1-day lifetime, 2.72M each | 4.35G | 4.35 |
| Manifest, refs, peer, bind, root identity, anchor | about 6M | 0.006 |
| Replay and expiry repos (small, about 10 control txs plus 3 small packs) | about 20M | 0.02 |
| Wallet top-up transfers | negligible | under 0.001 |
| One full gate run | about 4.4G | about 4.4 |

Budget about 15 tGLM for three full runs plus debugging at 10 MiB; the faucet amount per claim is unknown and it has a 24-hour cooldown (open question Q5).
Time: push 15 to 20 minutes, clone 1 to 3 minutes, replay and expiry steps about 10 minutes, so about 35 to 45 minutes per run.
The run also closes the ASSUMED items: inclusion finality, per-sender window, response size cap per page, annotation queries on our own entities, `ORDER BY` ties, and the 66.9 KB chunk tx passing the RPC edge.

### 8.6 What runs where

| Tier | Where | Needs |
| --- | --- | --- |
| Unit, property, `MemArkiv` integration (8 MiB) | every CI run | nothing |
| `MemArkiv` 100 MiB, local node tier | nightly CI | Docker |
| Rabdi read-only smoke (queries, `entityNonce`, chain id) | nightly CI | access key secret |
| Rabdi funded E2E (8.5) | manual, or nightly once funding is reliable | testnet key secret (testnet only, never a real key) and access key |

## 9. Room left for P2

- Envelope `v` and `enc`, and `ag.v`, allow a ciphertext body and a new layout without touching P1 readers' fail-closed behaviour.
- All identifier attributes are `bytes32`, ready to hold `HMAC(k, value)`; search attributes are simply not written for private repos.
- Chunk and pack hashes are over stored bytes, so availability stays verifiable without keys.
- Per-repo EVM addresses already avoid linking one peer across repos.
- `IdBody` field 9 reserves private visibility; `ManifestBody` gets a key-epoch field under a new version.
- What P2 must still decide: whether signatures cover plaintext as well as ciphertext, whether `ag.kind` is blinded, and how the identity chain is hidden while staying verifiable by members.

## 10. Open questions

### 10.1 Need a human decision

- Q1. Signature parameter set and crate (pending `docs/p0/pq-signatures.md`).
  Recommendation: pure ML-DSA-65; every format here is parameterised on `PK_LEN` / `SIG_LEN` and fits ML-DSA-87 too, so only the 64 KiB body-cap arithmetic and sizes move.
- Q2. Drop heartwood crates in P1.
  ML-DSA breaks byte compatibility with Radicle's Ed25519 / SHA-1 formats, so this spec ports their semantics and depends on no heartwood crate, which reverses the P0 "depend" pick and gives up a byte-compatible Radicle bridge.
  Recommendation: accept for P1, and revisit `radicle-cob` for P3 only if it can take an abstract signer.
- Q3. EVM keys derived from the same seed as the ML-DSA key (P0 open decision 4).
  Recommendation: derive, as in section 3.1; consequence to accept: secp256k1 is not post-quantum, so a future quantum attacker who recovers a control key can delete control entities, which this design turns into `E_CONTROL_NOT_LIVE` (denial of service), not a rollback.
- Q4. Seed storage at rest.
  Recommendation: a 0600 file for P1, OS keychain or passphrase encryption in P2.
- Q5. Testnet funding for the gate: about 4.4 tGLM per full run, about 15 tGLM in total.
  Recommendation: ask Arkiv for a testnet grant or a faucet allowance; until then, run the full scenario on the local node tier and a 10 MiB variant on Rabdi.
- Q6. Lifetime defaults: data 30 days, control plane maximum, keepalive horizon 90 days, renew below 30 days.
  Recommendation: accept; they trade roughly 4 renewals a year against push throughput and are config values.
- Q7. Canonical refs by exact-match quorum instead of heartwood's merge-base quorum.
  Recommendation: accept for P1 (identical for single-delegate repos); port merge-base quorum when multi-delegate repos appear.

### 10.2 Resolved here (flag if you disagree)

- Encoding: canonical CBOR with a re-encode check.
- Two EVM roles per repo (control, bulk) plus a wallet, and the completeness check by `entityNonce`, instead of log scanning (logs are broken on Rabdi) or non-permissionless anchors (ruled out by the fixed decision).
- Ordering from query creation order; no rule depends on within-tx order.
- Equivocation: warn and keep first-created-wins; no exclusion and no on-chain evidence in P1.
- All control-plane entities kept alive forever; old refs may expire; no anchor checkpointing (control-plane growth is about 4 KB and 1.4M gas per year per push, acceptable until P3).
- Self-contained packs only, at most 100 MiB each; thin packs and repacking on Arkiv deferred.
- One chunk per tx, paced at 40 percent of block gas.
- Clone fetches delegates by default; others by config or `--all-peers`.
- Refs over about 62 KB fail with `E_REFS_TOO_LARGE`.

### 10.3 Open, with a recommendation, no decision needed now

- Q8. Repos with more refs than fit in one refs entity (about 1,100).
  Recommendation: in P3, store large refs bodies as a pack-like blob (manifest plus chunks) referenced from the anchor.
- Q9. More than 16 delegates.
  Recommendation: raise the cap only with measured need; 16 keeps a revision under 5 KB.
- Q10. Recovery after an old control key is compromised and deletes old control entities: the repo-peer is then permanently `E_CONTROL_NOT_LIVE`.
  Recommendation: the remedy is a new nid via an identity revision; consider a signed "tombstone acknowledgement" in P3 if this ever happens in practice.
- Q11. `eth_getLogs` and receipts: once Rabdi serves log ranges, add an optional audit that cross-checks positions and recovers the content of expired anchors from create calldata for diagnostics.
- Q12. Anonymous query budget: measure the actual quota with an access key in the funded run and document the per-clone cost in cost units (10 per query, about 1,100 units per 100 MiB clone).
