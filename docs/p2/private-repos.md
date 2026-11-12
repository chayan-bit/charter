# P2: private repos

Date: 2026-11-07, hardened 2026-11-11 (format 2, charter 0.6.0).
Crate: `crates/charter-private`.

## What a private repo is

A private repo stores its packs, refs, anchors, manifests, identity and binds on Arkiv only as ciphertext.
Members agree on keys through an MLS group (RFC 9420) on OpenMLS.
Removing a member is an MLS commit, which starts a new epoch with a new content key.
Push, clone and every core verification rule run unchanged: sealing happens at the Arkiv boundary.

## Ciphersuite

`MLS_128_MLKEM768X25519_CHACHA20POLY1305_SHA384_MLDSA44`, OpenMLS code point 0x0052 (draft-ietf-mls-pq-ciphersuites-07 requests 0x0020).
Key agreement is the X-Wing hybrid KEM (ML-KEM-768 + X25519), signatures inside MLS are ML-DSA-44.
OpenMLS is pinned to main at `28369d5b48608bef4ddf8371d91503b0f3852bb3` with `draft-ietf-mls-pq-ciphersuites`, on the libcrux provider, as gated in `docs/p0/openmls-xwing.md`.
The release 0.9.2 is not used because it carries RUSTSEC-2026-0331.
ML-DSA-44 inside MLS is not yet confirmed, so the suite is the single constant `charter_private::SUITE`, and moving to 0x004E (Ed25519 signatures) is a one-line change.
The suite code point is recorded in the body of the repo's `group` marker entity.

## Keys

The epoch content key is `export_secret("arkiv-git content", rid, 32)` of the current epoch.
The id-blinding key is `export_secret("arkiv-git id", rid, 32)`.
Generation 0 comes from the creating epoch, and every removal starts a new generation from the epoch it creates (`Keyring::id_rot`).
An entity is blinded with the generation of its epoch, and a query for the rid or a name is an `Or` over every generation the member holds, so history keeps matching.
Members keep every epoch key they ever held in a keyring file, `$CHARTER_HOME/private/<rid hex>.json`, mode 600.
OpenMLS state (the ML-DSA-44 signature key, key package secrets and groups) lives in `$CHARTER_HOME/private/mls.json`, mode 600.
Key material is zeroized on drop, and the buffers that load and save the keyring and the MLS state are zeroized after use.
The history key is `export_secret("charter history", rid, 32)` of an epoch; only members of that epoch derive it, and nothing published reveals it.
Each commit carries a lockbox with only what the members applying it lack: the new epoch key, its id-key generation, the roles, role-history entry and admin set of that epoch, and the pinned heads, sealed under the new epoch key (`Keyring::delta`).
Each welcome carries the whole keyring (id keys, all epoch keys so far, the role history, the admin set of each epoch, commit blocks and pinned heads) sealed under the history key of its epoch, so a new member can read history.
So a published grant for epoch E, which opens E's commit lockbox, opens no other epoch, and opens no welcome.
A removed member cannot open either lockbox, because it cannot derive the new epoch's keys.
Lockboxes written before 0.6.0 hold the full keyring under the epoch key, so a grant for such an epoch still opens the epochs before it.
The lockbox is sealed with the AAD kind `box`, and the welcome attestation with `att`; both are reserved, so no repo entity can be sealed under the same AAD.

## Sealing

