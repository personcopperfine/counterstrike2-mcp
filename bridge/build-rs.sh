#!/bin/bash
# Build the Rust bridge and the Metamod loader for both targets, stage the artefacts and gate their
# exports.
#
#   target/bridge-rs/win64/cs2_bridge.dll              <- cs2.exe -tools (Wine) loads this (WP0.4)
#   target/bridge-rs/win64/cs2mcp_launch.exe           <- runtime.sh's entrypoint runs this instead of
#   target/bridge-rs/win64/cs2mcp_mmload.dll              cs2.exe; it injects the loader DLL
#   target/bridge-rs/linuxsteamrt64/libcs2_bridge.so
#   target/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so <- LD_PRELOADed on the cs2.sh line
#
# All are release builds with the memory-bounded cargo form the repo rules require (see
# AGENTS.md: no `--workspace`, `CARGO_TARGET_DIR` inside the checkout, a virtual-memory cap and a
# deadline). Each result is gated: `bridge/check-exports.sh` (one exported symbol,
# `CreateInterface`) for the bridge, `bridge/check-loader-exports.sh` (`DllMain` / `dlopen`) for the
# loader, and this script fails on either.
#
# The launcher and the loader are, apart from the two libraries above, the whole gameinfo-free
# Metamod path (`target/DESIGN.md` §1): the launcher is a Windows-only binary (CONTRACT), so the
# Linux build of `mm-launch` contributes only the .so staged here.
#
# The Windows half needs the xwin headers/libs and `lld-link`; that is configured in
# `.cargo/config.toml`, so plain `cargo build --target` works from the repo root.
#
# --no-pins-check: skip the pins gate below (a CI runner has no game binaries to check against).
# It is never silent: the line "pins gate SKIPPED: no game binaries" is printed and written to
# target/bridge-rs/PINS_GATE.txt, which scripts/package.sh copies into the package's BUILDINFO.txt.
# The export gates still run. Releases built this way rely on the pins the maintainer checked
# locally before tagging (RELEASING.md).
set -euo pipefail

pins_check=1
for a in "$@"; do
    case "$a" in
        --no-pins-check) pins_check=0 ;;
        *) echo "build-rs.sh: unknown argument $a (only --no-pins-check)" >&2; exit 2 ;;
    esac
done

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
OUT="$ROOT/target/bridge-rs"
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

# Gate first: the pins must be up to date for the header the C++-side numbers come from and
# the pins.rs the crate embeds (crates/cs2-bridge/src/generated/pins.rs, wired into lib.rs) -
# a comment appended to pins.rs or an edit to bridge_pins.h fails the build here, before any
# cargo work. Plain `python3` (as the retired bridge/build.sh ran pins.py: the resolver's numpy lives in
# the user site-packages, which `python3 -I` does not see).
mkdir -p "$OUT"
pin_build="$(sed -n 's/^pub const PIN_GAME_BUILD: [^=]*= *"\{0,1\}\([0-9]*\)"\{0,1\};.*/\1/p' "$ROOT/crates/cs2-bridge/src/generated/pins.rs" | head -n1)"
if [ "$pins_check" = 1 ]; then
    echo "build-rs.sh: pins.py --check --rust crates/cs2-bridge/src/generated/pins.rs"
    python3 "$HERE/pins.py" --check --rust "$ROOT/crates/cs2-bridge/src/generated/pins.rs"
    echo "pins gate: passed (pins.py --check against the game binaries; pins for game build ${pin_build:-?})" > "$OUT/PINS_GATE.txt"
else
    echo "build-rs.sh: pins gate SKIPPED: no game binaries (--no-pins-check)"
    echo "pins gate SKIPPED: no game binaries (built with --no-pins-check; pins.rs as committed, for game build ${pin_build:-?})" > "$OUT/PINS_GATE.txt"
fi

build() {
    local package="$1" target="$2"
    echo "build-rs.sh: cargo build --release -p $package --target $target"
    (
        ulimit -v 6000000
        timeout 900 cargo build --release -p "$package" --target "$target" -j2
    )
}

# The bridge and the loader/launcher. Two packages, two targets each; the Linux build of
# `mm-launch` produces the interposer .so plus the "windows only" stub bin (which is not staged).
for target in x86_64-pc-windows-msvc x86_64-unknown-linux-gnu; do
    build cs2-bridge "$target"
    build mm-launch "$target"
done

WINDOWS_DLL="$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/cs2_bridge.dll"
LINUX_SO="$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/libcs2_bridge.so"
WINDOWS_LAUNCHER="$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/cs2mcp_launch.exe"
WINDOWS_LOADER="$CARGO_TARGET_DIR/x86_64-pc-windows-msvc/release/cs2mcp_mmload.dll"
LINUX_LOADER="$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/libcs2mcp_mmload.so"

for f in "$WINDOWS_DLL" "$LINUX_SO" "$WINDOWS_LAUNCHER" "$WINDOWS_LOADER" "$LINUX_LOADER"; do
    [ -f "$f" ] || { echo "build-rs.sh: $f was not built" >&2; exit 1; }
done

mkdir -p "$OUT/win64" "$OUT/linuxsteamrt64"
cp "$WINDOWS_DLL" "$OUT/win64/cs2_bridge.dll"
cp "$WINDOWS_LAUNCHER" "$OUT/win64/cs2mcp_launch.exe"
cp "$WINDOWS_LOADER" "$OUT/win64/cs2mcp_mmload.dll"
cp "$LINUX_SO" "$OUT/linuxsteamrt64/libcs2_bridge.so"
cp "$LINUX_LOADER" "$OUT/linuxsteamrt64/libcs2mcp_mmload.so"
echo "build-rs.sh: staged $OUT/win64/{cs2_bridge.dll,cs2mcp_launch.exe,cs2mcp_mmload.dll}" \
     "and $OUT/linuxsteamrt64/{libcs2_bridge.so,libcs2mcp_mmload.so}"

"$HERE/check-exports.sh" "$OUT/win64/cs2_bridge.dll"
"$HERE/check-exports.sh" "$OUT/linuxsteamrt64/libcs2_bridge.so"
"$HERE/check-loader-exports.sh" "$OUT/win64/cs2mcp_mmload.dll"
"$HERE/check-loader-exports.sh" "$OUT/linuxsteamrt64/libcs2mcp_mmload.so"
