# P3: collaborative-object ops

Status: implemented (core, sync, CLI), hackathon scope.
Date: 2026-11-07.
Builds on `docs/p1/design.md` sections 2.1 (envelope, attributes), 2.2 (the reserved `cob` kind) and 3.2 (ordering).

## Model

Issues, patches, reviews, attestations, payments and votes are all collaborative objects (COBs).
Each object is a sequence of signed ops; programs fold the verified ops of an object into its state later.
The design mirrors Radicle COBs, but Arkiv creation order replaces the git change DAG as the ordering source.
This layer checks authorship and causality only; who may do what is the folding program's decision.

## Op format

An op is a `cob` entity (kind code 9) whose envelope body is canonical CBOR:

```cddl
CobOpBody = {
  1: bstr .size 32,      ; rid
  2: tstr,               ; type_name, 1..64 B of [a-z0-9._-], e.g. "xyz.arkiv.patch"
  3: bstr .size 32,      ; object id, 32 zero bytes iff action = "create"
  4: [* bstr .size 32],  ; parents: ids of ops of the same object this op saw, may be empty
  5: tstr,               ; action, 1..32 B of [a-z0-9._-]
  6: bstr,               ; payload: one canonical CBOR value, at most 65,536 B
  7: uint,               ; ts_ms, the author's clock, display only
}
```

- Keys 1 to 4 follow the P1 reservation (rid, type, object, parents); the reserved `[op bstr...]` list became one op per entity with an `action` and a `payload`.
- The op is signed in the standard envelope with the author's composite identity key, signature context `arkiv-git/cob/v1`.
- The op id is the standard content hash `H("cob", envelope bytes)`.
- A create op has object = zero, action `create` and no parents; the object's id is the create op's id.
- The payload may hold integers, byte and text strings, arrays, maps, booleans and null, nested at most 32 deep.
- Floats, tags, undefined, simple values and indefinite lengths are rejected, so every payload has one byte form.
- The payload cap is 65,536 B, but the whole envelope must also fit the 65,536 B `ag.body` cap, so with ML-DSA-65 the usable body is 62,183 B.

## Attributes

| Name | Type | Value |
| --- | --- | --- |
| `ag.kind` | `str` | `cob` |
| `ag.v`, `ag.body` | as in P1 | layout version, envelope bytes |
| `ag.rid` | `bytes32` | field 1 |
| `ag.nid` | `bytes32` | author (envelope signer) |
| `ag.h` | `bytes32` | op id |
| `ag.cob` | `bytes32` | object id: field 3, or the op id for a create op |
| `ag.type` | `str` | field 2 |
| `ag.action` | `str` | field 5 |

- Nine attributes, well under the 32-attribute limit; the longest string is 64 B, under the 128 B limit.
- Setting `ag.cob` of a create op to its own id lets one `ag.cob = X` query return the create op and every follow-up.
- As in P1, attributes are query hints only: a decoded op whose attributes differ from the ones derived from its body is ignored.

## Writing

- `charter-sync::cob::post` validates the body, signs it and sends one create op from the author's control address, since `cob` is a control-plane kind.
- A writer with no bind for the repo gets its `peer` and `bind_0` entities in the same tx, which also makes it a known identity.
- Funding goes through the existing rolling top-up from the wallet.
- The entity lifetime is the control lifetime (`CHARTER_CONTROL_LIFETIME`, default 182.5 days).
- An invalid or oversize op fails with `E_BAD_FORMAT` before anything is sent.

## Verification

`charter-core::verify_ops` takes the observed entities and the known peer keys and returns `Vec<VerifiedOp { id, author, body, arkiv_order }>`.
It walks the entities in Arkiv creation order and drops, never errors on, any op that:

1. fails decoding, the canonical re-encode check, the field rules or the attribute check;
2. carries another repo's rid;
3. is signed by a nid with no valid self-signed `peer` document (any peer counts, delegates or not);
4. has a signature that does not verify under that key;
5. repeats the id of an op already accepted (a third party re-posting signed bytes adds nothing);
6. is not a create op and its object has no earlier accepted create op;
7. has a type_name different from its object's create op;
8. names a parent that is not an earlier accepted op of the same object.

- Rule 8 drops parents from other objects, parents from later ops, and parents that were themselves dropped.
- Ordering is Arkiv creation order (block, then rank in the creation-ordered query result, then entity key), the same source anchors use; `ts_ms` never orders anything.
- `arkiv_order` compares ops within one `list` result; ranks are not comparable across separate queries.
- Arkiv is trusted for ordering and availability only; content, authorship and causality come from the signed bytes.

## Reading

- `charter-sync::cob::list(rid, object, type_name)` queries `kind = cob AND rid [AND ag.cob] [AND ag.type]` with the standard offset paging (100 rows per page, count check), resolves each signer's `peer` document, and returns `verify_ops` of the result.

## CLI

- `chart op post --repo <rid> --type <t> --action <a> [--object <id>] [--parent <id>...] --payload-json '<json>'` converts the JSON to canonical CBOR and prints the op id in hex.
- JSON floats (including `1.0`) are rejected with `E_BAD_FORMAT`; map keys are sorted by their encoded bytes.
- `chart op list --repo <rid> [--object <id>] [--type <t>] [--json]` prints the verified ops in order, one line each, or one JSON array.
- In JSON output, byte strings show as `{"$bytes": "<hex>"}`, non-text map keys as their JSON text, and integers outside the i64 and u64 ranges as decimal strings.

## Labels, assignees and milestones (issue #40)

An issue or patch carries labels, assignees and one milestone, folded in Arkiv order from its create op and five follow-up actions on the same object:

