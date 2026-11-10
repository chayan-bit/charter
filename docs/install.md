# Install

Prebuilt binaries are attached to every GitHub Release, so no Nix or Rust build is needed.
Each archive holds `git-remote-arkiv`, `chart` and `chart-mcp` plus the licence files.
Two targets are published: `aarch64-apple-darwin` (Apple Silicon macOS) and `x86_64-unknown-linux-gnu` (Linux x86_64, glibc 2.35 or newer).

## Download URL pattern

```
https://github.com/chayan-bit/charter/releases/download/<tag>/charter-<tag>-<target>.tar.gz
https://github.com/chayan-bit/charter/releases/download/<tag>/SHA256SUMS
```

For v0.3.0 on Apple Silicon macOS the archive is `https://github.com/chayan-bit/charter/releases/download/v0.3.0/charter-v0.3.0-aarch64-apple-darwin.tar.gz`.
For v0.3.0 on Linux x86_64 the archive is `https://github.com/chayan-bit/charter/releases/download/v0.3.0/charter-v0.3.0-x86_64-unknown-linux-gnu.tar.gz`.
v0.1.0 predates the rename to Charter: its archives are named `arkiv-git-v0.1.0-<target>.tar.gz` and hold `git-remote-arkiv`, `arkiv-git` and `arkiv-git-mcp`.
A combined `SHA256SUMS` file is attached to the release as well.

## Install snippet

```sh
TAG=v0.3.0
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
