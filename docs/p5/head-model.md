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
- Stored as `ag.kind = 'anchor'` with `ag.v = 2`, plus `ag.rid`, `ag.nid = N`, `ag.h = H("device", envelope)` and `ag.seq = 2^64 - 1`, created by the identity's control address `C_N` with the maximum lifetime and kept alive like every control-plane entity.
- `ag.kind = 'anchor'` with `ag.v = 2` is deliberate: an unmodified v0.3.0 client reads every anchor of a tracked control address, meets an entity layout version it does not know, and stops with `E_UNSUPPORTED_VERSION` instead of showing stale refs (section 10).
- `ag.seq` is the maximum because v0.3.0 reads anchors incrementally with `ag.seq >= mark`: any smaller value would drop out of a long-lived v0.3.0 clone's read once its mark passed it, and that clone would keep showing its old refs.
- Because `C_N` creates it, deleting it or letting it expire trips N's P1 completeness check (`E_CONTROL_NOT_LIVE`), so a device cannot be hidden.
- A device delegation is valid if it opens with layout version 2, its signature verifies under N's key, `body.rid = R`, and its creator is one of N's bind control addresses under the P1 creator rule.
- The device key comes from its own seed (`device/seed`, outside `keys/`), not from the identity seed; the identity key only signs the delegation.
- A device cannot be revoked on its own: there is no revoke entity, so a leaked device seed can sign heads for this identity until the identity itself is replaced by an identity revision (P1 section 3.5).
  A machine that also holds `keys/` holds the identity key anyway, so in practice a lost machine means a lost identity key.

### 4.2 Head (`head`)

```cddl
HeadBody = {
  1: bstr .size 32,          ; rid
  2: bstr .size 32,          ; N, the identity this device writes for
  3: uint,                   ; seq of this device's head, 0, 1, 2, ...
  4: bstr .size 32,          ; id_head = h of the identity revision the writer verified
  5: uint,                   ; at = latest block seen just before signing
  6: uint,                   ; pieces_created = entityNonce(C_d) after this tx, minus 1
  7: [* RefEntry],           ; the refs this device wrote (its own writes, section 5.3)
  8: [* bstr .size 32],      ; snapshot: h of each snapshot manifest
  9: [* Pack],               ; packs since the snapshot, in apply order
  10: bstr .size 32,         ; prev_tx = tx that carried this device's previous body, zero at seq 0
  11: uint,                  ; object format: 1 = sha1 (20 B oids), 2 = sha256 (32 B oids)
}
RefEntry = [name: tstr, oid: bstr, counter: uint, stamp: uint, base: bstr]   ; empty oid = tombstone
Pack = [pack_id: bstr .size 32, pieces: [* [tx: bstr .size 32, sha256: bstr .size 32]]]   ; zero tx = this tx
```

- Signed by the device key (envelope `signer = d`, envelope kind code 12, context `arkiv-git/head/v1`).
- Stored with `ag.kind = 'head'`, `ag.v = 1`, `ag.rid`, `ag.nid = N`, `ag.dev = d`, `ag.h = H("head", envelope)`, created once by `C_d` with flags `permissionlessExtension` only (not read-only) and the maximum lifetime, then patched by every push (`ag.body`, `ag.h`).
- Anyone may extend it (keepalive, delegates, mirrors); extends do not change what a reader trusts, because nothing reads `$updatedAt`.
- Ref names follow `refs` (P1 2.2); entries are strictly ascending by name; `counter >= 1`; every non-empty `oid` and `base` has the length field 11 names.
- `stamp = 0` means "the block this body landed in"; a nonzero stamp is the landing block of an earlier body of the same device, copied forward by the writer, and must be at most `at`, so it always lies before this body's own landing.
- Stamps are a claim of the device, not checked against block history: every reader and the program fold use the same stamps, so they cannot make clients disagree, and a device that lies about them can only reorder ties among its own identity's writes, which its key already allows.
- `base` is the merged oid this entry fast-forwards, signed by the writer, or empty when the entry creates the ref, deletes it or is not a fast-forward (section 8).
- The head body has the byte cap of every signed body (`max_body_len`: 65,536 B minus the 3,417 B envelope, so 62,119 B), which its refs and piece list share.
  With sha1 oids and 20-byte names an entry is about 60 B (about 81 B for an entry updated by this body, which carries a base) and a piece about 70 B, so one device can list about 550 refs next to a full 256-piece list and about 900 with few pieces (DERIVED); the v1 refs entity held about 1,100 under the same cap.
  Before format v4 a push past it failed with `E_REFS_TOO_LARGE` and wrote nothing; since then the writer keeps most entries in a ref table named by the signed body (section 14).
- The writer sends a pack inline only when an upper bound of the body plus the 16 KiB of inline bytes fits one transaction (131,030 B); otherwise the pack goes as bulk pieces, so a large head never makes the head transaction too big.
- `pack_id = SHA-256(pack bytes)`; a pack is the concatenation of its pieces in order; each piece is at most 65,536 B.
- Body size (DERIVED): about 200 B of fixed fields, about 55 B per ref (about 77 B with a base), 34 B per snapshot manifest, about 70 B per piece, plus the 3,417 B envelope (44 B framing and the 3,373 B signature); a small repo's head is about 3.8 KiB.
- The head is not padded: a growing body pays storage for the added bytes over its remaining lifetime (about 6,500 gas for 64 B with 170 days left, DERIVED), which costs less than shipping zero padding on every push (3.8).

### 4.3 Pieces (`piece`)

- `ag.kind = 'piece'`, `ag.rid`, `ag.body` = raw bytes (sealed for private repos), created with a 20-block lifetime and never extended.
- Inline pieces sit in the head transaction and are created by `C_d`; bulk pieces are created by `B_d` in transactions that land before the head transaction.
- After expiry a piece is read back from block history: a bulk piece by its tx hash, an inline piece through the head transaction (section 5.4).
- A push with no inline piece (a refs-only push, or a push whose bytes all went to the snapshot or to bulk pieces) still creates one inline marker piece with an empty body, so every head transaction creates at least one entity from `C_d`.

### 4.4 Snapshot

