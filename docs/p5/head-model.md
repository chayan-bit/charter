# P5: the head model (format v2)

Status: design for v0.4.0, implemented on branch `feat/head-model`.
Written 2026-11-11.
It implements `docs/p5/speed-and-cost.md` section 3.4 with the decisions taken on 2026-11-11 and handles every downside of section 3.9.
Labels follow `docs/p0/arkiv-limits.md`: MEASURED, DERIVED, ESTIMATE.

## 1. Scope

Only git data moves: refs and pack bytes.
Peer documents, binds, identity revisions and accepts stay exactly as in P1.
Collaboration ops (issues, patches, comments, reviews, releases, attestations, escrow, timelock, program pins) stay one entity each, so listing, programs, CI, releases and third-party queries see no change.
v0.4.0 reads v1 (anchors) and v2 (heads) and writes only v2.
Signatures stay composite ML-DSA-65 + Ed25519 on every signed body.

## 2. What Rabdi offers (MEASURED 2026-11-11)

- `arkiv_query` with `atBlock` answers only for the latest block; any older block fails with "state for this block is not retained".
- `eth_call` at an older block works for less than about 32 blocks, then fails with "block not found".
- `eth_getLogs` fails (`golemdb segment_range_of(transactions): NotImplemented`), and `eth_getTransactionBySenderAndNonce` does not exist.
- Every mutation of an entity, extend included, stamps `$updatedAt` (`arkiv-reth-executor` `mutate`), so `$updatedAt` is not the block of the last patch.
- Patching a mutable entity works; a same-size patch is charged calldata only (3.4).
- `eth_getTransactionByHash` and `eth_getBlockByNumber` with full transactions return the calldata of old transactions (3.8).

Consequence for decision 3: the rollback invariant cannot be checked "at the head's block", so v2 checks it at the latest block inside a nonce bracket, which needs one extra rule (the device control address only creates entities inside head transactions, section 6).
Consequence for ordering: the block a head body landed in is found by scanning forward from the block the body names, never from `$updatedAt` (section 5.4).

## 3. Keys and addresses

- The identity key is today's `keys/seed`: the composite key whose nid is the peer in every P1 rule, plus the `wallet`, `control(rid)` and `bulk(rid)` EVM keys derived from it.
- Each `CHARTER_HOME` also has a device seed, `device/seed` (32 random bytes, mode 0600), created on the first v2 push; it lives outside `keys/` so copying `keys/` to a second machine yields a second device, never a second writer of one head.
- The device seed derives a composite device key (device nid `d`) and the per-repo addresses `C_d = control(rid)` and `B_d = bulk(rid)` with the P1 derivation.
- The identity wallet funds `C_d` and `B_d` with the rolling top-ups of P1 section 4.2 step 6.
- Single-machine users never see a device: no command creates, lists or revokes one.

## 4. Entities

### 4.1 Device delegation (`device`)

```cddl
DeviceBody = {
  1: bstr .size 32,   ; rid
  2: uint,            ; device alg id
  3: bstr,            ; device public key
  4: bstr .size 20,   ; C_d, creates the head and inline pieces
  5: bstr .size 20,   ; B_d, creates bulk pieces and snapshot chunks and manifests
}
```

- Signed by the identity key (envelope `signer = N`, envelope kind code 11, signature context `arkiv-git/device/v1`); the device nid is `H("nid", u16be(alg) || pk)`.
- Stored as `ag.kind = 'anchor'` with `ag.v = 2`, plus `ag.rid`, `ag.nid = N`, `ag.h = H("device", envelope)` and `ag.seq = 0`, created by the identity's control address `C_N` with the maximum lifetime and kept alive like every control-plane entity.
- `ag.kind = 'anchor'` with `ag.v = 2` is deliberate: an unmodified v0.3.0 client reads every anchor of a tracked control address, meets an entity layout version it does not know, and stops with `E_UNSUPPORTED_VERSION` instead of showing stale refs (section 10).
- Because `C_N` creates it, deleting it or letting it expire trips N's P1 completeness check (`E_CONTROL_NOT_LIVE`), so a device cannot be hidden.
- A device delegation is valid if it opens with layout version 2, its signature verifies under N's key, `body.rid = R`, and its creator is one of N's bind control addresses under the P1 creator rule.
- A device is not revocable on its own: it holds the same identity seed as every other device, so a lost device is a lost identity key, removed by an identity revision exactly as in P1 section 3.5.