Any create op whose `ag.rid` is the repo's rid is sealed in `Arkiv::seal_ops`, which the tx pipeline calls before pricing and signing.
The attributes `ag.body`, `ag.rid`, `ag.name`, `ag.lang`, `ag.lic`, and any other `bytes32` attribute equal to the rid (the root identity's `ag.h`, revision 1's `ag.parent`) move into one sealed `ag.body`.
In a format-2 repo `ag.pack` moves into the sealed body too, and the entity gains `ag.pack = HMAC-SHA-256(id_key, "pack" || 0x00 || pack)`.
The sealed body is `nonce || XChaCha20-Poly1305(key, nonce, u32be(len) || plaintext || zeros)` with a random 24-byte nonce.
The AAD is `rid || kind name || u64be(epoch)`.
The plaintext is padded to a power-of-two bucket from 256 B to 64 KiB.
Above 64 KiB (full chunks and near-maximal envelopes with their few hidden attributes) the bucket is the next 4 KiB multiple, because a doubled 128 KiB bucket would break the 128 KiB attribute and tx caps.
The entity gains `ag.rid = HMAC-SHA-256(id_key, "rid" || 0x00 || rid)`, `ag.name = HMAC-SHA-256(id_key, "name" || 0x00 || name)` when it had a name, and the plaintext epoch `ag.ep` so readers pick the right key.
`ag.kind`, `ag.v`, `ag.nid`, `ag.h`, `ag.seq`, `ag.at` and `ag.i` stay in plaintext for now (see TODO).
The `Sealed` adapter rewrites queries for the rid, names and packs to their blinded values and opens rows before the sync layer sees them.
An `ag.pack` query is `Or(plain, blinded)`, so chunks sealed before the upgrade still match.
A row this member cannot open, or one that fails admission (see "Threat model"), is passed through unchanged, and core then ignores it as malformed, like any junk entity.
The identity root has `visibility = 1`.

## MLS entities

All are `ag.kind = mls`, unsigned like chunks (MLS messages carry their own ML-DSA-44 signatures), with `ag.t` naming the type.
`kp` is a key package published from the member's wallet, with `ag.h = sha256(bytes)` as its id, `ag.pk` the member's composite public key, and `ag.sig` its leaf binding.
`group` is the private-repo marker, with `ag.tag = H("private-tag", rid)`.
`commit` comes from an admin's control address, with `ag.rid` blinded, `ag.ct = HMAC(generation-0 id key, "commits" || rid)`, `ag.ep` the new epoch, `ag.box` the epoch's lockbox and `ag.att` the public commit attestation (see "Join").
`welcome` carries `ag.kp`, the joiner's key package id, `ag.ep`, `ag.box` (the keyring under the history key) and `ag.att`, the welcome attestation sealed under the new epoch key.
`pub` is a published epoch grant, with `ag.tag` and `ag.ep`.
`stop` is the migration stopper (see "Migration").
Commits are MLS PrivateMessages (`PURE_CIPHERTEXT_WIRE_FORMAT_POLICY`), so proposals never reveal member identities.
Members read commits in Arkiv order and apply the one for the next epoch only if it is attested by its MLS sender and that sender is an admin.

## Roles

Roles are `admin | write | read`, kept in the sealed keyring and keyed by nid, with one entry per epoch in which they changed (`Keyring::role_log`).
Only admins add or remove members, and only root delegates publish epoch keys; write is needed to push.
The CLI refuses with `E_ROLE_DENIED`, and readers apply the same rules to what they read (see "Threat model").
The MLS credential is a basic credential naming the nid and the device (`nid || device`, the head-model device nid of the machine that made the key package), and it is bound to the nid's composite key by the leaf binding.

## Threat model

The adversary controls any number of wallets, may have been a member or admin earlier, and can write any entity to Arkiv.
It cannot break ML-DSA-65, Ed25519, ML-DSA-44, X-Wing or XChaCha20-Poly1305, and cannot forge the root identity of the repo.
Content and membership must stay confidential to current members, and members must accept only state an authorized member made.
The fixes below are named after the findings of the 2026-11 review (F), of its follow-up review of the fixes (N) and of the final review before 0.6.0 (R).

### Join (F2)

