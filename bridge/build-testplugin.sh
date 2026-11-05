#!/bin/bash
# Build the coexistence fixture plugin (crates/mm-testplugin) for both targets, stage the two
# libraries and gate their exports.
#
#   target/testplugin/win64/mm_testplugin.dll
#   target/testplugin/linuxsteamrt64/libmm_testplugin.so
#
# Both are release builds with the memory-bounded cargo form the repo rules require (see
# AGENTS.md: no `--workspace`, `CARGO_TARGET_DIR` inside the checkout, a virtual-memory cap and
# a deadline) - the same form `bridge/build-rs.sh` runs for the bridge - and
# `bridge/check-exports.sh` runs on each result: exactly one exported symbol,
# `CreateInterface`, or this script fails. The orchestrator installs the staged pair with a
# test-only vdf; this script does not write one.
#
# The Windows half needs the xwin headers/libs and `lld-link`; that is configured in
# `.cargo/config.toml`, so plain `cargo build --target` works from the repo root.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
OUT="$ROOT/target/testplugin"
export CARGO_TARGET_DIR="$ROOT/target/cargo"

# The target flags (linker, xwin lib paths, +crt-static) live in `.cargo/config.toml`, and an
# environment `RUSTFLAGS`/`CARGO_ENCODED_RUSTFLAGS` would *replace* them: the build would then
# either fail for a missing linker or link a DLL that depends on vcruntime140.dll. The config is
# the only source of those flags, so the environment is cleared here. Nested cargo calls inherit
# this.
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS

# rustflags from `.cargo/config.toml` are picked up relative to the *current* directory, so the
# builds run from the root.
cd "$ROOT"

build() {
    local target="$1"
    echo "build-testplugin.sh: cargo build --release -p mm-testplugin --target $target"
    (
        ulimit -v 6000000
        timeout 900 cargo build --release -p mm-testplugin --target "$target" -j2
    )
}

build x86_64-pc-windows-msvc
build x86_64-unknown-linux-gnu

WINDOWS_DLL="$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/mm_testplugin.dll"
LINUX_SO="$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/libmm_testplugin.so"

for f in "$WINDOWS_DLL" "$LINUX_SO"; do
    [ -f "$f" ] || { echo "build-testplugin.sh: $f was not built" >&2; exit 1; }
done

mkdir -p "$OUT/win64" "$OUT/linuxsteamrt64"
cp "$WINDOWS_DLL" "$OUT/win64/mm_testplugin.dll"
cp "$LINUX_SO" "$OUT/linuxsteamrt64/libmm_testplugin.so"
echo "build-testplugin.sh: staged $OUT/win64/mm_testplugin.dll and $OUT/linuxsteamrt64/libmm_testplugin.so"

"$HERE/check-exports.sh" "$OUT/win64/mm_testplugin.dll"
"$HERE/check-exports.sh" "$OUT/linuxsteamrt64/libmm_testplugin.so"
