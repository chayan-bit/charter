# Signed, CI-attested releases

Status: implemented and tested on the `mem:` backend; the live v0.1.0 record on Rabdi is posted by the coordinator.
Date: 2026-11-07.

A release is one signed op on Arkiv that names a commit and the sha256 and size of every artifact built from it.
The bytes are never in the op.
Trust comes from the hash inside a signed op that the pinned repo program accepted, so any mirror is fine: the GitHub Release, IPFS, S3, or later Arkiv blobs.
A downloader checks the bytes against the record, not the mirror.

## Op shapes

The release record is an `xyz.arkiv.release` op with action `create`, posted by a delegate:

```json
{
  "tag": "v0.1.0",
  "commit": "<hex oid>",
  "artifacts": [
    {"name": "arkiv-git-v0.1.0-x86_64-unknown-linux-gnu.tar.gz", "target": "x86_64-unknown-linux-gnu",
     "sha256": "<64 hex>", "size": 17340809, "locations": ["https://..."]}
  ],
  "notes": "optional"
}
```

A delegate adds mirrors with action `revise` on the same object, with the same `artifacts` shape.
A revise only adds locations to an artifact whose `sha256` and `size` match, and may add artifacts with new names.
It can never change the commit or the hash of an existing artifact.
There is no CLI for revise; use `chart op post --type xyz.arkiv.release --action revise --object <release op id> --parent <release op id> --payload-json '{"artifacts": [...]}'`.

An artifact attestation is an `xyz.arkiv.attest` op with action `artifact`: `{tag, commit, name, sha256}`.
The attester posts it after downloading the artifact and checking that its sha256 and size match the record.
It hangs off the attester's attest object, the same one its `ci` attestations use.

## Program rules

`programs/ci-gated.rhai` tracks releases in the same file as the CI gate, because programs cannot import.
Releases are state; they never gate a ref.
A tag is created once: the first delegate `create` for a tag wins, and later ones, and any by a non-delegate, are ignored.
Malformed artifacts (no name, a sha256 that is not 64 lowercase hex characters, a negative or missing size) are dropped.
`state.releases[tag]` is `{object, author, commit, notes, artifacts, valid, missing}`, recomputed after every op and accepted push.
A release is valid only when `missing` is empty, that is when all of these hold:

1. Its commit has at least `need()` passing `ci` attestations from `attesters()`.
2. Its commit is an accepted `main` head, or the head of a patch with `repo.quorum` delegate approvals.
3. It has at least one artifact.
4. Every artifact has at least `need()` `artifact` attestations from `attesters()` whose `commit` and `sha256` equal the record; an attester's latest attestation for a name wins.

Accepted `main` heads come from `on_push`, which the engine calls for every accepted push, into `state.main`.
Pushes before the identity revision that set the pin are not events (docs/p3/programs.md, "Seeding"), so a re-pin hides earlier merges from `state.main`.
Rule 2 therefore also accepts an approved patch head: ops before the pin are folded normally, and an approved head with a CI pass is exactly what the gate lets onto `main`.
This is why v0.1.0 at `a83f7b5`, merged before the release rules were pinned, can still be valid.

## Commands

`chart release publish --repo <url> --tag <t> --commit <sha> --artifact <name>=<path-or-url>[@<target>] ... [--location <name>=<url>] ... [--notes <text>]` hashes each artifact and posts the record, then prints the op id.
A local path is read and hashed; a URL is downloaded, hashed and listed as a location.
Every artifact needs at least one location, and every `--location` must name a given artifact.

`chart release verify --repo <url> --tag <t> [--download-dir <d>]` folds the pinned program and refuses unless the release is valid, printing what is missing.
It then downloads each artifact from its first working location, checks sha256 and size, prints `OK <name> <sha256> <size> from <url>` and writes the file to `<d>/<name>` when asked.
It exits non-zero on any mismatch or if no location of an artifact can be downloaded.
Locations may be `https://`, `http://` or `file://`.

`scripts/chart-release-attest.sh --repo <url> --tag <t>` is the attester.
It reads the release from `chart program status`, downloads each artifact with `curl`, and posts an `artifact` attestation only if the sha256 and size match.
Artifacts this key already attested with the same payload are skipped; a mismatch is reported and not attested.
Run it with `CHARTER_HOME` set to a key listed in `attesters()`, for Charter the CI runner.

## Release gates

Before a release is tagged, these run on the merged `main` and must all pass:

- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- The performance gate, exact: `cargo test --workspace --all-features --test bench_gate`.
- The private-repo query counts: `cargo test -p charter-private --all-features --test query_counts`.
- The workspace tests: `cargo test --workspace --all-features -- --skip bench_gate`.
- The UI e2e (`crates/charter-ui/e2e/run.sh`) under `caffeinate -dims`, on AC power.
- `cargo audit` on the workspace `Cargo.lock` (`nix-shell -p cargo-audit --run 'cargo audit'`), which must find no vulnerability; accepted advisories live in `.cargo/audit.toml`, each with its reason and exit condition.

GitHub CI runs `cargo audit` on every push as well.
The Arkiv CI runner does not (docs/p3/arkiv-ci.md, "Known limits"), so the local audit is what stops a release with a known advisory.
`zk/` is outside the workspace and not in the release archives, so its lockfile is not part of this gate.

## Trust model

A valid release proves that a delegate signed the record, that the listed CI runners said the suite passed at that commit, that the commit reached `main` through the gate, and that the runners downloaded bytes with exactly these hashes.
Anyone who verifies gets exactly the bytes the delegate and the runners signed for, from any mirror.
It does not prove that the artifacts were built from that commit: the runner attests downloaded bytes, not a rebuild.
Reproducible builds, where each attester rebuilds the commit and attests the hash it got, are the stronger future mode; the op shapes already fit it.
For more trust, list several independent attesters and raise `need()`.

Large files (LFS) will reuse the same idea: a signed, content-addressed hash on Arkiv with the bytes on any mirror.

## Known limits

- Artifacts are held in memory while hashing; fine for tens of MB.
- `file://` locations are not percent-decoded.
- The attester checks the first location it can download; a bad first mirror blocks its attestation until a delegate's revise or a fixed mirror.
- `verify` uses the first working location and fails on a mismatch there rather than trying the next mirror, so a tampered mirror is always reported.