A welcome is accepted only if it carries an attestation: the composite signature of an admin over `rid || epoch || sha256(welcome) || sha256(lockbox) || kp id`, sealed under the new epoch key.
Every commit carries a public attestation: the signer's nid, `A = HMAC(epoch key, "epoch" || rid)`, the new epoch's admin set sealed under `HMAC(generation-0 id key, "admin-set" || rid)` (AAD kind `adm`), and the signer's composite signature over `rid || epoch || sha256(commit) || sha256(lockbox) || A || admin nids`.
Anyone holding the generation-0 id key finds every commit by `ag.ct`, reads its admin set and can check these signatures, so the admin chain needs no epoch key; only holders of that epoch's key can compute `A`.
The signed message also names the commit row's creator, the signer's control address for the repo, and a commit counts only if the row was created by that address (R2).
Without it, anyone could copy an attested commit into a new row with the same annotations, and a copy landing after the original's row expired would be taken for the commit and place the epoch's end at its own block.
A copy of a welcome or a grant changes nothing: a member places an epoch's start only from the canonical commit, never from a welcome's block, and a grant carries no block of its own.
A grant does make its epoch's key public, though, so for a grant reader a lockbox that opens under that key proves nothing about who wrote the row.
So where the canonical chain does not reach (the commits before it expired), a grant reader places an epoch's start only from an attested commit row whose attestation verifies, with the row's creator, under a key it already looked up for the chain or the grants (no extra query); a row without an attestation places an epoch only in a repo that has no attested commit at all.
An epoch whose start cannot be placed that way stays unplaced, and late writes of the epoch before it are not shown.
A joiner replays the admin history from the root before it accepts a welcome for epoch E.
It starts at the first commit signed by a delegate of the root identity (whose hash is the rid, and whose generation-0 blinding pins the welcome's generation-0 id key).
At each epoch from there to E it takes the first commit in Arkiv order signed by an admin of the epoch before, as the previous commit's attested admin set names them, and requires that commit's admin set and `A` to match the welcome keyring's admin set and key for that epoch.
The commit at E must be signed by the welcome's signer.
Members check the same fields when they apply a commit, so the history a joiner replays is the one members applied.
A removed admin keeps every key and admin set up to her removal, but the first commit after it was signed by a remaining admin and pins a key and admin set she does not have, so any welcome she forges fails the replay, even when no genuine welcome exists yet.
A joiner also evaluates every welcome for its key packages, each on a snapshot of its MLS state.
Every verified welcome continues the canonical record, so verified welcomes at different epochs (a re-add, or the same key package added again before its owner joined) are all genuine, and the newest wins (N5).
`E_EQUIVOCATION` is left for two verified welcomes at one epoch that name different groups, which the replay leaves no room for.
A welcome that is not for an unused key package of the joiner is skipped; any other failure to stage it is an error, not a silent skip.
A removed member that is added again on the same home rejoins: the new welcome replaces its old MLS state of the group (N7).
Until then its keyring keeps the old epochs, so it can still read history, but no role, so nothing acts on the role it held before the removal.

### Late writes and role checks (F3)

Each commit records the block it landed in (`Keyring::rotated`).
A member places the start of an epoch only from a commit it applied, a commit whose lockbox opens under that epoch's key, or the commit that removed it (which MLS authenticates as an admin's).
A joiner places them from the commits its replay accepts, and a grant reader from the canonical commit chain (see "Published grants").
Anyone who copies the blinded rid can write a commit row naming any epoch, so such a row places nothing (N1).
A keyring saved with an epoch start past the next epoch, or past the current one while the member is still in the group, drops it when next refreshed, and a lockbox's epoch starts past its own epoch are ignored.
A row sealed in epoch e is admitted only if it landed no later than the commit for e+1 plus `GRACE_BLOCKS` (64 blocks, about two minutes), so an in-flight push is not lost.
Readonly rows use their creation block.
Heads are mutable and anyone may extend them, which moves their update block, so a head is also admitted if its `ag.h` is in the snapshot of heads the admin pinned in the commit.
A head that is not pinned, was created in time and whose update block passed the cutoff (a body that landed in the grace window and was extended later) is admitted if its body's transaction from the head's creator landed by the cutoff plus grace.
The search reads only blocks `at + 1` to `min(at + 64, cutoff + grace)`, because a body must land within 64 blocks after its signed `at` (docs/p5/head-model.md 5.4), so a removed member's later patch with a backdated `at` finds nothing there.
Without this, such a head was hidden from other members, and keepalive stopped extending the snapshot and pieces it names, until its writer pushed again.
The verdict is final, because the epoch has ended, so a backend made with `Sealed::caching` keeps it per entity key, `ag.h` and bound in `$CHARTER_HOME/private/<rid hex>.landed.json` (mode 600), and later reads pay no block reads for it (N4).
The CLI, the remote helper and the UI sync loop use it.
A signed row of a write kind (anchor, refs, manifest, device, head) is admitted only if its `ag.nid` held write or admin in the row's epoch; other signed rows need read.
A head is signed by a device key but names its member in `ag.nid` (the device is `ag.dev`), so it is checked against the member's role: a writer demoted to read keeps its device key and device record, but its new heads do not count, and neither do an outsider's (N8).
A snapshot manifest's `ag.nid` is the device key, which holds no role, so a manifest passes this check; it is content addressed and counts only when an admitted head names it, so a manifest from a demoted writer's device has no effect.
Unsigned rows (chunks, pieces) pass, since only an admitted signed row can reference them.
The same check covers cob ops, heads and refs, because all of them are opened by `Sealed`.

