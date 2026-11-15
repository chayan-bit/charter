# Developing Charter on Arkiv

Status: implemented and tested on the `mem:` backend; the live switch on Rabdi is run by the coordinator.
Date: 2026-11-07.

Charter's own source lives in an Arkiv repo, and its CI gate is a repo program.
The canonical repo is `arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ`, with one delegate (`zFUx2fHef7octbuyEJ4DrCArrLSRmAKwMtvf8zQa5gbKG`) and quorum 1.
GitHub is now only a read-only mirror and the host of release binaries.
The canonical source, the patches, the reviews and the CI gate all live on Arkiv.

## Workflow

Every step is a signed op or a signed push on Arkiv; nothing depends on a server.

1. Issue: anyone posts an `xyz.arkiv.issue` op describing the work.
2. Patch: the contributor runs `scripts/chart-patch.sh <url> <branch>`, which pushes the branch to the contributor's own namespace and posts an `xyz.arkiv.patch` op `{title, head, branch}`.
   Running it again after new commits revises the same patch with the new head.
3. CI attestation: a runner running `scripts/chart-ci.sh` sees the patch head, tests it, and posts an `xyz.arkiv.attest` `ci` op `{head, status, suite, log_sha256, duration_s}` signed by the runner key.
4. Approval: a delegate runs `scripts/chart-merge.sh <url> <patch>`, which first checks in `chart program status` that CI passed at the patch head and stops if it did not.
   It then posts an `xyz.arkiv.review` `approve` op `{patch, head}` unless the quorum already approved that head.
5. Merge: the same script pushes `main` to the patch head.
   The remote helper folds the pinned program with that push appended and refuses with `E_PROGRAM_REJECTED` before any transaction if the program would reject it.

## The gate: `programs/ci-gated.rhai`

`refs/heads/main` moves only to a commit that is the head of a patch with both of these at exactly that head:

- at least `repo.quorum` delegate approvals, cleared whenever the patch author revises the head;
- at least `need()` passing `ci` attestations from the nids listed in `attesters()`.

Only the patch author may revise a patch.
An attester's latest attestation for a head wins, so a later `fail` overrides an earlier `pass` from the same runner, and a later re-run can turn it back into a pass.
Attestations from keys outside `attesters()`, and attestations for any other commit, are ignored.
A rejection says what is missing, for example `main at <head> needs 1 passing CI attestations (has 0)`, or the delegate approvals, or both.
Other refs follow the default rule (fast-forward of the accepted head).
The defaults are the Charter CI runner `z76sCb7au55NQrrbUC8YdoGoLSnwqw5vBvFzumNtjCBoi` as the only attester and `need() = 1`.
Programs take hex nids; `charter_sync::ids::parse_id` converts the `z` form.
The state JSON (`program status`) carries `ci: {head: {attester: status}}`, `patches`, `need` and `quorum`, which is what `chart-merge.sh` reads.
It also carries `main` (accepted main heads) and `releases`, the signed release records (docs/p3/releases.md).

## Trust model

A CI attestation means exactly this: "this runner key says the suite passed at this head".
It does not prove that the suite ran; it binds a named key to that claim on a public, append-only log.
For more trust, list several runners run by different people in `attesters()` and raise `need()` to k of n.
A lying runner is caught by anyone who re-runs the suite at the same head and publishes a contradicting attestation, which stays on Arkiv next to the lie.
The delegates then drop the lying key from `attesters()` and re-pin, which is an ordinary identity revision.
`log_sha256` commits the runner to one exact log, so a runner that later publishes its log cannot swap it for another.
The delegate approval stays a separate condition, so a runner alone can never move `main`.

## Running the runner

```sh
export CHARTER_HOME=~/.config/chart/ci-runner   # the runner identity, funded
scripts/chart-ci.sh --repo arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ          # loop
scripts/chart-ci.sh --repo arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ --once   # one pass
```

Each round lists the ops and fetches the canonical `main`.
It skips patch heads already in `main` (for example patches merged before the gate was pinned) and heads this runner already attested.
For every other patch head it fetches the author's branches from `<url>/<author nid>`, checks the head out in a clean detached worktree and runs the suite there.
The default suite is the cargo part of `.github/workflows/ci.yml`: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo test --workspace --all-features`, wrapped in `mac-heavy` when it is on `PATH`.
`CHARTER_CI_CMD` replaces the suite (run with `sh -c`), `CHARTER_CI_SUITE` names it in the payload (default: the command), `CHARTER_CI_DIR` moves the cache (default `~/.cache/charter-ci`, or `~/.cache/arkiv-ci` if only that one exists), and `CHARTER_CI_INTERVAL` sets the seconds between rounds (default 60).
The pre-rename `ARKIV_CI_*` names still work as fallbacks.
Logs stay in the cache under `logs/<head>.log`; the payload carries their SHA-256.
The script posts `pass` only when the suite exits 0 and `fail` for exit codes 1 to 128.
A suite killed by a signal, an interrupted runner, or a failed fetch or checkout posts nothing, and the head is retried in the next round.
It needs `chart`, `git` and `jq` on `PATH`.

## Cost per patch

These are estimates from the live spend records in `docs/live/`, to be confirmed by the first live patch.
A small op costs roughly 0.0002 to 0.0005 tGLM, scaled from the measured 0.0027 tGLM of a 63 KB blob op, and a small push spends about 0.001 tGLM.
One patch takes a branch push and a patch op (contributor), one attestation op (runner, plus one `create` op the first time), and a review `create`, an `approve` and the `main` push (delegate).
That is about 0.003 to 0.005 tGLM per patch in total, spread over the three keys.
A push the program rejects costs nothing, because the helper refuses it before sending a transaction.

## GitHub mirror

`scripts/mirror-to-github.sh [url] [remote]` fetches the canonical `main` from Arkiv and pushes it to the GitHub remote (default `origin`).
It refuses when GitHub's `main` is not an ancestor of the Arkiv `main`, and it never force-pushes, so the mirror can only fast-forward.
Changes reach GitHub only through this script; pull requests on GitHub are not merged there.

## Known limits

- The runner trusts its own machine; the suite runs unsandboxed with the runner user's permissions, so run it on a dedicated host or account.
- Logs are not published on Arkiv; only their hash is.
- A head is attested once per runner; to re-run a flaky head, post a new attestation by hand or revise the patch.
- `chart-patch.sh` pushes without force, so a rebased branch needs a manual `git push --force` to the author's namespace before it is re-posted.
- The cost figures above are estimates until the first live patch records real spend.
- The runner's suite leaves out `cargo audit`: it needs a network fetch of the advisory database, and a failed fetch exits 1, so the runner would post a `fail` for a network error; its verdict also changes as advisories are published, so it would not be a claim about the head alone.
  GitHub CI and the local release gates (docs/p3/releases.md, "Release gates") run it instead.
