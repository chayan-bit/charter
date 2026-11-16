#!/usr/bin/env bash
# Builds the sealed_patch guest and proves the demo relation on THIS machine (CPU, Metal on macOS).
# Usage: scripts/zk-prove.sh [base patch spec out-dir]   (defaults: zk/demo/*, zk/out)
# Needs the pinned Rust toolchain (rust-toolchain.toml); on the shared Mac prefix with `mac-heavy`.
# The demo key is generated in memory by zk-prover and never written to disk.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
tc=$root/.zk-toolchain
export RISC0_HOME=$tc/risc0
export CARGO_TARGET_DIR=$root/target
unset RISC0_DEV_MODE
[[ $(uname) == Darwin ]] && export PATH=$root/scripts/macos-shims:$PATH
timeout_s=${ZK_PROVE_TIMEOUT_S:-1800}

if [[ ! -d $RISC0_HOME/toolchains ]]; then
  # Project-local toolchain: rzup from crates.io, then the RISC Zero rustc into $RISC0_HOME.
  cargo install rzup --version 0.5.2 --locked --root "$tc"
  "$tc/bin/rzup" install rust
fi

base=${1:-$root/zk/demo/base.rs}
patch=${2:-$root/zk/demo/patch.txt}
spec=${3:-$root/zk/demo/spec.txt}
out=${4:-$root/zk/out}

# The remap wrapper must see every guest rustc call; an env RUSTC_WRAPPER (mac-heavy's sccache)
# would override zk/methods/.cargo/config.toml, so take its place and chain it.
wrap=$root/zk/methods/remap-rustc.sh
if [[ ${RUSTC_WRAPPER:-} != "$wrap" ]]; then
  export ZK_INNER_RUSTC_WRAPPER=${RUSTC_WRAPPER:-} RUSTC_WRAPPER=$wrap
fi
cargo build --release --manifest-path "$root/zk/Cargo.toml" -p zk-prover
# The image id must not depend on this machine: no home or repo path may reach the guest ELF.
elf=$CARGO_TARGET_DIR/riscv-guest/methods/sealed_patch/riscv32im-risc0-zkvm-elf/release/sealed_patch
if grep -qaF -e "$HOME/" -e "$root/" -e "${CARGO_HOME:-$HOME/.cargo}/" "$elf"; then
  echo "zk-prove: $elf embeds a machine path; the image id would not reproduce" >&2
  exit 1
fi
# perl alarm bounds the runtime (macOS has no coreutils timeout); time -l / -v reports peak RSS.
time_flag=-l; [[ $(uname) == Linux ]] && time_flag=-v
/usr/bin/time "$time_flag" perl -e 'alarm shift; exec @ARGV' "$timeout_s" \
  "$CARGO_TARGET_DIR/release/zk-prover" "$base" "$patch" "$spec" "$out"
