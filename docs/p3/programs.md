# Repo programs

A repo program is a small Rhai script that decides which ref updates a repo accepts and what the repo owes.
The identity doc pins the script by hash, so every honest client runs the same program.
Each client folds the program over the same Arkiv-ordered ops and ref updates, so all of them compute the same state and accept the same branch heads.
The engine lives in `crates/charter-program`; it is pure and does no I/O.

## Pinning

The identity body has an optional field `13: program`, a map `{ 1: engine, 2: hash }`.
`engine` is the engine tag; the only tag is `rhai/1`, and any other tag fails to load.
`hash` is `H("program", source)`, the tagged SHA-256 of the exact UTF-8 source bytes (design section 2.1).
A body without a pin omits key 13, so its canonical CBOR, its `h` and its signatures are unchanged.
Changing the pin is an ordinary identity revision and needs the usual delegate quorum.
`chart program pin --repo <rid> --file <path.rhai>` compiles the script, posts its source as an op, then proposes a revision with the new pin.

## Source distribution

The source travels as a collaborative-object op (docs/p3/cob-ops.md) of type `xyz.arkiv.program` and action `publish`, with payload `{ engine, source }`.
Publish ops hang off one program object per repo; `program pin` creates that object with a `create` op the first time.
A client uses the source of the first verified publish op whose `engine` equals the pinned engine and whose `Program::hash(source)` equals the pinned hash.
Anyone may post a publish op, because the hash, not the author, decides which source counts.
A pin with no matching source, a source that does not compile, or a fold that fails (for example in `init`) is `E_PROGRAM_MISSING`.
The client then refuses to compute heads at all, so fetch and clone fail instead of falling back to the plain delegate rule.

## Determinism

The engine is built without floats (`no_float`, `only_i64`), without time (`no_time`), without custom syntax and without modules (`no_module`), so there is no `import`, no module resolver and no file access.
`eval` is a disabled symbol, so a script that uses it fails to compile.
`print` and `debug` write to the fold's log, never to stdout.
Rhai maps are `BTreeMap`s, so map iteration and `keys()` are in sorted key order, and the JSON state has sorted keys.
The only host function is `sha256_hex(string or blob)`, which returns lowercase hex and is pure.
Each fold uses a fresh engine.

## Limits

Each ABI call may run 200,000 operations, and a whole fold may run 5,000,000.
Strings are capped at 64 KiB, arrays and maps at 10,000 entries, call depth at 32 and expression depth at 64 (32 inside functions).
A call that hits a limit fails like any other runtime error.

## ABI

All ids and oids reach the script as lowercase hex strings.
Integers are `i64`; a value that does not fit makes the event fail.

### `fn init(repo)` (required)

`repo` is `#{ rid, delegates: [hex ids], quorum, name }`.
It returns the initial state, any value, usually a map.
A program without `init` fails to load, and an error in `init` fails the fold.

### `fn on_op(state, op)` (optional)

`op` is `#{ id, author, type, action, object, parents: [..], payload, order, ts, proofs: [..] }`.
`proofs` holds the client's verification of the op's proof attachments, in payload order (see "Proofs" below); it is empty for an op without any.
`payload` is the op's canonical CBOR converted to Rhai values: maps with string keys, integers, strings, booleans, arrays, byte strings as blobs, and null as `()`.
It returns the new state.
An error, including running out of operations or a payload that does not convert, skips the op, logs it and leaves the state unchanged.

### `fn check_push(state, push)` (optional)