| Action | Payload | Effect |
| --- | --- | --- |
| `label` | `{ labels: [tstr] }` | adds the labels |
| `unlabel` | `{ labels: [tstr] }` | removes the labels |
| `assign` | `{ assignees: [peer id hex] }` | adds the assignees |
| `unassign` | `{ assignees: [peer id hex] }` | removes the assignees |
| `milestone` | `{ milestone: tstr / null }` | sets the milestone; `null` or `""` clears it |

- A create payload may carry `labels`, `assignees` and `milestone` as the initial values.
- Readers check every value from the chain: a label or milestone is trimmed, 1 to 48 (label) or 64 (milestone) characters with no control characters, an assignee is 64 lowercase hex digits, and only the first 16 values of one op count; a thread keeps at most 64 labels and 64 assignees; anything else is dropped.
- Labels are free text: there is no repo-level label set, and the labels already in use are the vocabulary; a milestone is a name with no due date.
- Who may label or assign is a program rule, like who may close an issue: the UI shows every valid op, and a pinned program may fold only the ones it allows.
- Old readers fold these ops as ordinary entries of the thread and ignore them for its state, so the change is safe for them; no format version changes.
- `chart op post --action label --payload-json '{"labels":["bug"]}' ...` and the MCP `op_post` tool write them; the UI shows labels, assignees and the milestone on lists and threads, filters lists with `?label=`, `?assignee=` and `?milestone=`, and its edit form signs one op per kind of change.
- Cost: one op (one transaction) per kind of change; reading costs nothing new, since the fold uses the ops the list already reads.

## Signing keys and signed commits (issue #61)

Git commits signed with an SSH Ed25519 key (`gpg.format = ssh`) are checked in process, from the commit object in the local cache, with no `ssh-keygen` or `gpg` on the read path.
A signing-key claim names the Charter identity behind a key: an `xyz.arkiv.signing-key` create op with payload `{ key, sig }`.
`key` is the `ssh-ed25519 <base64> [comment]` line, and `sig` is an armored SSH signature (OpenSSH `PROTOCOL.sshsig`) by that key under the namespace `charter-signing-key` over `charter signing key for <author nid hex>` and a newline.
The op's composite signature says who claims, and `sig` proves the claimer holds the private key, so nobody can claim another person's key or replay someone else's proof.
The first valid claim of a key in Arkiv order wins; a later claim of the same key is ignored, and a `revoke` op by the claimer on the claim object frees the key.
Claims are per repo, like every op, and in a private repo they are sealed like any other op.
A commit then shows one of: verified and signed by the claimer, a valid signature by an unclaimed key, a bad signature, not checked (OpenPGP, X.509 or an SSH key type other than Ed25519), or unsigned.
OpenPGP is not checked because an in-process verifier (`pgp`, rPGP) would add a large dependency tree; `ssh-key` was not used because its stable release depends on another `ed25519-dalek` major version, so the SSHSIG check is about 80 lines on the `ed25519-dalek` and `sha2` already in the tree.

- `chart signing-key claim --repo <rid> --key <private key file>` signs the claim with `ssh-keygen -Y sign`, checks it as every reader will, and posts it; it prints the op id.
- `chart signing-key verify --repo <rid> <commit>` checks a cached commit and prints who signed it; a bad signature exits with `E_BAD_SIG`.
- The UI commit page shows the result in its "Git signature" row, with the key's `SHA256:` fingerprint; it reads the repo's ops only for a signed commit.
- Cost: a claim is one op (one transaction); checking a commit costs no query, because claims come from the ops the last sync verified.

## Inline review comments (issue #38)

A review `comment` or `approve` op may carry `comments`, each anchored to one line, or a range of lines, of the patch diff at the op's `head`:

```text
{ patch, head, body?, comments: [ { path, line, side?, end_line?, body, suggestion? } ] }
```

- `side` is `new` (the default: a line of the patch head) or `old` (a line of the base); `line` and `end_line` are 1-based, `end_line` defaults to `line` and may be at most 199 lines past it.
- `suggestion` is replacement text for those lines of the head, so it exists only on the new side; the UI shows it as a small diff of the lines it replaces.
- All comments of one review go in its one op, so a review with ten comments is still one transaction.
- Readers check every comment from the chain: at most 32 per op, a path of 1 to 1,024 bytes that is relative and has no control characters, line numbers from 1 to 10,000,000, a body and a suggestion of at most 8,000 characters, and a body or a suggestion present; a bad comment is dropped and the rest of the op still counts.
- A comment shows under its line when it was made at the patch's current head and the diff shows that line; otherwise it is listed as outdated.
  Charter does not move comments across versions, since that needs a line mapping every client would have to compute the same way.
- The diff is the patch head against its merge base with the create op's `base`, else with the default branch; a patch already merged shows its head commit's own diff.
- Old readers show such an op as an ordinary review with its body and ignore `comments`, so the change is safe for them; no format version changes.
- The UI's files page (`/r/<rid>/patches/<id>/files`) shows the diff with the comments, and a "+" on any line opens a box for a comment and, on the new side, a suggestion; every box filled in goes out with the review form at the bottom, in one op.
  The conversation page lists each review's comments, linked to their lines.
- `chart op post` and the MCP `op_post` tool write them with the payload above.
- Cost: a review stays two ops (its create and its verdict), whatever the number of comments; the files page reads only the local git cache and the ops the last sync verified.

## Not in this version

- Keepalive does not extend `cob` entities yet, so ops expire after the control lifetime unless someone extends them.
- No completeness check: a withheld or expired op is simply absent, and ops that cite it as a parent are dropped.
- No creator rule: an op is valid whichever EVM address created the entity, because the signature carries authorship.
- No P2 encryption of payloads.