### Removing every device (F4)

A removal removes every MLS leaf whose credential names the nid, not only the first.
`chart member list` prints how many devices each member has.
A second leaf for a nid that already holds a role is added only by `chart member add --device`, never by a plain add.

### Revoked devices (head-model 15.7)

Every leaf names its device, so a device revoke (`chart device revoke`, docs/p5/head-model.md section 15) names exactly one leaf.
When an admin's client refreshes the group (every private-repo command and every push, fetch and clone does), it reads the repo's revoke records in the same query as the commits and counts the leaves whose member and device a record names, from their plaintext annotations, with no further query.
A removal opens those records and keeps only the ones `charter_core::revocations` accepts, the same rule readers apply to heads: signed by the leaf's member, for this repo, created by one of its control addresses (R5); a record that fails it removes nothing.
A refresh never sends anything: removing a leaf is a paid commit, so it happens only on an explicit write by an admin (R4).
`chart device revoke` by an admin removes the leaf at once; otherwise an admin runs `chart member sync --repo R`, which removes every revoked leaf in one commit and prints the cost, or confirms the priced "Remove from the group" write that `chart ui` shows on the repo page while a revoked leaf is left.
The removal rotates the epoch key and the id-key generation, as a member removal does; the member keeps its role and its other devices.
An admin whose own device is revoked has that removal made by its remaining device, and a client whose own device is revoked removes nothing.
Two admins may both remove the same leaf; the pending-commit rule below makes exactly one removal land in the group.
From the revoke on, the device's later heads are ignored by every reader (head-model 15.2), so what it writes after the cut is refused before the removal lands.
What it can still do is decrypt every row sealed before the removal commit lands.
`chart device revoke` says so and names `chart member sync` when the revoking identity is not an admin.
The revoke record lives as long as other control rows (`control_lifetime`), so the removal must happen within that time.
Leaves made before 0.6.0 name no device: a revoke cannot find them, `chart member list` marks them, and the fix is to remove the member (`chart member remove`) and add its remaining devices again (`chart member keypackage` on each, then `chart member add` and `chart member add --device`).

### Pending commits (R1)

