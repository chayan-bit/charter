# Contributing

## Develop Charter on Arkiv

The canonical source of Charter is the Arkiv repo `arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ`.
GitHub is a read-only mirror plus the host of release binaries, so changes land through Arkiv, not through GitHub pull requests.
The full workflow and trust model are in `docs/p3/arkiv-ci.md`.

1. Install Charter (`docs/install.md`), create your key with `chart keys init`, send testnet funds to the wallet it prints (`chart fund` estimates how much), and clone with `git clone arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ charter`.
2. Commit on a branch based on the current `main`, then open or update a patch with `scripts/chart-patch.sh <url> <branch>`, which prints the patch id.
3. The CI runner (`scripts/chart-ci.sh`) tests the patch head and posts a signed attestation; check it with `chart op list --repo <url> --type xyz.arkiv.attest`.
4. A delegate merges with `scripts/chart-merge.sh <url> <patch-id>`, which works only once CI passed at the patch head.
   A direct push to `main` is refused with `E_PROGRAM_REJECTED` until the patch head has a delegate approval and a CI pass.
5. A maintainer mirrors `main` to GitHub with `scripts/mirror-to-github.sh`.

Before posting a patch, run the same checks locally: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo test --workspace --all-features`.
Use conventional commits (`feat:`, `fix:`, `docs:` and so on), and keep Markdown at one sentence per line without em dashes.