### 4.2 Head (`head`)

```cddl
HeadBody = {
  1: bstr .size 32,          ; rid
  2: bstr .size 32,          ; N, the identity this device writes for
  3: uint,                   ; seq of this device's head, 0, 1, 2, ...
  4: bstr .size 32,          ; id_head = h of the identity revision the writer verified
  5: uint,                   ; at = latest block seen just before signing
  6: uint,                   ; pieces_created = entityNonce(C_d) after this tx, minus 1
  7: [* RefEntry],           ; the identity's full ref list as this device wrote it
  8: [* bstr .size 32],      ; snapshot: h of each snapshot manifest
  9: [* Pack],               ; packs since the snapshot, in apply order
  10: bstr .size 32,         ; prev_tx = tx that carried this device's previous body, zero at seq 0
}
RefEntry = [name: tstr, oid: bstr, counter: uint, stamp: uint]   ; empty oid = tombstone
Pack = [pack_id: bstr .size 32, pieces: [* [tx: bstr .size 32, sha256: bstr .size 32]]]   ; zero tx = this tx
```

- Signed by the device key (envelope `signer = d`, envelope kind code 12, context `arkiv-git/head/v1`).
- Stored with `ag.kind = 'head'`, `ag.v = 1`, `ag.rid`, `ag.nid = N`, `ag.dev = d`, `ag.h = H("head", envelope)`, created once by `C_d` with flags `permissionlessExtension` only (not read-only) and the maximum lifetime, then patched by every push (`ag.body`, `ag.h`).
- Anyone may extend it (keepalive, delegates, mirrors); extends do not change what a reader trusts, because nothing reads `$updatedAt`.
- Ref names follow `refs` (P1 2.2); entries are strictly ascending by name; `counter >= 1`; `stamp = 0` means "the block this body landed in", otherwise it is a block number.
- `pack_id = SHA-256(pack bytes)`; a pack is the concatenation of its pieces in order; each piece is at most 65,536 B.
- Body size (DERIVED): about 200 B of fixed fields, about 55 B per ref, 34 B per snapshot manifest, about 70 B per piece, plus the 3,417 B envelope (44 B framing and the 3,373 B signature); a small repo's head is about 3.8 KiB.
- The head is not padded: a growing body pays storage for the added bytes over its remaining lifetime (about 6,500 gas for 64 B with 170 days left, DERIVED), which costs less than shipping zero padding on every push (3.8).

### 4.3 Pieces (`piece`)

- `ag.kind = 'piece'`, `ag.rid`, `ag.body` = raw bytes (sealed for private repos), created with a 20-block lifetime and never extended.
- Inline pieces sit in the head transaction and are created by `C_d`; bulk pieces are created by `B_d` in transactions that land before the head transaction.
- After expiry a piece is read back from block history: a bulk piece by its tx hash, an inline piece through the head transaction (section 5.4).
- A push with no inline piece (a refs-only push, or a push whose bytes all went to the snapshot or to bulk pieces) still creates one inline marker piece with an empty body, so every head transaction creates at least one entity from `C_d`.

### 4.4 Snapshot

- A snapshot is the self-contained pack (or packs, split at 100 MiB) of every object reachable from the live refs of the new head, written with today's `manifest` and `chunk` entities from `B_d`, signed by the device key, 30-day lifetime, kept alive by keepalive.
- The head names its snapshot manifests by `h`; superseded snapshots are simply not extended any more and expire.
- A device writes a fresh snapshot when the identity has no snapshot yet in this repo (a new repo or a migration), when the piece bytes since its last snapshot plus this push would exceed `max(1 MiB, 25 percent of the snapshot)`, when this would be its 64th push since the snapshot, or when the pieces list would exceed 256 entries; otherwise the push goes as pieces.
- On a snapshot push the push's own objects are in the snapshot, so it carries only the marker piece.