- A snapshot is the self-contained pack (or packs, split at 100 MiB) of every object reachable from every live ref of the identity's merged view after this push (all devices, not only this device's own writes), written with today's `manifest` and `chunk` entities from `B_d`, signed by the device key, 30-day lifetime, kept alive by keepalive.
- The head names its snapshot manifests by `h`; superseded snapshots are simply not extended any more and expire.
- A device writes a fresh snapshot when the identity has no snapshot yet in this repo (a new repo or a migration), when the piece bytes since its last snapshot plus this push would exceed `max(1 MiB, 25 percent of the snapshot)`, when this would be its 64th push since the snapshot, or when the pieces list would exceed 256 entries; otherwise the push goes as pieces.
- On a snapshot push the push's own objects are in the snapshot, so it carries only the marker piece.
- Keepalive extends the snapshot manifests and chunks named by every accepted head of every peer it follows, so a device that stops pushing never lets the objects its head names expire while another device still relies on them.
- Objects another device's snapshot covered can still vanish when that device refreshes onto a smaller set; a clone therefore downloads every listed pack of every accepted head and skips only packs that no live ref of the merged view reaches (they belong to refs deleted since).

## 5. Reading

### 5.1 Discovery, one round of queries

Per synced peer N: the P1 peer document and bind chain, the anchor query (which now returns v1 anchors and device delegations, split by `ag.v`), and `ag.kind = 'head' AND ag.rid = R AND ag.nid = N`.
A peer with at least one valid device delegation is a v2 peer.
Once at least one of its devices has an accepted head, its refs come only from its device heads, and its v1 anchors only count in its P1 completeness check.
A v2 peer whose devices have no accepted head yet (a half-done migration: the delegation landed, the head did not) keeps being read from its v1 anchor chain; an empty merge is never taken as "every ref deleted".
A peer without a delegation is read exactly as in P1.
The head query also filters on `$creator = C_d`, so heads from other creators never reach the client.

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

- A head lists only the device's own writes: every ref it ever set or deleted, with tombstones dropped once no other device lists the name; refs another device wrote are never copied, so one device can never repeat a stale value of another's ref.
- Every entry of every accepted head of N competes for its name.
- Precedence, highest first: the larger `counter`; on a tie, the earlier landing block (`stamp`, with `stamp = 0` resolved to the block the body landed in); on a tie, the smaller device nid.
- The winner of each name is N's value for that ref; a winning tombstone means the ref does not exist.
- Why a counter: a stale copy of an entry can never beat a newer write, because a writer always sets `counter = winner.counter + 1` for the refs it updates.
- Why first-landed wins a tie: two equal counters mean two writes from the same base that did not see each other; the one that landed first already returned `ok` to its user, so the later one must lose, which its re-check reports as `E_LOST_RACE` (section 7.3), exactly like a concurrent push to one GitHub branch.
- Two writes that land in the same block both re-check after that block, so the device-nid tie-break gives every client the same winner and both writers the same verdict.
- Writes to different refs never compete, so two devices pushing different branches concurrently both keep their branch.

### 5.4 Finding the transaction of a body

The landing block of the current body and its tx hash are needed only to resolve `stamp = 0` in a tie and to read inline pieces after they expired, so readers search only when a merge tie needs it or a piece is missing.
The reader fetches blocks `at + 1, at + 2, ...` with full transactions until it finds the transaction from `C_d` with a successful receipt whose patch targets this head's entity key (or whose create made it, in its creation block) and carries exactly this `ag.h`; usually that is the first block.
Because the search starts at `at + 1`, the v1 rule `at < landing` holds by construction.
The search stops at `min($updatedAt, at + 64)`, and a body not found there is unreadable (`E_NOT_LIVE`).
The writer keeps that window: it reads `at` immediately before signing, and if its receipt lands later than `at + 64` it sends a follow-up head at once that carries concrete stamps and tx hashes.
The next body of the same device replaces every `stamp = 0` and every zero piece tx with the concrete block and tx hash, so only the current body ever needs the search.

### 5.5 Clone and fetch

