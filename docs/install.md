# Install

Prebuilt binaries are attached to every GitHub Release, so no Nix or Rust build is needed.
Each archive holds `git-remote-arkiv`, `chart` and `chart-mcp` plus the licence files.
Charter runs the `git` on `PATH` and needs git 2.18 or newer (snapshot packs use `pack-objects --keep-pack`).
On Windows the default home is `%USERPROFILE%\.charter`, and the key files there inherit the profile's ACL: the user, SYSTEM and Administrators only.
Two targets are published: `aarch64-apple-darwin` (Apple Silicon macOS) and `x86_64-unknown-linux-gnu` (Linux x86_64, glibc 2.35 or newer).

## Download URL pattern

```
https://github.com/chayan-bit/charter/releases/download/<tag>/charter-<tag>-<target>.tar.gz
https://github.com/chayan-bit/charter/releases/download/<tag>/SHA256SUMS
```

For v0.7.1 on Apple Silicon macOS the archive is `https://github.com/chayan-bit/charter/releases/download/v0.7.1/charter-v0.7.1-aarch64-apple-darwin.tar.gz`.
For v0.7.1 on Linux x86_64 the archive is `https://github.com/chayan-bit/charter/releases/download/v0.7.1/charter-v0.7.1-x86_64-unknown-linux-gnu.tar.gz`.
v0.1.0 predates the rename to Charter: its archives are named `arkiv-git-v0.1.0-<target>.tar.gz` and hold `git-remote-arkiv`, `arkiv-git` and `arkiv-git-mcp`.
A combined `SHA256SUMS` file is attached to the release as well.

## Install snippet

```sh
TAG=v0.7.1
TARGET=aarch64-apple-darwin   # or x86_64-unknown-linux-gnu
BASE=https://github.com/chayan-bit/charter/releases/download/$TAG
curl -fsSLO "$BASE/charter-$TAG-$TARGET.tar.gz"
curl -fsSLO "$BASE/SHA256SUMS"
shasum -a 256 -c --ignore-missing SHA256SUMS
mkdir -p ~/.local/bin
tar xzf "charter-$TAG-$TARGET.tar.gz" -C ~/.local/bin git-remote-arkiv chart chart-mcp
export PATH="$HOME/.local/bin:$PATH"
```

Git finds `git-remote-arkiv` on `PATH`, so cloning works straight away.

```sh
git clone arkiv::<rid>
```

## macOS Gatekeeper

Files fetched with `curl` carry no quarantine flag, so the snippet above runs as is.
If you download the archive in a browser and macOS refuses to open the binaries, clear the flag with `xattr -d com.apple.quarantine ~/.local/bin/git-remote-arkiv ~/.local/bin/chart ~/.local/bin/chart-mcp`.

## Upgrading to v0.7.1

v0.7.1 is a security fix for the Markdown sanitizer of `chart ui`: it moves to ammonia 4.2.5 for RUSTSEC-2026-0533, a mutation XSS through embedded SVG and MathML.
v0.6.0 and v0.7.0 ship the affected ammonia 4.2.4, so upgrade every machine that runs `chart ui`.
Charter's sanitizer config allows none of the tags the advisory needs, and the UI's CSP allows only its own scripts, but the fixed version is the only supported one.
Nothing else changes: no format, command or file differs from v0.7.0.

## Upgrading to v0.7.0

v0.7.0 adds search to the web UI, signed-commit display, labels and milestones, code owners, inline review comments, and archive, rename and hand-over (`docs/p3/cob-ops.md`, `docs/p3/programs.md`, `docs/p1/design.md` section 2.2).
It reads every repo v0.6.0 wrote and changes no storage format on its own, and the device seed, `keys/` and `private/` from v0.6.0 are kept; no new files are needed.

### New commands

`chart signing-key claim --repo <url> --key <ssh key file>` proves with `ssh-keygen` that you hold an Ed25519 SSH key and posts the claim, so commits you sign with it show as signed by you.
`chart signing-key verify --repo <url> <commit>` checks one cached commit's SSH signature against the repo's claims and exits with `E_BAD_SIG` on a bad one.
`chart id propose <url> --archive` and `--unarchive` mark a repo archived or not, `--name` renames it, and `--add-delegate` and `--remove-delegate` hand it over; each is an identity revision under the delegate quorum.
A new delegate must already have a peer on Arkiv, and a revision that removes your own delegate seat needs `--confirm <current repo name>`.
Labels, assignees and milestones (actions `label`, `unlabel`, `assign`, `unassign` and `milestone`) and inline review comments are ops: `chart op post`, the MCP `op_post` tool and the UI write them.
`programs/codeowners.rhai` makes `main` need a code owner's approval for each changed path; `chart program pin` pins it on engine `rhai/2`, because it reads git.

### Search

The UI's search runs on the repos in `$CHARTER_HOME` only by default.
Settings can switch it to Local + Arkiv, and even then Arkiv is asked only when you submit a search, never while you type.

### Archive is advisory

An archived repo is a rule honest clients keep, not one Arkiv enforces.
Charter's helper refuses a push to an archived repo with `E_ARCHIVED` before it spends gas, and the UI hides its write forms, but anyone can still write ops to it, and a delegate quorum can unarchive it.

