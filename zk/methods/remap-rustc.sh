#!/bin/sh
# rustc wrapper for the guest build (.cargo/config.toml, scripts/zk-prove.sh), so the guest
# image id does not depend on where it was built:
# - CARGO_HOME, the repo root and build-script OUT_DIRs are remapped to fixed prefixes
#   (panic locations embed source paths);
# - for guest crates in this repo, cargo's `-C metadata` hashes their absolute path (they sit
#   outside the guest workspace), which changes symbol hashes and code layout, so it is
#   replaced by the crate's name and version.
# ZK_INNER_RUSTC_WRAPPER chains another wrapper (sccache) when RUSTC_WRAPPER is taken by this one.
rustc=$1
shift
root_l=$(cd "$(dirname "$0")/../.." && pwd -L)
root_p=$(cd "$root_l" && pwd -P)
guest=
for a; do [ "$a" = riscv32im-risc0-zkvm-elf ] && guest=1; done
case ${CARGO_MANIFEST_DIR:-} in
  "$root_l"/* | "$root_p"/*) ;;
  *) guest= ;;
esac
if [ -n "$guest" ]; then
  n=$#
  while [ "$n" -gt 0 ]; do
    a=$1
    shift
    case $a in metadata=*) a="metadata=charter-$CARGO_PKG_NAME-$CARGO_PKG_VERSION" ;; esac
    set -- "$@" "$a"
    n=$((n - 1))
  done
fi
cargo_home=${CARGO_HOME:-$HOME/.cargo}
exec ${ZK_INNER_RUSTC_WRAPPER:+"$ZK_INNER_RUSTC_WRAPPER"} "$rustc" \
  "--remap-path-prefix=$cargo_home=/cargo" \
  "--remap-path-prefix=$root_l=/charter" \
  "--remap-path-prefix=$root_p=/charter" \
  ${OUT_DIR:+"--remap-path-prefix=$OUT_DIR=/out"} \
  "$@"