## 5. Reading

### 5.1 Discovery, one round of queries

Per synced peer N: the P1 peer document and bind chain, the anchor query (which now returns v1 anchors and device delegations, split by `ag.v`), and `ag.kind = 'head' AND ag.rid = R AND ag.nid = N`.
A peer with at least one valid device delegation is a v2 peer: its refs come only from its device heads, and its v1 anchors only count in its P1 completeness check.
A peer without one is read exactly as in P1.

### 5.2 Accepting one device head

A head entity is the head of device `d` if all of these hold:

1. `ag.kind = 'head'`, layout version 1, the body opens canonically, and every attribute equals the value derived from the body and envelope.
2. The envelope signer is `d`, the signature verifies under the key in `d`'s delegation, `body.rid = R` and `body.nid = N`.
3. The creator is `C_d` from the delegation, and `body.id_head` is in the verified identity chain.
4. The rollback check of section 6 holds for `C_d`.
5. If a local mark exists for `d`, `body.seq > mark.seq`, or `body.seq = mark.seq` and the body hash equals the mark's hash; otherwise `E_STALE_HEAD` or `E_HISTORY_REWRITTEN`.

A delegated device with no head is accepted only if `entityNonce(C_d) = 0` (it never pushed); otherwise `E_CONTROL_NOT_LIVE`.
More than one valid head from one `C_d` fails the rollback check, because only one create of a head is legitimate.

### 5.3 Merging device heads into the identity's refs

- Every entry of every accepted head of N competes for its name.
- Precedence, highest first: the larger `counter`; on a tie, the earlier landing block (`stamp`, with `stamp = 0` resolved to the block the body landed in); on a tie, the smaller device nid.
- The winner of each name is N's value for that ref; a winning tombstone means the ref does not exist.
- Why a counter: a stale copy of an entry can never beat a newer write, because a writer always sets `counter = winner.counter + 1` for the refs it updates.
- Why first-landed wins a tie: two equal counters mean two writes from the same base that did not see each other; the one that landed first already returned `ok` to its user, so the later one must lose, which its re-check reports as `E_LOST_RACE` (section 7.3), exactly like a concurrent push to one GitHub branch.
- Two writes that land in the same block both re-check after that block, so the device-nid tie-break gives every client the same winner and both writers the same verdict.
- Writes to different refs never compete, so two devices pushing different branches concurrently both keep their branch.

### 5.4 Finding the transaction of a body

The landing block of the current body and its tx hash are needed only to resolve `stamp = 0` in a tie and to read inline pieces after they expired.
The reader fetches blocks `at + 1, at + 2, ...` with full transactions until it finds the transaction from `C_d` whose create or patch of this head carries exactly this `ag.body`; usually that is the first block.
The search stops at `min($updatedAt, at + 64)`, and a body not found there is unreadable (`E_NOT_LIVE`).
The writer keeps that window: it reads `at` immediately before signing, and if its receipt lands later than `at + 64` it sends a follow-up head at once that carries concrete stamps and tx hashes.
The next body of the same device replaces every `stamp = 0` and every zero piece tx with the concrete block and tx hash, so only the current body ever needs the search.

### 5.5 Clone and fetch

- Download, for every accepted head of every synced peer: the snapshot manifests (query by `ag.h`, creator `B_d`) and their chunks (P1 chunk paging, creator `B_d`), then the pieces of every pack not yet applied.
- Bulk pieces are fetched by `eth_getTransactionByHash`, inline pieces through the body transaction (section 5.4); every piece must match its sha256 in the signed head and every pack its `pack_id`.
- Snapshots are indexed first, then packs with `index-pack --fix-thin`, retried in rounds while progress is made (thin packs only use bases of the same identity, section 7.2).
- The connectivity check, the canonical-ref quorum and the program fold run on the merged refs exactly as on P1 refs.
- A clone therefore costs one snapshot plus at most the piece bytes since it, whatever the history length, and old blocks being pruned loses nothing a user can see once the snapshot exists.

## 6. Completeness and the rollback invariant