### Old clients

A program pinned on engine `rhai/2` (for example `codeowners.rhai`) makes v0.6.0 stop with `E_PROGRAM_MISSING`, because it does not know the engine, instead of folding the program differently; programs that read no git stay on `rhai/1`, which v0.6.0 reads.
An archived repo carries an identity revision with `ag.v = 2`, and v0.6.0 stops on it with `E_UNSUPPORTED_VERSION` instead of rolling the identity back.
A rename or a hand-over keeps `ag.v = 1`, so v0.6.0 reads it.
v0.6.0 shows the new label, assign, milestone, signing-key and inline-comment ops as ordinary entries or ignores them, so they are safe for it, but it shows none of the new state.
Upgrade every machine and CI runner that reads a repo before the repo pins a `rhai/2` program or is archived.

## Upgrading to v0.6.0

v0.6.0 hardens private repos (`docs/p2/private-repos.md`), adds device revoke (`docs/p5/head-model.md` section 15) and keeps large ref lists in ref tables (section 14).
It still reads every repo v0.5.0 wrote, and the device seed and `keys/` from v0.5.0 are kept; no new files are needed.

### Private repos and keyring format 2

A private repo created before v0.6.0 has keyring format 1, and v0.6.0 reads it as before.
The first membership change on v0.6.0 (an add, a remove or a revoked-device removal) migrates the repo to keyring format 2, and a delegate of the root identity must make it; any other admin is refused with an upgrade message.
From then on every commit and welcome is attested and checked against the chain, published grants are checked against the attested admin chain, pack ids are sealed, and every removal rotates the id-blinding key as well as the epoch key.
The migrating commit also writes a stopper row, so a v0.5.0 member stops with `E_UNSUPPORTED_VERSION` on its next read instead of accepting forged state, and a grant reader of a migrated repo needs v0.6.0 too.
Upgrade every member's machine before a root delegate makes the first membership change.
MLS leaves made before v0.6.0 name no device: `chart member list` marks them, and they become revocable one by one only after the member is removed and its devices are added again.

### `chart member sync`

`chart member sync --repo <url>` removes the MLS leaf of every revoked device in one commit and prints its cost, or says there is nothing to remove.
An admin runs it after a member who is not an admin revokes a device; an admin's own `chart device revoke` removes the leaf at once.
`chart ui` offers the same priced removal on the repo page.

### Device revoke and stolen machines

`chart device list <url>` lists the devices of this identity, and `chart device revoke <url> <device nid>` revokes one from a machine that holds `keys/`.
From the revoke on, every reader ignores the device's later heads, and a push from that device fails with `E_DEVICE_REVOKED`.
In a private repo the revoked device can still decrypt every row sealed before its MLS leaf is removed.
A revoke stops a device seed, not an identity.
A stolen machine that also held `keys/` holds your identity key, so it can still sign as you, revoke your other devices and read every private repo you can, and `chart device revoke` prints this warning.
The recovery is in `docs/p2/private-repos.md`, "Lost or stolen machine": revoke the device, have another admin remove the identity from every private repo, and start a new identity on a clean machine.

### Format v4 ref tables

A device whose refs no longer fit the 62 KB head body used to fail with `E_REFS_TOO_LARGE`; how many fit depends on the ref names, and in a measured run 1,500 short tags on one commit still fit while 3,000 did not.
v0.6.0 keeps most of those refs in a ref table and writes a format v4 head that lists only the entries that differ from the table.
Nothing migrates eagerly: a device switches to v4 on the first push whose refs do not fit and back to v3 when they fit again, so a repo whose devices stay under the cap keeps its format.

### Old clients

v0.5.0 stops with `E_UNSUPPORTED_VERSION` and "upgrade charter" on a private repo migrated to keyring format 2, on a repo with a revoke record, and on a repo where any synced device wrote a v4 head, rather than show stale refs or forged state.
A v0.5.0 program fold that walks back through a v4 body stops with `E_NOT_LIVE` instead of folding partial refs.
On a repo with none of these, v0.5.0 keeps working.
Upgrade every machine and CI runner that reads a repo before its first v0.6.0 membership change, revoke or large push.

## Upgrading to v0.5.0

v0.5.0 writes git data in storage format v3 (`docs/p5/head-model.md` section 13) and still reads repos written in formats v1 and v2.
The first v0.5.0 push of each machine migrates that machine's head, and from then on v0.4.0 and older stop on the repo with `E_UNSUPPORTED_VERSION` rather than show stale refs.
Upgrade every machine and CI runner that reads a repo before or right after its first v0.5.0 push.
The device seed from v0.4.0 is kept; no new files are needed.

## Upgrading to v0.4.0

v0.4.0 writes git data in storage format v2 (`docs/p5/head-model.md`) and still reads repos written by older versions.
Its first push to an older repo migrates that repo, and from then on v0.3.0 and older stop on it with `E_UNSUPPORTED_VERSION` rather than show stale refs.
Upgrade every machine and CI runner that reads a repo before or right after its first v0.4.0 push.
Each `CHARTER_HOME` gets a device seed at `device/seed` on its first v0.4.0 push; keep it next to `keys/`, and do not copy it between machines (copy only `keys/`, so the second machine becomes its own device).