- Download, for every accepted head of every synced peer: the snapshot manifests (query by `ag.h`, creator `B_d`) and their chunks (P1 chunk paging, creator `B_d`), then the pieces of every listed pack not yet applied (which objects a pack holds is known only once it is downloaded).
- A pack that cannot be indexed (its thin bases may have left with a snapshot refresh) is skipped with a warning, and the connectivity check decides whether the live refs needed it.
- When that peer then passes every check, the reader records the pack in `state.json` (`skipped_packs`), keyed by the `h` of every accepted head of the peer, and later fetches do not download it while those heads are unchanged (issue #25).
  This is safe from local data alone: the same heads give the same merged live refs, those refs were complete without the pack when it was recorded, and the cache never drops their objects, so every check reaches the same verdict without the download.
  Any new head of the peer drops the record: the pack is downloaded and tried once more, and recorded again if it still does not index.
  A fresh clone has no record, so it still downloads such a pack once; marking packs unreachable in the signed head would avoid that, but changes the format and is not done.
- Bulk pieces are fetched by `eth_getTransactionByHash`, inline pieces through the body transaction (section 5.4); every piece must match its sha256 in the signed head and every pack its `pack_id`.
- Snapshots are indexed first, then packs with `index-pack --fix-thin`, retried in rounds while progress is made (thin packs only use bases of the same identity, section 7.2).
- The connectivity check, the canonical-ref quorum and the program fold run on the merged refs exactly as on P1 refs.
- A clone therefore costs one snapshot plus at most the piece bytes since it, whatever the history length, and old blocks being pruned loses nothing a user can see once the snapshot exists.

### 5.6 Speculative sync and the head probe

A full sync records every query its view build and program reads sent, with the pages that came back, in `view-memo.json` next to the repo's `state.json`.
`entityNonce` reads and count-only queries (the completeness counts of design 3.3) are never recorded: they are always sent live, in the build's order, so every nonce is still read after the rows it checks.
A recorded page is served at most once per sync, so a second read of the same rows (the full bracket of section 6 after the short form failed) always goes to the network.

The next sync of the repo takes one of three paths, falling back to the next on any failure:

1. Replay, when the last full sync is less than 10 minutes old (`CHARTER_PROBE_WINDOW_SECS` overrides, in seconds) and the probe is unchanged.
   The probe is one paged query for the `(key, $updatedAt)` of every `id`, `idacc`, `bind`, `anchor` and `head` entity of the repo, and, for a repo that pins a program, the `(key, $updatedAt)` of its ops (a count alone misses an extend, or an expiry and a create in one block); both lists go out in one round with the live nonce reads, and the op list adds a round only when it alone runs past one page.
   Any create, delete, patch, extend or expiry of those entities changes the list, so a new push, a rollback patch, a deleted or hidden head, a new device delegation and an identity revision are all seen at once.
   Only pages read at or after the probe's block are replayed (anything a page missed then happened after the probe, so it changes the next probe); the rest are sent live.
2. Speculative: every recorded query is sent at once, and the build is served from those answers; queries it did not record go out as before.
3. The plain build.

A replayed or speculative sync takes the probe's block as the chain head instead of asking the node for it.
With `CHARTER_TRACE=1` each sync prints the path it took.

What a replay can miss until the next full sync (at most the window, 10 minutes by default):

- The deletion or expiry of a v1 `refs` entity, a v1 manifest or snapshot chunks: content-addressed entities whose kinds the probe does not list.
  Chunks are read live by the download anyway; a missed refs or manifest deletion only delays `E_NOT_LIVE` for that peer.
- The deletion or expiry of a `peer` document (it carries no `ag.rid`).
  A peer document only binds a key to a nid that hashes it, so no other key can take its place.
- For a pinned repo, one op deleted and another created in the same window: the op count is unchanged.
- Anything a lying RPC hides, which is no change from the trust model without the probe.

## 6. Completeness and the rollback invariant

Rules for the device control address `C_d`:

- `C_d` creates entities only inside head transactions: the head itself (once) and inline pieces or the marker.
- Every head transaction creates at least one entity from `C_d`.
- `body.pieces_created = entityNonce(C_d) after the transaction - 1` (the 1 is the head itself).

Check, run on every fetch for every device of every v2 peer: read the heads, then `n = entityNonce(C_d)`, and accept the head when `head.pieces_created + 1 = n` (or no head and `n = 0`).
Anything else falls back to the full bracket: read `n1 = entityNonce(C_d)`, then the heads, then `n2 = entityNonce(C_d)`; retry up to 4 times while `n1 != n2`; then require `head.pieces_created + 1 = n1`, else fail closed with `E_CONTROL_NOT_LIVE`.
This is the coordinator's invariant `live control-plane entities of C_d + pieces_created = entityNonce(C_d)` with the head as the only control-plane entity of `C_d`, checked at the latest block, because Rabdi cannot answer for older blocks (section 2).

Why a rollback is caught by a fresh client with no checkpoint and no local state:

1. A key holder of `C_d` (with or without the device key) can patch the head back to any body the device signed earlier, with `pieces_created = p_old`.
2. Between that old body and the newest one, the device sent at least one more head transaction, and each created at least one entity from `C_d`, so at the newest body `entityNonce(C_d) >= p_old + 2`.
3. `entityNonce` never decreases, and the rollback transaction itself can only add creates.
4. So after the rollback `entityNonce(C_d) >= p_old + 2 > p_old + 1`, and the check fails.
5. The same holds for a body replayed into a second head entity: the extra create raises `entityNonce` past every signed body.

Why one nonce read after the heads is enough:

- Along one device's body chain `pieces_created` strictly increases: a push signs only when `entityNonce(C_d)` equals its previous body's `pieces_created + 1` (section 7.1), and its own transaction adds at least one create; the repair of section 7.4 only raises it further.
- Once a body lands, `entityNonce(C_d)` is at least its `pieces_created + 1` forever (step 3 above).
- So a nonce read after the heads that equals the read head's `pieces_created + 1` is below every newer body's `pieces_created + 1`: no newer body had landed when the nonce was read, which is later than the heads were.
- A push landing between the two reads raises the nonce past the read head's claim, so the short form fails and the full bracket decides, exactly as before.
- Rabdi answers `entityNonce` only at the latest block, so the read cannot be pinned to the query's block (`arkiv_query` takes `atBlock`, `eth_call` does not); the short form costs two round trips per device instead of three.

The completeness observation of design 3.3 takes the same short form: the live count of the control address, then one `entityNonce` read, accepted when they are equal, else the full (nonce, count, nonce) observation.
A live count never exceeds what the address created and the nonce never decreases, so a count equal to a later nonce was complete when it was read.
A peer's completeness observations and its device heads are read at the same time and judged in the old order, so a no-op fetch of a v2 peer costs two sequential round trips after discovery instead of four.

An over-claiming body: a body that records more creates than `C_d` had made when it landed, from a buggy or malicious holder of the device seed.

- While it is current every reader fails closed (`entityNonce(C_d) < pieces_created + 1`), and its own device cannot push past it (section 7.1 needs equality).
- It can be rolled back to later only from a newer body that records no more than it does, which honest code never signs (first point above): whoever signed that newer body holds the device key and could sign any refs for this device directly, so the rollback adds nothing to that power.
- The full bracket does not close it either: a holder of `C_d` can create entities until `entityNonce(C_d)` equals the old claim plus 1 and then stop, and `n1 = n2 = pieces_created + 1` holds for the old body.
  The two reads differ only while creates land between them, which the short form already turns into a fallback.
- A lying RPC can report any nonce, with either form.
- Closing it would need the nonce at the old body's landing block, which Rabdi does not serve; readers that saw a newer head reject the old one by seq (section 5.2, rule 5).

Why an honest device never fails it:

- Pieces that must land before the head (big pushes) come from `B_d`, never from `C_d`, so a crash between them and the head leaves `C_d` untouched.
- The head transaction is one atomic `execute`, so the nonce and the body change together.
- The writer computes `pieces_created` from `entityNonce(C_d)` read just before signing, so nothing it ever created is miscounted.
- A push landing between the reads fails the short form and the `n1, n2` bracket decides.

What it costs an attacker who only holds `C_d`: any create from `C_d` outside a head transaction breaks the check for every reader, which is a denial of service, the same power a stolen P1 control key already has (it can let an anchor expire); it can never produce an older or forged view that passes.
Deleting the head is caught too: a delegated device with `entityNonce(C_d) > 0` and no head fails closed.
Deleting or expiring a device delegation is caught by N's P1 completeness check, because `C_N` created it.
Clients also keep the highest seq and body hash seen per device (section 5.2, rule 5).

## 7. Writing

### 7.1 Push

1. `list for-push` runs the fetch sync, so the base is the verified merged view of the own identity, and git refuses a non-fast-forward against it exactly as today.
2. Stage objects in the cache as today.
3. New entries: the device's previous entries that still win their name (with `base` cleared, since it only matters to the body that made the update), plus each update with `counter = merged counter + 1` (or 1), `stamp = 0` and the signed `base`; every `stamp = 0` of the device's previous body becomes its landing block; tombstones are kept only while another device head of N still lists that name.
4. Pack: one thin pack of the objects reachable from the updated tips, excluding every object reachable from the identity's merged live refs before the push; or a snapshot (section 4.4).
5. First write of this device to this repo: one `C_N` transaction with the P1 `peer` and `bind_0` if missing and the device delegation, waited for.
6. Snapshot chunks and manifests, or bulk pieces, from `B_d`, through the pipeline; every receipt must succeed.
7. The head transaction from `C_d`: `[create head]` the first time, `[patch head]` afterwards, plus the inline pieces or the marker; `at` and `entityNonce(C_d)` are read just before signing, and only once the pending nonce of `C_d` equals its mined nonce (no earlier head tx of this device is still in flight).
   It reads the confirmed tx count of `C_d` first, then `entityNonce(C_d)`, and requires `entityNonce(C_d)` to equal what its previous body accounts for (`pieces_created + 1`, or 0 before the first head), else it stops with `E_CONCURRENT_PUSH`: another writer of this device seed would otherwise fork the body chain.
   The head transaction is signed at exactly that confirmed tx count, never at the pending one, and it is never re-signed at a later nonce: when another sender of `C_d` holds that nonce, mined or pending, the push stops with `E_CONCURRENT_PUSH` (section 7.4).
   After `setup` sent the delegation or waited, the writer reads the identity's heads again; a pushed ref whose value there differs from the listing git checked is `E_LOST_RACE`, and the program check runs again on the fresh refs.
   When nothing was sent and no earlier head tx had to land since the `list for-push` sync, the writer plans from the heads that sync verified instead of reading them again; `C_d` is topped up for four head txs at a time, so most pushes send no top-up first.
   Before sending, the writer records the outgoing head under `outgoing/head-pending`; the next push first waits until `C_d` is idle, so a crash after sending never lets a second body be built on the same `prev_tx`.
   The writer records the body it sends in `state.json` (`sent_heads`, per device) before sending it.
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
- A copied `CHARTER_HOME` (two machines sharing one device seed) is the only way to get two writers of one head.
  Both sign from `C_d`, and each signs its head transaction at the confirmed tx count it read just before `entityNonce(C_d)`, so two concurrent head transactions carry the same nonce and only one can land; the other fails with `E_CONCURRENT_PUSH` and is never re-signed at the next nonce.
  A later push reads the landed head first and continues its chain.
  A push refused this way after its bulk pieces landed has already paid for them, so the writer records where each bulk piece landed (`outgoing/bulk-pieces.json`, by the sha256 of its bytes) and the retry names those txs again instead of uploading (issue #26).
  The retry reads each recorded tx back first and reuses it only if it was sent by this device's `B_d` and carries exactly that piece's bytes, which is what every reader will check; anything else is uploaded again.
  Readers take piece bytes from calldata, live or not, so the 20-block lifetime of the piece entity does not bound the reuse; block-history retention does, as for every piece push.
  Bulk pieces come from `B_d`, never from `C_d`, so reuse does not change `pieces_created` or the rollback rule; inline pieces travel in the refused head tx itself and never landed.
  The record costs a first attempt one local file write and no round trip; a successful push deletes it.
  Clients before this rule signed at the pending nonce and could land a second head transaction behind a pending one; `C_d` then created more entities than the current body records and every reader fails with `E_CONTROL_NOT_LIVE`.
  The machine that sent the current body (it is the one recorded in its `sent_heads`) accepts that head anyway, with a warning, and its next push signs a body whose `pieces_created` matches the real `entityNonce(C_d)`, which every reader accepts again.
  Any other machine, including another copy of the same seed that sent a different body, keeps failing closed until that repair lands.
  The repair re-reads the head inside one `entityNonce(C_d)` bracket just before signing and runs only if that head is still the body it sent and planned on; if a copy landed a consistent body after the plan, the push fails with `E_CONCURRENT_PUSH` instead of overwriting it.
  A rollback is not repaired: an older body is not the body the device sent last, and the local mark rejects it as before (section 6).
  If a body still forks (two bodies naming the same `prev_tx`), readers follow only the current head back along `prev_tx`, so the discarded body never enters the merge or the fold, and the rollback check stays sound because every head transaction adds creates.

## 8. Programs, releases, CI

- `charter-program` folds the same events as before: verified ops in Arkiv order, and every delegate's ref updates after the pin.
- For a v2 delegate, the ref updates come from its head bodies: the reader walks each device's bodies back through `prev_tx` (block history) to the first body that landed before the pin's effective block, merges all bodies of the identity per landing block with the rule of section 5.3 (bodies of one block together, exactly as the head merge and the writer's re-check see them), and emits the differences as `RefUpdate`s; v1 anchors before the migration produce events exactly as today.
- Each step of the walk must carry a body of the same device whose seq is one lower and which landed in a strictly earlier block, with a successful receipt; otherwise the fold fails closed.
- Entities and head bodies share one order: by block, the block's entities first in query order, then the merge of that block's head bodies; every client computes the same order, so every client folds the same state.
- For a v2 update, `RefUpdate.old` is the signed `base` of the winning entry, and `ff` is true exactly when the identity's previously accepted value equals that base; the fold never looks at git ancestry for a v2 update, so a fresh clone missing old objects and a long-lived client reach the same verdict.
- The base is the writer's claim, so every fetch checks it where all clients hold the same objects: after the connectivity check, each winning entry of every synced v2 peer must have a base that is an ancestor of its oid (or no base), else that peer's head is refused with `E_BAD_REFS` (a delegate's refusal fails the fetch).
  This holds for every client because the live refs' full history is in every cache after a fetch.
  The fold runs the same check on the winning entries of every body it walks, so a false base that the same device covers with its next body before any client fetched it is still refused with `E_BAD_REFS` by every client that holds both commits.
  A check that git cannot decide (an object is absent) passes; honest bodies never fail it, so clients only differ where one of them holds proof of a forgery, and that client fails closed.
  The check runs on every fold and is never cached, so a fold checkpoint made by a client that lacked the objects cannot hide a later refusal.
  Not covered: a forgery whose forced commit is absent from every client's objects (it can then never be proven, and no live ref reaches it).
- Fetched bodies are cached under the repo directory (`git/arkiv-head-bodies.json`), so each fetch walks only the bodies it has not seen.
- What depends on block history: a fresh client of a pinned repo reads every body since the pin from calldata, which needs Rabdi to serve those transactions (it serves them back to block 1, section 12); the walk is bounded by the pin, so re-pinning a program resets it.
  If that history were pruned, the fold fails closed with `E_CONTROL_NOT_LIVE` instead of guessing.
- A delegate pinned under v1 keeps needing its v1 refs entities after it migrates: the fold reads its last v1 link before the pin and every later one, so `chart keepalive` keeps those refs entities alive (they are not reproducible from the migration body, whose v1 counterpart is a signed refs envelope).
- `chart program status`, `chart release verify`, `programs/ci-gated.rhai`, `scripts/chart-ci.sh` and `scripts/chart-merge.sh` need no change: they consume the fold.

## 9. Private repos

- Piece and snapshot bytes are sealed by `charter-private` exactly like chunks, since they are creates carrying `ag.rid`.
- The head patch is sealed too: its `ag.body` moves into a sealed body under the current epoch key with `ag.ep`, like a sealed create.
- Rows read back from block history (pieces and head bodies) go through the same opening as query rows.
- The epoch keys live in `$CHARTER_HOME/private`, not in the identity seed: a second machine that copied only `keys/` gets `E_NOT_MEMBER` on a private repo (MEASURED in section 12) until that directory is copied too, as in v0.3.0.

## 10. Migration and old clients

- The first v0.4.0 push to a v1 repo migrates it in that same push: the device delegation (the v0.3.0 stopper), the first snapshot of everything reachable from the merged refs, and the first head, whose entries are the peer's v1 head refs with `counter = 1` plus the push's updates.
- No command is needed; other identities migrate on their own first v0.4.0 push, and a repo may mix v1 and v2 peers.
- After the migration, keepalive extends the identity's control plane (old anchors included, because the P1 completeness check counts them), the device heads and the current snapshots, and stops extending v1 refs, manifests and chunks.
- A new repo created by v0.4.0 carries a device delegation from its first push, so v0.3.0 stops on it too.
- An unmodified v0.3.0 client fetching or cloning such a repo reads the device delegation among the peer's anchors and stops with `E_UNSUPPORTED_VERSION ... from a tracked control address has an unsupported version`; it never lists stale refs (MEASURED on Rabdi, section 12).
- The v0.4.0 message for `E_UNSUPPORTED_VERSION` ends with "upgrade charter", for the next format change.
- Every peer is affected, not only the migrated one: v0.3.0 reads the anchors of every peer it syncs (delegates always, others on a peer url), so cloning or fetching the repo stops as soon as any synced peer has migrated.
- When the migrated identity is a non-delegate peer, a v0.3.0 clone of the plain repo url (which syncs delegates only) keeps working and never shows that peer's refs, as before the migration; a v0.3.0 fetch of the peer url `arkiv::<rid>/<nid>`, or with the peer in `extra_peers`, stops with `E_UNSUPPORTED_VERSION`.
  So v0.3.0 never shows a stale v1 ref of a migrated peer; it either stops or does not show that peer at all.
- A v1 peer whose devices have no head yet (a half-done migration) keeps its v1 refs for v0.4.0 readers (section 5.1), and v0.3.0 stops on its delegation.

## 11. Downsides of 3.9 and what a user can still notice

1. Several machines: automatic device heads; nobody runs a command; a concurrent push to the same branch from two machines gives the loser `E_LOST_RACE` as today.
2. Collaboration ops stay entities, so nothing about issues, patches, reviews, programs or queries changes.
3. History availability: the snapshot is a paid, kept-alive entity, so pruned blocks lose nothing visible; only the piece bytes since the last snapshot (at most `max(1 MiB, 25 percent)`) live in block history alone, and a node that pruned those few blocks makes clone fail with `E_NOT_LIVE` until the next snapshot.
4. Rollback: detected by fresh clients without checkpoints (section 6); clients that saw a newer head also reject older ones by seq.
5. Migration: automatic on the first push; v0.3.0 fails loudly on a migrated repo.

What remains noticeable: a snapshot push is slower and costs more (entity chunks at 30 days), about every 64 pushes or 1 MiB of changes; a first push from a new machine adds one transaction (the delegation); a refs-only push still writes one small marker; block-history retention is not a priced promise (section 12 records how far back Rabdi serves it); a pinned program's fold on a fresh client also reads every head body since the pin from block history (section 8).

Known limits not handled in v0.4.0:

- Until format v4 a device's head held at most about 550 to 900 refs (section 4.2) and a push past that failed with `E_REFS_TOO_LARGE`; v4 moves the bulk of a large ref list into a ref table (section 14), which bounds a device at the table cap of 4 MiB stored (tens of thousands of refs).
- A pack that cannot be indexed is downloaded once per change of the peer's heads, and by every fresh clone (section 5.5); until v0.5.0 it was downloaded on every fetch.
- A copied `device/seed` makes two machines one device; they serialise on the confirmed nonce of `C_d`, and a push that loses gets `E_CONCURRENT_PUSH` or `E_LOST_RACE` (section 7.4); the retry reuses the bulk pieces the refused push landed.
- Several devices of one identity pay their top-ups from the one identity wallet, so two devices topping up at the same moment can sign the same wallet nonce (issue #28).
  After v0.5.0 the one that reaches the node second recovers instead of failing: when the node refuses its transfer with `replacement transaction underpriced` (the other transfer holds the nonce in the pool, MEASURED on Rabdi 2026-11-11) or `nonce too low` (it already landed), and the node does not hold the refused transfer, the client reads the wallet's pending nonce again and signs the transfer anew, up to four times; the prefund's wallet pipeline re-reads the nonce the same way.
  The source of truth is the node's pending nonce, so clients with different retry timing never leave a gap; a transfer is only re-signed after the node confirmed it does not hold the first one, so nothing is sent twice.
  Without a collision nothing changes: no extra read or transaction.

## 12. Measurements

MEASURED on Rabdi on 2026-11-11 with the v0.4.0 release build rebased on `main` 611da1f; the comparison with v0.3.0 and the cost table are in `docs/p5/speed-and-cost.md` section 3.15.

- Rollback rule (section 6): the live probe `crates/charter-cli/tests/rabdi_head_nonce.rs` patched a head with its own body (248,540 gas), extended it (43,085 gas) and sent a reverted patch from `C_d`; `entityNonce(C_d)` stayed 16 through all three, so only creates move it.
- Block history: `eth_getTransactionByHash` served transactions back to block 1 (about 11 days of history) on the day of the run.
- Small push (2.7 KB change, warm cache): 11.7 to 13.6 s, one transaction of 340k to 354k gas; the head grows by about 3.5k gas per push as its signed body carries one more entry.
  About 13 s of a push is RPC round trips at 300 to 500 ms each (the fetch sync before the push and the re-check after the receipt); the transaction itself lands in one 2 s block.
- Big pushes: 10 MiB took 72.9 s and refreshed the snapshot (164 transactions, 432M gas, 0.697 tGLM); 2 MiB afterwards stayed on the piece path (34 transactions, 84.9M gas, 0.0947 tGLM, 26.6 s).
- Clone after the pieces expired from the entity store: 18.3 s for the 12 MiB repo, objects read from calldata and the snapshot, `git fsck` clean.
- Two devices of one identity: concurrent pushes of different branches both landed (12.6 s and 14.7 s) and the merged refs listed both; a concurrent push of the same branch gave the slower device `E_LOST_RACE`, and a pull, rebase and push from that device then landed.
- Private repo: first push 31.9 s, next push 12.2 s, clone 4.2 s; no ref name or content in plaintext calldata; a second device with only `keys/` copied gets `E_NOT_MEMBER` (section 9).
- Migration of a v0.3.0 repo: one automatic push of 8 transactions (1,134,393 gas, 38.4 s); v0.3.0 fetch and clone of the migrated repo stop with `E_UNSUPPORTED_VERSION`.
- The re-run on the build with the review fixes (concurrent pushes from a copied seed and from a fresh device, migration under a pinned program) is at the end of `docs/p5/speed-and-cost.md` section 3.15.

## 13. Format v3 (design for v0.5.0, core code on branch `perf/b-lane-b`)

Status: designed 2026-11-11 and wired on `perf/batch-b` the same day: every push writes v3, readers read v2 and v3.
Format v3 changes only the bytes of the device head and of the packs it names; entities, keys, the rollback rule, the merge and the program fold stay as in sections 4 to 8.

### 13.1 Encoding

```cddl
HeadBodyV3 = {               ; fields 1 to 8, 10 and 11 exactly as HeadBody (4.2)
  9: [* PackV3],             ; packs since the snapshot, in apply order
}
PackV3   = [pack_id: bstr .size 32, codec: uint, runs: [+ PieceRun]]
PieceRun = [block: uint, index: uint, count: uint]
```

- Signed by the device key with envelope kind code 13 and context `arkiv-git/head/v3`; the content hash is `H("head3", envelope)`.
- Stored as `ag.kind = 'head'` with `ag.v = 3`, plus `ag.rid`, `ag.nid`, `ag.dev` and `ag.h` as in v2.
- `pack_id = SHA-256(stored bytes)`, the concatenation of the pack's pieces in order, so integrity is checked before any decompression.
- `codec = 0`: the stored bytes are the git pack; `codec = 1`: they are a brotli stream of it (13.3).
- A run names `count` consecutive transactions of block `block` from index `index`; each contributes the bodies of its piece creates in op order.
  `[0, 0, 1]` is the transaction carrying this body (inline pieces).
  Bulk pieces are one create per transaction from `B_d`, so a 2 MiB push of 34 pieces in two blocks is two runs.
- The per-piece SHA-256 of v2 is gone: the pack check already covers every byte, and a piece is useless alone.
- The body rules of 4.2 still hold (strictly ascending refs, counters, stamps, oid lengths, `prev_tx`), plus: every pack has at least one run, every run `count >= 1`, block 0 only as `[0, 0, 1]`, a known codec, and at most 256 piece transactions in all.
- Snapshot manifests of a v3 head use `pack_kind = 3` (a brotli stream of a self-contained pack): `pack_id`, `size` and the chunk hashes describe the stored bytes, `trailer` the git pack inside.

### 13.2 Smaller head transactions

- A head patch sets only `ag.body` and `ag.h`.
  Arkiv applies a patch as attribute mutations merged into the stored entity (`arkiv-reth-executor`, `Op::Patch` with `apply_mutations`; `MemArkiv` does the same), so the other attributes stay as the create wrote them.
  Readers that open a body from calldata (the landing search of 5.4 and the body walk of section 8) accept a row with only those two attributes when `ag.h` matches the envelope (`open_head_calldata`).
- The marker piece of a push with no inline piece is a create with no attributes and an empty payload from `C_d`: it still raises `entityNonce(C_d)`, which is all the rollback rule needs.
  A head tx that creates anything else from `C_d` (an inline piece or a snapshot manifest) sends no marker; `pieces_created` counts every create of `C_d` in head txs.
- Snapshot manifests of up to 32 KiB in all are created by `C_d` in the head tx instead of by `B_d` beside the last chunks; larger ones keep the v2 path.
  Readers and keepalive accept a manifest created by either device address.
  Root cause (MEASURED by lane A, explained here): GolemDB charges `write.index_term_create` (about 2,004 units in the genesis weights) for each index term it does not find, and it looks terms up in the commit the block started from (`IndexJoin.create_term = !reader.origin_term_exists(..)` in `golemdb/crates/record/src/plan.rs`).
  A manifest in the same block as its chunks therefore pays again for terms the chunks create in that block, about 6k gas more than v0.4.0, which sent it a block later.
  In the head tx it lands a block after the chunks, needs no tx of its own and replaces the marker.
- Private repos (a backend that seals) send full head patches: the sealing layer seals only ops naming `ag.rid` and `ag.kind`, and refuses a minimal patch rather than sending it in plaintext.
- ABI framing stays: the node requires the canonical ABI encoding of the precompile call, so the zero padding cannot be cut client-side.

### 13.3 Pack codec

- The writer builds every piece pack and snapshot pack with `--compression=0 --no-reuse-object` (deltas recomputed, objects not zlib-compressed), then compresses the whole pack with brotli (window 2^24).
  Brotli over today's zlib-compressed packs saves under 2%, because git already compresses each object; without the inner zlib it saves 22 to 35% (MEASURED, 13.6).
- Quality by size: 11 up to 64 KiB, 9 up to 1 MiB, 6 above (MEASURED with the Python binding of the same encoder on a loaded Mac):

  | Pack (no zlib) | q6 | q9 | q11 |
  |---|---|---|---|
  | Charter thin, 3 commits, 403 KB | | 125,108 B, 0.05 s | 116,284 B, 1.1 s |
  | Charter thin, 10 commits, 1.62 MB | 470,500 B, 0.11 s | 462,534 B, 0.26 s | 423,585 B, 4.5 s |
  | Charter snapshot, 4.04 MB | 1,263,554 B, 0.28 s | 1,225,900 B, 1.1 s | 1,133,492 B, 12.0 s |
  | focus-dock snapshot (binary), 12.3 MB | 11,322,899 B, 0.33 s | 11,230,582 B, 8.4 s | 11,118,104 B, 55 s |

  Above 1 MiB q9 costs 4 to 25 times the CPU of q6 for 1 to 3% fewer bytes, and q10 and q11 break the budget of 2 s per 10 MiB, so they are used only for small packs.
- When the brotli stream is not smaller than the pack (random data, one-commit pushes), the pack is stored raw with `codec = 0`.
- Readers check `pack_id` first, then decompress with a cap of 100 MiB (the largest pack one manifest can name), so a signed decompression bomb fails with `E_BAD_PACK` instead of filling the disk.
- Brotli is the pure-Rust `brotli` crate already in `Cargo.lock` through `reqwest`; zstd would have added a C binding.
- Private repos compress before sealing: the sealed bytes are the stored bytes, so nothing new is in plaintext and the ciphertext length leaks no more than the v2 length did.
- Reader cache: `index-pack` keeps the pack as given, so the objects of a v3 pack sit in the cache without zlib.
  For text-heavy packs that is about 2.5 times today's size (Charter snapshot: 4,043,666 B in the cache against 1,646,039 B); binary packs barely grow (focus-dock: 12,345,246 B against 12,006,713 B).
  A local recompress of the pack's objects by path (`pack-objects --revs <tips> --no-reuse-object --window=10`) brings the Charter snapshot back to 1,647,895 B in 0.5 s on a loaded Mac; without paths (`--batch-all-objects`) it reaches only 2,550,809 B, and for the binary pack it costs 2.6 s to save 6% (MEASURED).
  Decision (2026-11-11): readers do not repack in the fetch.
  The repack costs about 0.12 s per decoded MiB, and the project rule is that no operation gets slower than v0.4.0; a clone of Charter's repo downloads the same number of chunk pages either way, so the repack would be pure added time.
  The cache of a text repo is therefore about 2.5 times larger than v0.4.0's; disk is not part of the speed and cost rule, and a later maintenance command can recompress off the critical path.
  A device's own snapshot layers keep their indexed pack (the layer policy finds them by git trailer), so the writer never repacks its own snapshot packs.

### 13.4 Migration and old clients

- The first v0.5.0 push of a device migrates its head in that push, with the mechanism of the v1 to v2 migration: the head entity keeps its key, and the migrating patch sets every attribute (`ag.v = 3` included), so the stored entity reads as a v3 head; later patches set only `ag.body` and `ag.h`.
- The migrating body lists the device's carried packs re-encoded as runs: each v2 piece tx hash maps to its `(block, index)` through the receipt the writer already holds or one `eth_getTransactionByHash`.
- v0.5.0 reads v1, v2 and v3 and writes only v3; a repo may mix v2 and v3 devices.
- v0.4.0 meets `ag.kind = 'head'` with `ag.v = 3` from the tracked `C_d`, treats it as an unknown layout version and stops with `E_UNSUPPORTED_VERSION` (`fail_unsupported` in `charter-core/src/chain/head.rs`); it never drops the head or shows stale refs.
  The test `the_v040_reader_stops_on_a_v3_head_with_unsupported_version` runs the unchanged v0.4.0 acceptance code on a signed v3 row.
- A migrating patch that left `ag.v = 1` would make v0.4.0 fail closed with `E_CONTROL_NOT_LIVE` instead (an undecodable head); the writer therefore always sets every attribute on the first v3 patch.

### 13.5 Security impact

- Integrity is unchanged: every stored byte is covered by `pack_id`, which the device signs, and git checks every object after decompression.
- Losing the per-piece hash costs only diagnosis: a corrupt piece is reported for its pack, not for the piece.
- Naming a piece by `(block, index)` instead of its tx hash depends on that block being final; Rabdi has a single sequencer and every piece receipt lands before the head tx is sent, so a reorg that moved a piece would also have to move the head, and a mismatch fails closed with `E_BAD_PACK`.
- The minimal patch changes nothing a reader trusts: every attribute is still compared against the values derived from the body.
- The new signature context and kind code separate v3 bodies from v2 bodies, so a v2 signature can never be replayed as a v3 head or the reverse.
- A rollback to a v2 body after migration still fails the rule of section 6, and the local mark rejects a lower seq.
- The decompression cap bounds memory and disk per pack.

### 13.6 Projected savings (DERIVED from the measurements in `speed-and-cost.md` 3.15)

| Push | v2 | v3 | Saving |
|---|---|---|---|
| Small push, head tx gas | 340k | about 318k | minimal patch -13k, attribute-less marker -9k |
| Head after a 2 MiB push (34 carried pieces) | +2,420 B per push | about +40 B | about -95k gas per push, growing to about -700k at 256 pieces |
| Thin pack of 2 to 30 commits | 100% | 70 to 72% | -28 to -30% of piece gas |
| Snapshot refresh (Charter's own repo) | 1.81 MB | 1.17 MB | -35% (-29% on top of the 13.3 pack settings) |

## 14. Ref tables: heads past the body cap (issue #24, format v4)

Status: designed 2026-11-11 on branch `fix/issues-batch-1`.
Before it, a device whose refs did not fit the 62,119 B head body (about 550 to 900 refs, section 4.2) could not push at all: the push failed with `E_REFS_TOO_LARGE` and wrote nothing.

### 14.1 Goal and budget

- A head whose v3 body fits stays a v3 head, byte for byte: no new field, no new read, no new transaction, so every operation of the performance gate (`speed-and-cost.md` section 7) keeps its exact cost.
- A device with more refs keeps most of them in a ref table stored as bulk pieces, named by hash from the signed head body, and lists only the entries that differ from the table inline.
- A push that changes a few refs of a large device reuses the table and pays only for the changed entries, so a 5,000-ref device pushes at about the cost of a small device.
- The table is rewritten only when the inline difference grows past a bound (14.3).

### 14.2 Encoding

```cddl
HeadBodyV4 = {               ; HeadBodyV3 (13.1) plus field 12
  12: RefTable,
}
RefTable   = [table_id: bstr .size 32, codec: uint, runs: [+ PieceRun], stamp: uint]
TableBytes = [* RefEntry]    ; canonical CBOR, before the codec
```

- Signed with envelope kind code 14 and context `arkiv-git/head/v4`; the content hash is `H("head4", envelope)`; stored as `ag.kind = 'head'` with `ag.v = 4`, plus `ag.rid`, `ag.nid`, `ag.dev` and `ag.h` as in v3.
- A v3 body never carries field 12 (minicbor omits the absent field, so v3 bytes are unchanged), and a v4 body always does.
- `table_id = SHA-256(stored bytes)`, the concatenation of the table's pieces in order; `codec` is 0 (the stored bytes are `TableBytes`) or 1 (a brotli stream of them), as for packs (13.3).
- The table's pieces are bulk pieces of `B_d`: every run has `block > 0` (never `PieceRun::HERE`), at most 64 piece transactions (4 MiB stored), and the decoded table is capped at 16 MiB, so a signed table bomb fails with `E_BAD_REFS`.
- `TableBytes` follow the rules of 4.2 for a ref list: strictly ascending names, `counter >= 1`, oid and base lengths of field 11.
- The effective refs of a v4 body are the table's entries, each table entry with `stamp = 0` taking the table's `stamp` instead, overlaid by the entries of field 7 by name (field 7 wins).
  The effective list must pass every rule of 4.2, including `stamp <= at`; the table's own `stamp` must be at most `at`, and 0 means the block this body landed in, like an entry stamp.
- The merge (5.3), the push re-check (7.3) and the program fold (section 8) read only effective refs, so a v4 head merges and folds exactly like a v3 head that listed the same entries.

### 14.3 Writing

The writer decides from the planned entries (7.1 step 3) before it builds the pack content:

1. When the v3 body fits (an upper bound of the encoded refs, the carried packs and one new pack, with a 1 KiB margin, at most `max_body_len`), it writes v3 exactly as before.
2. Else, when the previous head is v4, its table still lists no name the new entries dropped, and the entries that differ from the previous table (by every field, after its `stamp` substitution) encode to at most 16 KiB (about 250 entries), it reuses that table: the same `table_id`, `codec` and `runs`, with `stamp` made concrete (the previous body's landing block when it was 0), and field 7 lists only the differing entries.
3. Else it writes a new table: every entry with an empty `base` goes into the table and the rest (the fast-forwards of this push, which carry their base, section 8) into field 7, with the table's `stamp` set to 0; when field 7 would still not fit, every entry goes into the table.
   The table is compressed when that is smaller, cut into 64 KiB pieces and sent from `B_d` before the head tx, with the reuse of issue #26 (a retry names the pieces a refused attempt landed).

- The pack choice of 13.1 (inline when the body plus 16 KiB fits one transaction) counts only the inline entries, so a reusing push of a large device keeps its small pack inline.
- Moving between v3 and v4 changes `ag.v`, so that patch sets every attribute, as the migrating patch of 13.4 does; every other v4 patch sets only `ag.body` and `ag.h`.
- A follow-up head (a body that landed late, 7.1) makes its stamps concrete as before, the table's `stamp` included.
- The 16 KiB bound is a cost trade (DERIVED): 16 KiB of extra entries cost about 0.26M gas per push at 16 gas per calldata byte, while rewriting a 5,000-ref table (about 140 KB after brotli) costs about 2.5M gas, so the table is rewritten after at most about 10 pushes that each carry the maximum difference.

### 14.4 Reading

- Readers accept a v4 row like a v3 row (5.2), then read the table's pieces from calldata with `eth_getTransactionByBlockNumberAndIndex` (the pieces of `B_d` only), check `table_id`, decode under the cap and build the effective refs before the merge.
  Pieces outlive their 20-block lifetime in block history, exactly as the packs since the snapshot do (11.3), so the table needs no keepalive.
- Decoded tables are cached in memory by `table_id`, which is safe across devices and repos because the hash names the bytes.
  The program fold keeps each walked v4 body without its table and each table once, by `table_id`, in `git/arkiv-head-bodies.json`.
- Cost: a sync that meets a new or changed v4 head reads the table once per process (one parallel round of `tx_at` calls, about three for 5,000 refs); a v3 head costs nothing new.
  shortcut: the table cache is per process, so a CLI fetch of a changed v4 head re-reads its table; persist it under the repo cache if large devices sync often.

### 14.5 Old clients and migration

- v0.5.0 meets `ag.kind = 'head'` with `ag.v = 4` from a tracked `C_d` as an unknown layout version and stops with `E_UNSUPPORTED_VERSION` (the path of 13.4); it never shows the inline difference as the device's refs.
- v0.5.0 walking a body chain (section 8) back through a v4 body sees envelope kind code 14, which its calldata reader refuses, and stops with `E_NOT_LIVE` instead of folding partial refs.
- Nothing migrates eagerly: a device switches to v4 on the first push whose refs no longer fit, and back to v3 when they fit again; repos whose devices stay under the cap never change format, so v0.5.0 keeps reading them.

### 14.6 Security

- Every table byte is covered by `table_id`, which the device signs, so a reader can neither be shown other refs nor have entries dropped.
- The new kind code and context separate v4 signatures from v3: a v4 body can never be replayed as a v3 body that silently lacks its table.
- The rollback rule (section 6) is unchanged: table pieces come from `B_d`, which `entityNonce(C_d)` does not count, exactly like bulk pack pieces.
- A table whose pieces a node no longer serves makes that device's head unreadable (`E_NOT_LIVE`), as a missing pack piece does (11.3).