MLS state (`$CHARTER_HOME/private/mls.json`) is shared by every repo of a home, so every command that reads or changes it (add, remove, a revoked-leaf removal, the format migration, a refresh, a key package, init) takes the home's lock file (`private/lock`) for its whole run.
Two commands on one machine therefore never both commit, and a refresh never saves over a commit made meanwhile.
A membership change leaves its MLS commit pending instead of merging it, and saves it to disk together with the nonce its tx takes on the admin's control address before anything is sent, so a crash or Ctrl-C after the broadcast still finds it.
The next refresh merges it only when its commit row is the first valid commit for that epoch (recognised by its lockbox opening under the pending epoch's key, since MLS cannot decrypt its own commit); when another admin's commit landed first, it drops its own and applies that one, so the group never forks.
This costs no query: the refresh reads the commit rows anyway.
A pending commit that the refresh did not settle is dropped only once another tx has taken that nonce on chain, since only then can it never land; a block count alone is not used, because a tx that timed out can still sit in the pool and land late.
While the nonce is still free, the next membership change first sends a zero-value transfer from the control address at that nonce.
If the node refuses it, the old commit is still waiting in the pool, and the change is refused with `E_CONCURRENT_PUSH` until it is mined; the next refresh then merges it.
If the transfer is mined, the old commit can never land, so it is dropped and the new change is sent after it.
That transfer is paid only after an interrupted or lost send; the normal path costs nothing extra (the nonce read replaces the one the send made anyway).
Pricing a change in `chart ui` sends nothing and prices it as if the old commit is dropped.
The commit tx is assumed to be one tx: a commit plus welcome is a few KB, far under the tx size limit.

### Key package claims (F5)

A key package names a nid, its device, its composite public key and a signature by that key over the MLS signature key, the nid and the device (context `charter-mls-leaf-device`), and an admin refuses one whose binding does not verify (`E_BAD_SIG`), or that names no device (made before 0.6.0).
A plain add of a key package whose nid already holds a role is refused (`E_BAD_FORMAT`), so an add can never demote an admin or add a hidden device.

### Published grants (F7, N2)

A grant is `id_key || key || public key || signature || (nid || role)*`, signed by a root delegate over the rid, the epoch, both keys and the roles of that epoch.
A reader picks the root-epoch grant whose keys open the root identity row (its hash is the rid), preferring a signed one.
That gives it the generation-0 id key, so it finds the commit records by `ag.ct`.
It then builds the canonical commit chain from the attestations alone, as a joiner does: from the first commit signed by a root delegate, at each epoch the first commit signed by an admin of the epoch before, as the previous commit's attested admin set names them.
A grant for an epoch on that chain counts only if the epoch's commit authenticates its key (`A`), its signer was an admin of the epoch before, and the admins among its roles are exactly the commit's attested admin set (R8).
Roles below admin are not attested on chain, so a grant is trusted for those as its signer states them.
A grant for an epoch before the chain starts (a legacy epoch) needs only a root delegate's signature.
The chain also places the start of every epoch, including the end of the newest granted one.
A root delegate that another admin removed from the group therefore cannot publish a grant for an epoch it never held, and a grant published first by anyone can neither block nor replace the real one.
The check opens no lockbox, so a reader learns exactly the epochs it holds grants for, and gaps between granted epochs are fine.
A grant names one epoch; `publish_epoch_keys(a, cfg, keys, rid, range)` publishes one grant per epoch of a range, so a reader of a range holds each key by its own verified grant, and no separate history entity is needed.
A legacy 64-byte grant is still accepted for the root epoch, where opening the root identity authenticates it.
In a repo with no attested commit yet (legacy, or never changed), every grant signed by a root delegate counts, the first per epoch.

### Id-key rotation (F8)

Every removal starts a new id-key generation, so a removed member can no longer link new entities to the repo by their blinded rid or names.
Sealing uses the generation of the row's epoch; a row whose blinded rid does not match that generation is rejected.

### Pack ids (F9)

The plain SHA-256 of a pack linked a sealed chunk to a public copy of the same pack.
In a format-2 repo it is sealed and blinded, as above.

### Low-severity items (F12)

Key material is zeroized on drop.
The lockbox and the attestation have their own reserved AAD kinds.
A welcome names its key package in plaintext (`ag.kp`), and key packages, commits and welcomes come from members' wallets, so an observer can link a joiner's wallet to the time it joined a group (not to the rid); relayers would remove this (see TODO).

## Lost or stolen machine

A device revoke stops a device seed (`device/seed`), not an identity.
If the machine also held this identity's `keys/` directory, it holds the identity key: it can sign as you, revoke your other devices, and read every private repo you can, since `keys/` and `private/` usually travel together.
Revoking its device does not stop that, and `chart device revoke` prints this warning in public and private repos alike.
Recovery:

1. From a machine you still trust, revoke the lost device (`chart device revoke`), so its later heads stop counting at once.
2. In each private repo, have another admin remove the identity (`chart member remove --member <nid>`); this rotates the epoch key and the id-key generation, so the lost machine reads nothing new.
   If the lost identity is the repo's only admin, the lost machine holds the same powers as you, so the group cannot be taken back: start a new private repo from the new identity (step 4) and push the content there.
3. In each public repo, have a delegate set that no longer names the lost identity take over (a new root identity or delegate set).
   A public repo whose only delegate is the lost identity cannot be recovered: an identity change needs that delegate's signature, which the lost machine holds as much as you do.
   The only remedy is ahead of time: add a second delegate (`chart id propose`) to every repo you cannot afford to lose.
4. Start a new identity on a clean machine (a fresh `$CHARTER_HOME`) and have an admin add it again with `chart member keypackage` and `chart member add`.

Until step 2 lands, the lost machine can also revoke your other devices and remove members as you, so act from the machine that still holds the keys first.

## Migration

A repo created before 0.6.0 has keyring format 1: no attestations, admin sets, role history or commit blocks.
A 0.6.0 client reads such a repo as before, deriving the role history and commit blocks from the commits it can open, and accepts unattested commits only while the format is 1.
The first membership change on 0.6.0 must be made by a root delegate; any other admin is refused with a clear upgrade message.
Members enforce this too: they apply the migrating commit only if its signer is a delegate of the root identity, so an admin whose client skips the check cannot migrate the repo (N3).
That commit sets format 2, records the admin set, pins the heads, and is attested; it is the first commit a joiner's replay starts from.
From then on every client requires attestations, admission and the blinded pack.
The same tx writes a `stop` row: an identity row under the generation-0 blinded rid whose `ag.v` no client supports.
A pre-0.6.0 member then fails with `E_UNSUPPORTED_VERSION` on its next read instead of accepting forged state, and 0.6.0 clients skip the stopper.
Keepalive extends the stopper with the rest of the root delegate's control plane, so it lives as long as the repo is kept alive.
A grant reader on a migrated repo needs 0.6.0 as well.

## API

`init(a, cfg, keys, meta) -> rid` creates the repo with the caller as sole delegate and admin, in format 2.
`publish_key_package(a, cfg, keys) -> id` publishes a key package with its leaf binding.
`add_member(a, cfg, keys, rid, kp_id, role) -> nid`, `add_device(a, cfg, keys, rid, kp_id) -> nid` and `remove_member(a, cfg, keys, rid, nid)` issue commits.
`members(home, rid) -> (epoch, [(nid, role, devices, devices made before 0.6.0)])` lists the current members.
`revoked::leaf_state(a, cfg, keys, rid, device)` refreshes the group and says whether a leaf still names the device.
`revoked::remove_revoked(a, cfg, keys, rid) -> count` removes every revoked leaf in one commit (admins only), and `revoked::removal_ops(a, cfg, keys, rid)` builds that commit's operations without sending or saving anything, for pricing.
A keyring's `revoked_leaves` says how many revoked leaves its last refresh found still in the group.
`refresh` and `ring_for` bring a member's keyring up to date, joining from a welcome when needed, and return `E_NOT_MEMBER` for a private repo the caller cannot read.
`publish_epoch_key(a, cfg, keys, rid, epoch) -> grant` publishes one epoch's signed grant in plaintext, so anyone who knows the rid can read what was sealed in that epoch, and nothing else (sponsorware, source escrow).
`publish_epoch_keys(a, cfg, keys, rid, range) -> [grant]` publishes one grant per epoch of a range, one tx each.
`export_epoch_key(home, rid, epoch) -> grant` returns the same grant locally, for an escrow agent or a timelock.
Repo programs are expected to call these through obligations later.

## CLI

`chart init --private` creates a private repo.
`chart member keypackage` publishes a key package and prints its id.
`chart member add --repo R --key-package <id> --role write` adds a member.
`chart member add --repo R --key-package <id> --device` adds another device of an existing member.
`chart member remove --repo R --member <nid>` removes a member and all its devices.
`chart member list --repo R` prints nids, roles and device counts, and how many devices of each member predate device binding.
`chart member sync --repo R` removes every revoked device's leaf in one commit and prints its cost, or says there is nothing to remove.
`chart device revoke` by an admin of a private repo does the same at once.
`git push` and `git clone arkiv::<rid>` on a private repo work for members (a new member joins on first use) and fail with `E_NOT_MEMBER` for anyone else.

## Unsealed-write guard and local permissions

Every repo-scoped `chart` command and the `chart ui` write worker pick their backend with `charter_private::for_repo` (the remote helper with `ring_for`), so a private repo's writes and reads go through `Sealed`.
As a fail-closed backstop, charter-sync refuses with `E_UNSEALED_PRIVATE` before any control write (identity, binds, ops, device revokes), any push (chunks, pieces, head txs) and any keepalive when the backend does not seal under that repo's own keys (`Arkiv::seals_for`) and `$CHARTER_HOME/private/<rid>.json` exists.
A backend sealing another private repo therefore fails the check too, and `Sealed` refuses to seal an op that names another repo's rid instead of sending it as is (N6).
The check is one `stat` per write and none on a backend sealing that repo; group operations (key packages, commits, welcomes, grants) seal themselves and do not pass through it.
A machine that reads a private repo only through published grants holds no keyring file, so the guard does not cover it; such readers cannot write anyway.
Decrypted data stays with its owner: `$CHARTER_HOME`, `repos/` and each repo cache are made 0700 (an existing home is tightened on first use), the cache git is created with `core.sharedRepository=0600`, and packs and fetch temp files are created 0600.
A cache made before this is tightened once on its first open: every directory 0700, every file 0600, and `core.sharedRepository=0600` added to its git config; an `owner-only` marker file in the cache then makes every later open a single `stat`.
`scripts/chart-ci.sh` and `scripts/chart-release-attest.sh` run under `umask 077`.
The cache directory is still named by the rid in hex, which a local user who can list `$CHARTER_HOME` would learn; with the 0700 home nobody else can, so naming caches by a keyed hash of the rid is left as an option.

## Cost

Push, clone, fetch and sync make no more queries than in 0.5.0, and `tests/query_counts.rs` holds them to the 0.5.0 counts on the fake chain.

| Operation | 0.5.0 | 0.6.0 |
| --- | --- | --- |
| push | 20 | 20 |
| clone (member, warm) | 3 | 3 |
| fetch | 12 | 12 |
| sync | 6 | 6 |
| clone through published grants | 14 | 14 |
| join and first clone | 13 | 16 |
| add | 2 | 4 |
| remove | 1 | 3 |

Join reads the root identity, the signer's peer key and the commit list for the admin replay, plus one peer key per other admin who committed.
Add and remove read the heads to pin and the admin's own peer document; the migrating commit also reads the root identity, and so does each member applying it, once.
Each commit applied costs one peer-key lookup for its signer, and a refresh queries commits again only when a removal starts a new id-key generation.
A grant reader saves one query (the `group` and `pub` lookups are merged) and spends it on one commit list, attested and legacy rows together, that serves both its history check and admission.
Its chain check looks up the key of each committing admin that signed no grant (none when, as usual, the root delegate both commits and publishes) and checks one commit signature per epoch on the chain.
A removed member's refresh also looks for a re-add's welcome, one query per key package it published.
The refresh's commit query also returns the repo's device records (delegations and revokes), so finding a pending revoke costs no query.
A refresh never sends: the count of revoked leaves it reports is local.
A removal (`chart member sync`, an admin's `chart device revoke`, or the UI write) costs one peer-key lookup per revoking identity, the remove path's queries and one commit tx, and its pricing in the UI runs the same queries without the tx.
The commit stays pending until the next refresh, which reads the commit rows anyway, so seeing which admin's commit landed first costs no extra query; the first refresh after any removal may query commits once more for the new id-key generation, as for every member.
Each attestation and signed grant costs one composite signature to make and one to check, and a join checks one commit signature per epoch it replays.
Each commit and welcome grows by an attestation (about 3.4 KB), and the welcome's lockbox by a few bytes per role change, admin set, commit and pinned head.
A commit's lockbox no longer grows with history: with 1 to 9 members it is 1,068 to 2,092 B, against 2,092 to 4,140 B for the full keyring, so each commit row is 1,024 to 2,048 B smaller in the box.
Its attestation carries the sealed admin set instead of a 32 B digest: 3,741 B instead of 3,469 B for one admin (+272 B), so the commit row is 752 to 1,776 B smaller in all; the sealed set grows by 32 B per admin.
Welcome lockboxes keep their size (the whole keyring, as before).
A head extended past its epoch's cutoff costs at most 64 block reads, once per head and body on a backend that caches verdicts, and once per read otherwise.
A signed grant is 64 B plus 5.4 KB of key and signature plus 33 B per member.

## Known limits

Knowing the rid reveals that a private repo exists, through the `group` marker, which is what lets a non-member get `E_NOT_MEMBER` instead of a generic not-found.
A public identity for the rid takes precedence over the marker, so a forged marker cannot close a public repo.
A published grant covers one epoch, so readers also need the grants of the epochs that sealed the identity and binds.
In a repo migrated from 0.5.0, the lockboxes written before the migration hold the full keyring, so a grant for one of those epochs still reveals the epochs before it.
Concurrent admins committing at the same epoch do not fork the group: the first commit in Arkiv order wins and the other admin's pending commit is dropped on its next refresh ("Pending commits").
An admin can still commit at an epoch before she is removed; the first commit in Arkiv order wins, as for members.
A pending commit whose send result was lost is kept until its nonce is decided on chain ("Pending commits"), so it cannot be dropped locally while members apply it.
Commits made before 0.6.0 carry no attestation, so their creator cannot be checked; they are accepted only while the keyring format is 1.
A delegate of the root identity is trusted for good where no admin set is on record yet: it can sign the first commit of a replay while none exists, and in a migrated repo a junk record it signs for a legacy epoch makes joins fail until the row expires (the replay ignores records for epoch 0, which no commit makes).
After the first attested commit it is an admin only while the admin sets say so, for joiners, members and grant readers alike.
In a repo with no attested commit, a grant reader cannot place the end of its newest granted epoch.
Anyone who sees a commit row can count a repo's commits, see when they landed and which admin signed each: `ag.ct` is the same on every commit of a repo, and the attestation names the signer's nid in plaintext (R6).
Sealing that nid would not help, because the row's creator is the signer's control address, which its bind rows already link to its nid in plaintext; relayers would (see TODO).
Only holders of the generation-0 id key (every member, past or present, and grant readers) can also read the admin set each commit made, and nobody else learns what a commit changed or who else is a member.
A pre-0.6.0 client that first reads a migrated repo after its keepalive lapsed sees no stopper, though it still cannot open anything sealed under a later id-key generation.
Leaves added before 0.6.0 were never checked against a leaf binding; an admin who doubts one should remove that member and add it again.
Libcrux ML-DSA and X-Wing overflow 2 MiB stacks in debug builds, so the tests run on 64 MiB worker stacks.

## TODO

- Relayers, so commits, welcomes and pushes do not link members' control addresses to the repo.
- Full blinded annotations: `ag.nid`, `ag.h`, `ag.seq`, `ag.at`, `ag.i` and entity sizes still leak structure.
- Rotation timer: a scheduled `self_update` commit for post-compromise security.
- Re-encrypting history after a removal, if a removed member must also lose access to old content.
