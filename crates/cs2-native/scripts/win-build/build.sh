#!/bin/bash
# build.sh [--msi-tools]: build the Windows package (package.sh --os windows --archive) in a
# container, from a clean clone of this checkout's HEAD. Only the results come out, to
# target/win-build/out/: cs2mcp-native-windows/ (the package dir), the zip, BUILDINFO.txt, and
# with --msi-tools tools/bin/cargo-packager.exe (cargo-packager 0.11.8 for the MSI on Windows).
#
# The xwin sysroot (Microsoft's CRT + Windows SDK, `xwin --accept-license`) lives in the podman
# volume cs2mcp-xwin and is linked as target/xwin inside the container only; the cargo registry
# and target dir are volumes too (cs2mcp-win-cargo, cs2mcp-win-target). The pins gate runs
# against target/game-bins + target/game-bins-linux-<CURRENT_BUILD> (read-only mounts); without
# them, NO_PINS_CHECK=1 builds with --no-pins-check (recorded as SKIPPED). Inputs, read-only:
# target/metamod (fetch-metamod.sh win64), target/gbe (stage-gbe.sh).
# podman runs with XDG_RUNTIME_DIR=<project root>/.podman/xdg (crates/cs2-native/scripts/lib-root.sh).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../../.." && pwd)"
msi_tools=0
for a in "$@"; do
    case "$a" in
        --msi-tools) msi_tools=1 ;;
        *) echo "build.sh: unknown argument $a" >&2; exit 2 ;;
    esac
done
# shellcheck source-path=SCRIPTDIR source=../lib-root.sh
. "$repo/crates/cs2-native/scripts/lib-root.sh"
XDG_RUNTIME_DIR="$(cs2mcp_project_root "$repo")/.podman/xdg"
export XDG_RUNTIME_DIR
build="$(cat "$repo/sigs/CURRENT_BUILD")"
img=localhost/cs2mcp-win-build:latest
out="$repo/target/win-build/out"
need() { [ -e "$1" ] || { echo "build.sh: missing $1 ($2)" >&2; exit 1; }; }
need "$repo/target/metamod/win64/addons/metamod" "crates/metamod/fetch-metamod.sh win64"
need "$repo/target/gbe/win64/steam_api64.dll" "crates/cs2-native/scripts/stage-gbe.sh"
mounts=(-v "$repo:$repo:ro" -v "$repo/target/metamod:/in/metamod:ro" -v "$repo/target/gbe:/in/gbe:ro")
# a linked worktree's .git names the main checkout's: mount that too (read-only)
common="$(git -C "$repo" rev-parse --path-format=absolute --git-common-dir)"
[ "$common" = "$repo/.git" ] || mounts+=(-v "$common:$common:ro")
pins=1
if [ "${NO_PINS_CHECK:-0}" = 1 ]; then
    pins=0
else
    need "$repo/target/game-bins" "sigs/README.md \"Binaries\", or NO_PINS_CHECK=1"
    need "$repo/target/game-bins-linux-$build" "sigs/README.md \"Linux binaries\", or NO_PINS_CHECK=1"
    mounts+=(-v "$repo/target/game-bins:/in/game-bins:ro" -v "$repo/target/game-bins-linux-$build:/in/game-bins-linux:ro")
fi
podman build --network=host -t "$img" "$here"
rm -rf "$out"
mkdir -p "$out"
podman run --rm --network=host "${mounts[@]}" \
    -v cs2mcp-xwin:/xwin -v cs2mcp-win-cargo:/usr/local/cargo/registry -v cs2mcp-win-target:/work/src/target \
    -v "$out:/out" -e PINS="$pins" -e MSI_TOOLS="$msi_tools" -e BUILD="$build" -e SRC="$repo" -e JOBS="${JOBS:-4}" \
    "$img" bash -euo pipefail -c '
        if [ ! -d /xwin/crt/lib/x86_64 ]; then
            echo "build.sh: fetching the xwin sysroot into the cs2mcp-xwin volume (accepting Microsoft'"'"'s license)"
            xwin --accept-license --cache-dir /xwin/.cache --arch x86_64 splat --output /xwin/sysroot.tmp
            mv /xwin/sysroot.tmp/* /xwin/ && rmdir /xwin/sysroot.tmp && rm -rf /xwin/.cache
        fi
        # a clean checkout of HEAD; target/ is the cs2mcp-win-target volume (cargo caches stay)
        cd /work/src
        git init -q . && git fetch -q "$SRC" HEAD && git checkout -q --detach FETCH_HEAD
        echo "build.sh: building $(git rev-parse --short HEAD) ($(git log -1 --format=%s))"
        ln -sfn /xwin target/xwin
        rm -rf target/bridge-rs target/cs2mcp-native-windows target/dist
        if [ "$PINS" = 1 ]; then
            ln -sfn /in/game-bins target/game-bins && ln -sfn /in/game-bins-linux "target/game-bins-linux-$BUILD"
            cargo bridge build
        else
            cargo bridge build --no-pins-check
        fi
        METAMOD_DIR=/in/metamod GBE_DIR=/in/gbe crates/cs2-native/scripts/package.sh --os windows --archive
        cp -r target/cs2mcp-native-windows /out/
        cp target/dist/*.zip target/cs2mcp-native-windows/BUILDINFO.txt /out/
        if [ "$MSI_TOOLS" = 1 ]; then
            CARGO_TARGET_DIR=target/tools cargo install cargo-packager --version 0.11.8 --locked \
                --target x86_64-pc-windows-gnu --root /out/tools
        fi
        ls -l /out
    '
echo "build.sh: $out"