Rules for the device control address `C_d`:

- `C_d` creates entities only inside head transactions: the head itself (once) and inline pieces or the marker.
- Every head transaction creates at least one entity from `C_d`.
- `body.pieces_created = entityNonce(C_d) after the transaction - 1` (the 1 is the head itself).

Check, run on every fetch for every device of every v2 peer: read `n1 = entityNonce(C_d)`, then the heads, then `n2 = entityNonce(C_d)`; retry up to 3 times while `n1 != n2`; then require `head.pieces_created + 1 = n1`, else fail closed with `E_CONTROL_NOT_LIVE`.
This is the coordinator's invariant `live control-plane entities of C_d + pieces_created = entityNonce(C_d)` with the head as the only control-plane entity of `C_d`, checked at the latest block, because Rabdi cannot answer for older blocks (section 2).

Why a rollback is caught by a fresh client with no checkpoint and no local state:

1. A key holder of `C_d` (with or without the device key) can patch the head back to any body the device signed earlier, with `pieces_created = p_old`.
2. Between that old body and the newest one, the device sent at least one more head transaction, and each created at least one entity from `C_d`, so at the newest body `entityNonce(C_d) >= p_old + 2`.
3. `entityNonce` never decreases, and the rollback transaction itself can only add creates.
4. So after the rollback `entityNonce(C_d) >= p_old + 2 > p_old + 1`, and the check fails.
5. The same holds for a body replayed into a second head entity: the extra create raises `entityNonce` past every signed body.

Why an honest device never fails it:

- Pieces that must land before the head (big pushes) come from `B_d`, never from `C_d`, so a crash between them and the head leaves `C_d` untouched.
- The head transaction is one atomic `execute`, so the nonce and the body change together.
- The writer computes `pieces_created` from `entityNonce(C_d)` read just before signing, so nothing it ever created is miscounted.
- The `n1, n2` bracket removes the race with a push landing between the reads.

What it costs an attacker who only holds `C_d`: any create from `C_d` outside a head transaction breaks the check for every reader, which is a denial of service, the same power a stolen P1 control key already has (it can let an anchor expire); it can never produce an older or forged view that passes.
Deleting the head is caught too: a delegated device with `entityNonce(C_d) > 0` and no head fails closed.
Deleting or expiring a device delegation is caught by N's P1 completeness check, because `C_N` created it.
Clients also keep the highest seq and body hash seen per device (section 5.2, rule 5).

## 7. Writing

### 7.1 Push

1. `list for-push` runs the fetch sync, so the base is the verified merged view of the own identity, and git refuses a non-fast-forward against it exactly as today.
2. Stage objects in the cache as today.
3. New entries: the base winners, plus each update with `counter = base counter + 1` (or 1) and `stamp = 0`; every `stamp = 0` of the device's previous body becomes its landing block; tombstones are kept only while another device head of N still lists that name as a live ref with lower precedence.
4. Pack: one thin pack of the objects reachable from the updated tips, excluding every object named by any head of the own identity; or a snapshot (section 4.4).
5. First write of this device to this repo: one `C_N` transaction with the P1 `peer` and `bind_0` if missing and the device delegation, waited for.
6. Snapshot chunks and manifests, or bulk pieces, from `B_d`, through the pipeline; every receipt must succeed.
7. The head transaction from `C_d`: `[create head]` the first time, `[patch head]` afterwards, plus the inline pieces or the marker; `at` and `entityNonce(C_d)` are read just before signing.
8. Re-check (7.3), then commit locally: index the packs, move staging refs into the namespace, store the device mark and the tx hash and block of the body.

A small push is one transaction and one block after the staging, whatever the repo's history.

### 7.2 Thin packs

A piece is a thin pack whose delta bases are objects of the own identity only (any device), never of another peer, so every identity's data is self-sufficient and no `requires` list is needed.
Merging another peer's commits therefore uploads those objects once more into the merger's pieces, which is the price of not depending on another peer's keepalive.

### 7.3 Re-check after the receipt

