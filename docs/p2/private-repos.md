# P2: private repos

Date: 2026-11-07.
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
The id-blinding key is `export_secret("arkiv-git id", rid, 32)` of the creating epoch and never rotates, so blinded queries keep matching across epochs.
Members keep every epoch key they ever held in a keyring file, `$CHARTER_HOME/private/<rid hex>.json`, mode 600.
OpenMLS state (the ML-DSA-44 signature key, key package secrets and groups) lives in `$CHARTER_HOME/private/mls.json`, mode 600.
Each commit and welcome carries the keyring (id key, all epoch keys so far, roles) sealed under the new epoch key, so a new member can read history and every member learns role changes.
A removed member cannot open that lockbox, because it cannot derive the new epoch key.

## Sealing

Any create op whose `ag.rid` is the repo's rid is sealed in `Arkiv::seal_ops`, which the tx pipeline calls before pricing and signing.
The attributes `ag.body`, `ag.rid`, `ag.name`, `ag.lang`, `ag.lic`, and any other `bytes32` attribute equal to the rid (the root identity's `ag.h`, revision 1's `ag.parent`) move into one sealed `ag.body`.
The sealed body is `nonce || XChaCha20-Poly1305(key, nonce, u32be(len) || plaintext || zeros)` with a random 24-byte nonce.
The AAD is `rid || kind name || u64be(epoch)`.
The plaintext is padded to a power-of-two bucket from 256 B to 64 KiB.
Above 64 KiB (full chunks and near-maximal envelopes with their few hidden attributes) the bucket is the next 4 KiB multiple, because a doubled 128 KiB bucket would break the 128 KiB attribute and tx caps.
The entity gains `ag.rid = HMAC-SHA-256(id_key, "rid" || 0x00 || rid)`, `ag.name = HMAC-SHA-256(id_key, "name" || 0x00 || name)` when it had a name, and the plaintext epoch `ag.ep` so readers pick the right key.
`ag.kind`, `ag.v`, `ag.nid`, `ag.h`, `ag.seq`, `ag.at`, `ag.pack` and `ag.i` stay in plaintext for now (see TODO).
The `Sealed` adapter rewrites queries for the rid and names to their blinded values and opens rows before the sync layer sees them.
A row this member cannot open is passed through unchanged, and core then ignores it as malformed, like any junk entity.
The identity root has `visibility = 1`.

## MLS entities

All are `ag.kind = mls`, unsigned like chunks (MLS messages carry their own ML-DSA-44 signatures), with `ag.t` naming the type.
`kp` is a key package published from the member's wallet, with `ag.h = sha256(bytes)` as its id.
`group` is the private-repo marker, with `ag.tag = H("private-tag", rid)`.
`commit` comes from an admin's control address, with `ag.rid` blinded and `ag.ep` the new epoch.
`welcome` carries `ag.kp`, the joiner's key package id, and `ag.ep`.
`pub` is a published epoch grant (`id_key || content key`), with `ag.tag` and `ag.ep`.
Commits are MLS PrivateMessages (`PURE_CIPHERTEXT_WIRE_FORMAT_POLICY`), so proposals never reveal member identities.
Members read commits in Arkiv order and apply the one for the next epoch; a commit whose sender is not an admin is skipped.

## Roles

Roles are `admin | write | read`, kept in the sealed keyring and keyed by nid.
Only admins add or remove members or publish epoch keys, and write is needed to push.
Enforcement is client-side, as for delegates of public repos: the CLI refuses with `E_ROLE_DENIED`, and members ignore commits from non-admins.
The MLS credential is a basic credential naming the nid.

## API

`init(a, cfg, keys, meta) -> rid` creates the repo with the caller as sole delegate and admin.
`publish_key_package(a, cfg, keys) -> id` publishes a key package.
`add_member(a, cfg, keys, rid, kp_id, role) -> nid` and `remove_member(a, cfg, keys, rid, nid)` issue commits.
`refresh` and `ring_for` bring a member's keyring up to date, joining from a welcome when needed, and return `E_NOT_MEMBER` for a private repo the caller cannot read.
`publish_epoch_key(a, cfg, keys, rid, epoch) -> grant` publishes one epoch's grant in plaintext, so anyone who knows the rid can read what was sealed in that epoch (sponsorware, source escrow).
`export_epoch_key(home, rid, epoch) -> grant` returns the same 64 bytes locally, for an escrow agent or a timelock.
Repo programs are expected to call these through obligations later.

## CLI

`chart init --private` creates a private repo.
`chart member keypackage` publishes a key package and prints its id.
`chart member add --repo R --key-package <id> --role write` adds a member.
`chart member remove --repo R --member <nid>` removes one.
`chart member list --repo R` prints nids and roles.
`git push` and `git clone arkiv::<rid>` on a private repo work for members (a new member joins on first use) and fail with `E_NOT_MEMBER` for anyone else.

## Unsealed-write guard and local permissions

Every repo-scoped `chart` command and the `chart ui` write worker pick their backend with `charter_private::for_repo` (the remote helper with `ring_for`), so a private repo's writes and reads go through `Sealed`.
As a fail-closed backstop, charter-sync refuses with `E_UNSEALED_PRIVATE` before any control write (identity, binds, ops), any push (chunks, pieces, head txs) and any keepalive when the backend does not seal and `$CHARTER_HOME/private/<rid>.json` exists.
The check is one `stat` per write and none on a sealed backend; group operations (key packages, commits, welcomes, grants) seal themselves and do not pass through it.
A machine that reads a private repo only through published grants holds no keyring file, so the guard does not cover it; such readers cannot write anyway.
Decrypted data stays with its owner: `$CHARTER_HOME`, `repos/` and each repo cache are made 0700 (an existing home is tightened on first use), the cache git is created with `core.sharedRepository=0600`, and packs and fetch temp files are created 0600.
A cache made before this is tightened once on its first open: every directory 0700, every file 0600, and `core.sharedRepository=0600` added to its git config; an `owner-only` marker file in the cache then makes every later open a single `stat`.
`scripts/chart-ci.sh` and `scripts/chart-release-attest.sh` run under `umask 077`.
The cache directory is still named by the rid in hex, which a local user who can list `$CHARTER_HOME` would learn; with the 0700 home nobody else can, so naming caches by a keyed hash of the rid is left as an option.

## Known limits

Knowing the rid reveals that a private repo exists, through the `group` marker, which is what lets a non-member get `E_NOT_MEMBER` instead of a generic not-found.
A public identity for the rid takes precedence over the marker, so a forged marker cannot close a public repo.
A published grant covers one epoch, so readers also need the grants of the epochs that sealed the identity and binds.
Concurrent admins committing at the same epoch fork the group; the first commit in Arkiv order wins and the other admin must rejoin.
Libcrux ML-DSA and X-Wing overflow 2 MiB stacks in debug builds, so the tests run on 64 MiB worker stacks.

## TODO

- Relayers, so commits, welcomes and pushes do not link members' control addresses to the repo.
- Full blinded annotations: `ag.nid`, `ag.h`, `ag.seq`, `ag.at`, `ag.pack`, `ag.i` and entity sizes still leak structure.
- History lockbox as its own rotating entity, instead of the full keyring inside every commit and welcome.
- Rotation timer: a scheduled `self_update` commit for post-compromise security.
- Binding the MLS credential to the composite identity key, so a key package cannot claim someone else's nid.
- Reader-side role checks, so anchors from members without write are ignored rather than only refused at push.
- Re-encrypting history after a removal, if a removed member must also lose access to old content.