`push` is `#{ author, ref, old, head, order, ff }`.
`old` is the hex oid of the last accepted head of that ref, or `""` if the program has accepted none.
`head` is the new oid; Rhai reserves the word `new`, so the key is `head` even though the Rust and JSON field is `new`.
`ff` is true when the ref has no accepted head, when `head` is empty (a delete) or equals the accepted head, or when the accepted head is a git ancestor of `head`.
Ancestry comes from a host callback (`Program::fold_with`); when the host cannot tell (objects missing, or `fold` and `fold_from`, which pass no callback), `ff` falls back to whether the pusher's claimed `old` equals the accepted head.
For a push from a format v2 head (`docs/p5/head-model.md` section 8) the host never uses ancestry: `ff` is whether the accepted head equals the `base` the pusher signed for that ref (the program's accepted head or the merged value its push fast-forwards), so a fresh clone missing old objects and a long-lived client always reach the same verdict.
It returns `()` to allow a fast-forward, `true` to allow the push even if it is not a fast-forward, `false` or a string to reject, where the string is the reason.
An error, including running out of operations, rejects the push (fail closed).
Without `check_push` every fast-forward is allowed.

### `fn on_push(state, push)` (optional)

It runs after a push is accepted and returns the new state.
An error logs and leaves the state unchanged.

### `fn obligations(state)` (optional)

It runs once after the last event and returns an array of maps, for example `#{ kind: "pay", to, amount, reason }` or `#{ kind: "release_key", object, by }`.
An error logs and yields no obligations.
Obligations are claims the program computes; whether they were met on chain is checked outside the program.

## Proofs

Programs decide whether proofs are required; clients only verify them.

### Attaching a proof

An op payload may carry `proofs: [ { kind: "risc0", receipt, chunks: [..], public } ]`.
`public` is the statement the receipt claims to prove, for a sealed patch the `arkiv_git_zk_core::Public` map `{ key_hash, ct_hash, base_hash, spec_hash, spec }` with hex hashes.
A receipt (about 224 KB) does not fit the 64 KiB op payload cap, so its bytes travel in `xyz.arkiv.blob` ops and the proof entry references them by hash.
A blob op has action `create` and payload `{ data: <hex> }`, at most 30,000 raw bytes per op (`proofs::BLOB_CHUNK`), so `op post --payload-json` can write it.
`chunks` lists the sha256 hex of each chunk's raw bytes in order, and `receipt` is the sha256 hex of the whole receipt.
A client concatenates the blob data whose hashes are listed and requires the result to hash to `receipt`.
Blobs are content addressed, so anyone may post them and a junk blob cannot displace a real one.
Inline receipt bytes are not supported, since every real receipt exceeds the cap.

### What the program sees

Before folding, `program::enforce` runs `program::proofs::attach`, which verifies every attachment with `charter_zk::journal`: the receipt must verify against the pinned guest image id and its journal must equal `public` exactly.
Each op then carries `op.proofs = [ #{ kind, image_id, ok, public } ]`.
`ok` is true only for kind `risc0` with all chunks present, a matching receipt hash, a valid receipt and an exact statement match.
`image_id` is the hex image id the client verified against (`charter_zk::image_id_hex()`), empty for other kinds.
`public` is the claimed statement as posted; trust it only when `ok` is true.
`public` is a reserved word in Rhai, so scripts read it as `pr["public"]`.
Receipt bytes never reach Rhai; the payload keeps only hashes and the statement.
Verification is deterministic, so every client computes the same `ok` from the same ops.
Results are cached by receipt hash in `arkiv-proofs.json` inside the repo's git cache, so a re-fold does not verify again.
A missing chunk is not cached, because it may be posted later; the op then folds with `ok == false` until it is.

### The policy pattern

`sealed-patch.rhai` shows the pattern.
An `xyz.arkiv.issue` create op sets `proof: "required" | "optional" | "none"` and the `spec_hash` it accepts; an issue without `proof` takes `default_proof()` from the top of the script.
A sealed `commit` op that names the issue (`issue: <issue id>`) counts when proof is required only if some entry has `ok == true`, the pinned `image_id`, `public.spec_hash` equal to the issue's `spec_hash` and `public.key_hash` equal to the commitment.
Matching the statement is essential: `ok` alone would accept a valid proof of a different statement.
"optional" accepts the commit either way and records `proven: true | false`; "none" ignores proofs.
`crates/charter-sync/tests/proofs.rs` drives this with the committed real receipt: a match is accepted, and a missing proof, a tampered receipt and a valid receipt for another spec are rejected.

## Fold semantics

Events are ops and ref updates interleaved in Arkiv order; an event with a smaller order than the one before it fails the fold.
The engine keeps the last accepted head per ref.
A rejected push does not move that head, so later pushes on the ref are checked against the last accepted one.
A push that is not a fast-forward (see `ff`) is rejected with `not a fast-forward of the accepted head`, unless `check_push` returns `true`.
Accepted and rejected pushes in the outcome carry the accepted head as `old`, not the pusher's claim.
A push with an empty `new` deletes the accepted head of that ref.
The outcome holds `state_json`, the accepted and rejected pushes (with reasons), the obligations and the log lines.

## Enforcement

Every client enforces the pinned program; there is no server to trust.
The sync crate (`crates/charter-sync/src/program.rs`) builds the fold input from what P1 already verifies.

### Events

Ops are the verified COB ops of the repo, converted to `ProgramOp`.
Ref updates come from the verified anchor chains of the delegates of the current identity head, the same peers whose refs vote under P1.
Each anchor after the first contributes one `RefUpdate` per ref whose oid differs from that delegate's previous signed refs: `old` is the previous oid (empty for a new ref) and `new` the signed one (empty for a deleted ref); the fold then replaces `old` with its accepted head.
Pushes by non-delegates are not events, as under P1 they never become canonical.
Ops, anchors and identity entities are read in one creation-ordered query, so their ranks in that result give one Arkiv order for all events.
The ranks differ between clients only by entities that expired or were created later, never in relative order, so every client folds the same sequence.

### Seeding

The program applies from the identity revision that set the current pin, at the position where that revision took effect.
Pushes before that position are accepted as they were: the fold starts with the canonical heads (design 3.8, under the pinning revision's delegates and threshold) computed from each delegate's last signed refs before the pin.
Ops before the pin are folded normally, so a patch or a bounty opened before the pin still counts.
Only the refs entities of the last pre-pin push and later ones are read, so older refs entities may expire without breaking the fold.

### Fetch and clone

`fetch::sync` builds the verified view, then calls `program::enforce`, which folds the program and stores the result in `View::program`.
`fetch::listing` then lists the program's last accepted head of every ref instead of the delegate quorum result.
A rejected update never moves a canonical head, so every client ignores it, including pushes forced through a modified client.
The peer namespaces `refs/namespaces/<nid>/` still mirror each delegate's signed refs, so the rejected commits stay inspectable.
`chart program status --repo <rid>` prints the state JSON, the canonical heads, every rejected push with its reason, and the obligations.

### Push

`git-remote-arkiv` prepares the push locally, then calls `program::check_push`, which folds again with the would-be ref updates appended after everything on chain.
If the program rejects one of them, the helper discards the prepared push and fails with `E_PROGRAM_REJECTED: <reason>` before any transaction is sent, so no gas is spent on a push the program would ignore.
The check applies to delegates only, because only their pushes are events.
The check is advisory for the pusher; the binding rule is the fetch-side fold that every other client runs.

### Escrow hook

`charter_sync::program::obligations(arkiv, rid)` fetches the repo into the cache under `$CHARTER_HOME` and returns the obligations of the pinned program (empty without a pin); `obligations_in(arkiv, repo)` takes an explicit cache.
An obligation may carry `due_order`, the `order` of the event that made it due; clients add `since_block`, the Arkiv block of that event, unless the program set one.
`bounty.rhai` emits `#{ kind: "pay", to, amount, reason, key: "bounty:<issue id>", purpose: "bounty:<issue id>", due_order }`, which `chart escrow run` pays from deposits with that purpose (docs/p3/escrow.md).

### Private repos

A private repo enforces its program like a public one, for its members.
`op post`, `op list`, `program pin`, `program status` and the escrow's obligation reads detect a private repo the way the remote helper does (`charter_private::ring_for`) and then run over the same `Sealed` adapter (the CLI's `for_repo!` macro).
Ops, the program source op and the identity revision that sets the pin are therefore sealed under the current epoch key, with `ag.rid` blinded, exactly like pushed entities.
Members open them, fold them, and `program::enforce` in fetch sees them, so the helper refuses a rejected push with `E_PROGRAM_REJECTED` and a member clone gets the program's heads.
Non-members cannot read the program, its state or the ops; `program status` fails for them with `E_NOT_MEMBER`.
The op type (`ag.type`) and object id (`ag.cob`) stay plaintext attributes, as for any sealed entity that carries them.
`crates/charter-cli/tests/private_programs.rs` covers enforcement, a member clone and the absence of the plaintext rid on every op entity.

### Replay protection

The program changes only which head is canonical; it does not touch the P1 checks.
Anchor chains, high-water marks and `E_STALE_HEAD` still apply per peer before the fold sees any update, so a withheld or replayed anchor fails the fetch exactly as without a program.
A rejected push still advances its author's anchor chain and mark; the next push of that delegate builds on it as usual.

### Known limits

`ff` uses git ancestry in the local cache: fetch decides it after applying every delegate's packs, `program status` and `obligations` fetch first, and the helper checks after staging the push, so all of them have the objects.
A delegate whose push was rejected is therefore not stuck: its next push of a descendant of the accepted head is a fast-forward, whatever its own refs held.
A client that lacks an object falls back to comparing the pusher's previous oid with the accepted head, which can disagree with clients that have it; P1's connectivity check makes this a corner case.
Every client re-folds the whole history on each fetch; there is no cached state.

## Rust API

`Program::load(engine_tag, source)` compiles a program and checks for `init`.
`Program::hash(source)` gives the pin hash.
`Program::fold(repo: RepoInfo, events)` takes `Event::Op(ProgramOp)` and `Event::Push(RefUpdate)` and returns an `Outcome`.
`Program::fold_from(repo, seed, events)` starts the accepted heads at `seed` instead of empty; `Outcome::heads` holds the last accepted head per ref.
`ProgramOp` mirrors the verified COB op: `id`, `author`, `rid`, `object`, `type_name`, `action`, `parents`, `payload` (canonical CBOR), `ts_ms`, `arkiv_order`, plus `proofs` (`Vec<ProofCheck>`, filled by the client before the fold; optional in dry-run JSON, so `program check` can test a policy without receipts).

## Dry run

`chart program check --file <path.rhai> --events <events.json>` folds a local file and prints the outcome as JSON.
The events file is `{ "repo": { rid, delegates, quorum, name }, "events": [...] }`.
Each event has `"event": "op"` or `"event": "push"`.
An op has `id`, `author`, `object`, `type`, `action`, `payload` and `arkiv_order`, plus optional `rid`, `parents` and `ts_ms`.
Its `payload` is plain JSON, converted to canonical CBOR (floats are rejected).
A push has `author`, `ref`, `old` (optional, empty for a new ref), `new` and `arkiv_order`.
All ids and oids are hex.

## Templates

The `programs/` directory holds six demo programs, each tested from disk in the program crate.

- `branch-protection.rhai`: `refs/heads/main` moves only to a patch head that at least `repo.quorum` delegates approved at that exact head; a revise clears the approvals.
- `ci-attested.rhai`: `main` moves only to a commit that k of the n attesters listed at the top of the script reported as passing CI.
- `ci-gated.rhai`: `main` moves only to a patch head with `repo.quorum` delegate approvals and k passing CI attestations from the listed runners, both at that exact head; Charter's own repo runs it (docs/p3/arkiv-ci.md).
  It also tracks signed release records and their artifact attestations as state (docs/p3/releases.md).
- `bounty.rhai`: an issue bounty plus pledges becomes a `pay` obligation to the patch author once the merged patch head is accepted on `main`, and a payment receipt op clears it; the receipt's chain check happens outside the program.
- `sealed-patch.rhai`: a commit op pins `sha256(key)`, a later reveal counts only if the key hashes to it, and unrevealed commitments are `release_key` obligations.
  An issue may require, allow or ignore a zero-knowledge proof for commits that name it (see "Proofs").
- `agent-bounty.rhai`: `bounty.rhai` with scoped AI agents: a delegate grants an agent key scopes, a payout cap and an expiry, revokes it with another op, and agent patches need a human approval and stay under the cap (docs/p3/agents.md).

Rhai functions cannot read top-level constants without modules, so configuration such as the attester list is written as small functions at the top of the script.