After the head transaction lands, the writer re-reads the heads of its identity, verifies them, merges, and requires that for every ref it updated its own entry is the winner.
Otherwise the push reports `E_LOST_RACE` (another device of the same identity won a concurrent push from the same base); the losing entry stays in this device's head but never wins, and the next fetch and push proceed from the winner.

### 7.4 Concurrency

- One device, two processes: serialised by the repo lock, as today.
- Two devices: never the same head, so a blind write can never overwrite another device; conflicts are decided by section 5.3 and reported by 7.3.
- A copied `CHARTER_HOME` (two machines sharing one device seed) is the only way to get two writers of one head; the later patch overwrites the earlier body, the earlier writer's re-check fails with `E_LOST_RACE`, and the rollback check stays sound because every patch adds creates.

## 8. Programs, releases, CI

- `charter-program` folds the same events as before: verified ops in Arkiv order, and every delegate's ref updates after the pin.
- For a v2 delegate, the ref updates come from its head bodies: the reader walks each device's bodies back through `prev_tx` (block history) to the first body before the pin, orders all bodies of the identity by landing (block, tx index), merges after each body, and emits the differences as `RefUpdate`s; v1 anchors before the migration produce events exactly as today.
- Entities and head bodies share one order: by block, entities first in query order, then head bodies by tx index; every client computes the same order, so every client folds the same state.
- Fetched bodies are cached under the repo directory, so each fetch walks only the bodies it has not seen.
- `chart program status`, `chart release verify`, `programs/ci-gated.rhai`, `scripts/chart-ci.sh` and `scripts/chart-merge.sh` need no change: they consume the fold.

## 9. Private repos

- Piece and snapshot bytes are sealed by `charter-private` exactly like chunks, since they are creates carrying `ag.rid`.
- The head patch is sealed too: its `ag.body` moves into a sealed body under the current epoch key with `ag.ep`, like a sealed create.
- Rows read back from block history (pieces and head bodies) go through the same opening as query rows.

## 10. Migration and old clients

- The first v0.4.0 push to a v1 repo migrates it in that same push: the device delegation (the v0.3.0 stopper), the first snapshot of everything reachable from the merged refs, and the first head, whose entries are the peer's v1 head refs with `counter = 1` plus the push's updates.
- No command is needed; other identities migrate on their own first v0.4.0 push, and a repo may mix v1 and v2 peers.
- After the migration, keepalive extends the identity's control plane (old anchors included, because the P1 completeness check counts them), the device heads and the current snapshots, and stops extending v1 refs, manifests and chunks.
- A new repo created by v0.4.0 carries a device delegation from its first push, so v0.3.0 stops on it too.
- An unmodified v0.3.0 client fetching or cloning such a repo reads the device delegation among the peer's anchors and stops with `E_UNSUPPORTED_VERSION ... from a tracked control address has an unsupported version`; it never lists stale refs (MEASURED on Rabdi, section 12).
- v0.4.0 prints the hint "this repo uses a newer Charter format; upgrade chart" with that error.

## 11. Downsides of 3.9 and what a user can still notice

1. Several machines: automatic device heads; nobody runs a command; a concurrent push to the same branch from two machines gives the loser `E_LOST_RACE` as today.
2. Collaboration ops stay entities, so nothing about issues, patches, reviews, programs or queries changes.
3. History availability: the snapshot is a paid, kept-alive entity, so pruned blocks lose nothing visible; only the piece bytes since the last snapshot (at most `max(1 MiB, 25 percent)`) live in block history alone, and a node that pruned those few blocks makes clone fail with `E_NOT_LIVE` until the next snapshot.
4. Rollback: detected by fresh clients without checkpoints (section 6); clients that saw a newer head also reject older ones by seq.
5. Migration: automatic on the first push; v0.3.0 fails loudly on a migrated repo.

What remains noticeable: a snapshot push is slower and costs more (entity chunks at 30 days), about every 64 pushes or 1 MiB of changes; a first push from a new machine adds one transaction (the delegation); a refs-only push still writes one small marker; block-history retention is not a priced promise (section 12 records how far back Rabdi serves it).

## 12. Measurements

Filled in from the on-chain run, see `docs/p5/speed-and-cost.md` section 3.15.
