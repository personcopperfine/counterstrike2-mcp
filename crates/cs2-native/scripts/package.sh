#!/bin/bash
# package.sh [--os windows|linux] [--out DIR] [--no-build] [--archive]: lay out a self-contained
# cs2mcp-native directory (FIXME 13), the layout `Artifacts::resolve` (payload/) and the servers'
# `<exe dir>/sigs` fallback read. It is the portable zip/tarball as is, the MSI installs it as is,
# and the AppImage carries it at $APPDIR/opt/cs2mcp (crates/cs2-launcher/packaging/):
#
#   <out>/cs2mcp-launcher[.exe]          the desktop app (crates/cs2-launcher)
#   <out>/cs2mcp-native[.exe]  client-mcp[.exe]  hammer-mcp[.exe]
#   <out>/sigs/                          sigs/signatures (the servers' signature DB)
#   <out>/payload/bridge-rs/<plat>/      bridge/build-rs.sh output (win64 | linuxsteamrt64)
#   <out>/payload/metamod/<plat>/addons/ the Metamod build (win64 | linux)
#   <out>/payload/gbe/<plat>/            scripts/stage-gbe.sh output (win64 | linux64), + GBE_REF.txt
#   <out>/payload/vklayer/<plat>/        the capture layer (crates/cs2-vk-capture, built here):
#                                        VkLayer_cs2mcp_capture.json + cs2_vk_capture.dll | libcs2_vk_capture.so
#   <out>/BUILDINFO.txt                  git sha, toolchain, the pins gate (bridge/build-rs.sh's
#                                        PINS_GATE.txt), gbe tag, Metamod source/version + sha256,
#                                        the hl2sdk (s2sdk) commit; "not code-signed"
#   <out>/SHA256SUMS                     every file above (payload.rs verifies payload/ with it)
#
# --archive: also write target/dist/cs2mcp-<version>-<os>-x86_64.{zip|tar.gz} (VERSION, default
# `git describe --tags --always --dirty`).
#
# Inputs (each must exist; nothing is fetched): BRIDGE_DIR (default <repo>/target/bridge-rs),
# METAMOD_DIR (<repo>/target/metamod, else the main checkout's: bridge/lib-root.sh; win64 comes
# from bridge/metamod/fetch-metamod-win64.sh, linux from build-metamod.sh linux),
# GBE_DIR (<repo>/target/gbe).
# The binaries are built with cargo --release (-j${JOBS:-2}, the OOM guard) unless --no-build.
# The dev tool cs2mcp-native-dev is not shipped. Valve binaries never go in: the payload is ours
# (bridge, launcher, Metamod) and gbe_fork's; the game's own files stay in the user's install.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
os=windows out="" build=1 archive=0
while [ $# -gt 0 ]; do
    case "$1" in
        --os) os="$2"; shift 2 ;;
        --out) out="$2"; shift 2 ;;
        --no-build) build=0; shift ;;
        --archive) archive=1; shift ;;
        *) echo "package.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
case "$os" in
    windows) triple=x86_64-pc-windows-msvc ext=.exe bplat=win64 mplat=win64 gplat=win64 vplat=win64
             vlib=cs2_vk_capture.dll vmanifest=windows/VkLayer_cs2mcp_capture.json ;;
    linux) triple="" ext="" bplat=linuxsteamrt64 mplat=linux gplat=linux64 vplat=linux64
           vlib=libcs2_vk_capture.so vmanifest=VkLayer_cs2mcp_capture.json ;;
    *) echo "package.sh: --os windows|linux" >&2; exit 2 ;;
esac
out="${out:-$repo/target/cs2mcp-native-$os}"
bridge="${BRIDGE_DIR:-$repo/target/bridge-rs}"
gbe="${GBE_DIR:-$repo/target/gbe}"
metamod="${METAMOD_DIR:-$repo/target/metamod}"
# shellcheck source=../../../bridge/lib-root.sh
. "$repo/bridge/lib-root.sh"
[ -d "$metamod/$mplat" ] || metamod="$(cs2mcp_main_checkout "$repo")/target/metamod"

need() { [ -e "$1" ] || { echo "package.sh: missing $1 ($2)" >&2; exit 1; }; }
need "$bridge/$bplat" "bridge/build-rs.sh, or set BRIDGE_DIR"
need "$metamod/$mplat/addons/metamod" "the Metamod build, or set METAMOD_DIR"
need "$gbe/$gplat" "scripts/stage-gbe.sh, or set GBE_DIR"

if [ -n "$triple" ]; then rel="$repo/target/$triple/release"; tflag=(--target "$triple"); else rel="$repo/target/release"; tflag=(); fi
if [ "$build" = 1 ]; then
    (cd "$repo" && cargo build --release -j"${JOBS:-2}" "${tflag[@]}" \
        -p cs2-native -p client-mcp -p hammer-mcp -p cs2-launcher \
        --bin cs2mcp-native --bin client-mcp --bin hammer-mcp --bin cs2mcp-launcher)
    (cd "$repo" && cargo build --release -j"${JOBS:-2}" "${tflag[@]}" -p cs2-vk-capture --lib)
fi
bins=(cs2mcp-launcher cs2mcp-native client-mcp hammer-mcp)
for b in "${bins[@]}"; do need "$rel/$b$ext" "cargo build --release ${tflag[*]}"; done
need "$rel/$vlib" "cargo build --release ${tflag[*]} -p cs2-vk-capture --lib"

# a fresh directory: a stale payload file from an earlier run must not ship
rm -rf "$out"
mkdir -p "$out/payload/bridge-rs" "$out/payload/metamod/$mplat" "$out/payload/gbe" "$out/payload/vklayer/$vplat"
for b in "${bins[@]}"; do cp "$rel/$b$ext" "$out/"; done
cp -r "$repo/sigs/signatures" "$out/sigs"
cp -r "$bridge/$bplat" "$out/payload/bridge-rs/$bplat"
cp -r "$metamod/$mplat/addons" "$out/payload/metamod/$mplat/addons"
# the Metamod provenance: build-metamod.sh's BUILDINFO.txt (linux), fetch-metamod-win64.sh's (win64)
case "$mplat" in
    linux) cp -f "$metamod/BUILDINFO.txt" "$out/payload/metamod/BUILDINFO-linux.txt" 2>/dev/null || true ;;
    win64) cp -f "$metamod/win64/BUILDINFO-download.txt" "$out/payload/metamod/BUILDINFO-win64.txt" 2>/dev/null || true ;;
esac
cp -r "$gbe/$gplat" "$out/payload/gbe/$gplat"
cp -f "$gbe/GBE_REF.txt" "$out/payload/gbe/" 2>/dev/null || true
cp "$rel/$vlib" "$out/payload/vklayer/$vplat/"
cp "$repo/crates/cs2-vk-capture/$vmanifest" "$out/payload/vklayer/$vplat/VkLayer_cs2mcp_capture.json"
version="${VERSION:-$(git -C "$repo" describe --tags --always --dirty 2>/dev/null || echo unknown)}"
kv() { sed -n "s/^$1=//p" "$2" 2>/dev/null | head -n1; }
{
    echo "cs2-mcp $version ($os, x86_64)"
    echo "git_sha=$(git -C "$repo" rev-parse HEAD 2>/dev/null || echo unknown)$(git -C "$repo" diff --quiet HEAD 2>/dev/null || echo ' (dirty tree)')"
    echo "built_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "rustc=$(rustc -V 2>/dev/null || echo unknown)"
    echo "cargo=$(cargo -V 2>/dev/null || echo unknown)"
    echo "target=${triple:-$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')}"
    echo "game_build_targeted=$(tr -d '[:space:]' < "$repo/sigs/CURRENT_BUILD")"
    echo "pins_gate=$(cat "$bridge/PINS_GATE.txt" 2>/dev/null || echo "unknown (no $bridge/PINS_GATE.txt: the bridge was not built by this build-rs.sh)")"
    echo "gbe=$(tr '\n' ' ' < "$gbe/GBE_REF.txt" 2>/dev/null | sed 's/ *$//' || true)"
    if [ "$mplat" = win64 ]; then
        f="$metamod/win64/BUILDINFO-download.txt"
        echo "metamod_source=official AlliedModders dev build (downloaded; not built here)"
        echo "metamod_version=$(kv version "$f")"
        echo "metamod_url=$(kv url "$f")"
        echo "metamod_archive_sha256=$(kv archive_sha256 "$f")"
    else
        f="$metamod/BUILDINFO.txt"
        echo "metamod_source=built from source (bridge/metamod/build-metamod.sh linux)"
        echo "metamod_commit=$(kv metamod_commit "$f")"
        echo "metamod_version=$(kv metamod_describe "$f")"
        echo "hl2sdk_commit=$(kv s2sdk_commit "$f") (alliedmodders/s2sdk, branch $(kv s2sdk_branch "$f"))"
    fi
    for m in $(cd "$out/payload/metamod/$mplat" && find addons -type f | sort); do
        echo "metamod_file_sha256=$(sha256sum "$out/payload/metamod/$mplat/$m" | cut -d' ' -f1)  $m"
    done
    echo "code_signing=none: the binaries are not signed (Windows SmartScreen will warn)"
} > "$out/BUILDINFO.txt"
(cd "$out" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
echo "package.sh: $os package in $out ($(wc -l < "$out/SHA256SUMS") files, $(du -sh --apparent-size "$out" | cut -f1))"
if [ "$archive" = 1 ]; then
    dist="$repo/target/dist"
    mkdir -p "$dist"
    base="cs2mcp-$version-$os-x86_64"
    parent="$(dirname "$out")" name="$(basename "$out")"
    if [ "$os" = windows ]; then
        rm -f "$dist/$base.zip"
        # the zip unpacks into a folder named like the archive
        (cd "$parent" && rm -rf "$base.stage" && mkdir "$base.stage" && cp -a "$name" "$base.stage/$base" \
            && cd "$base.stage" && python3 -I -m zipfile -c "$dist/$base.zip" "$base" && cd .. && rm -rf "$base.stage")
        echo "package.sh: $dist/$base.zip"
    else
        tar -C "$parent" --transform "s|^$name|$base|" --owner=0 --group=0 -czf "$dist/$base.tar.gz" "$name"
        echo "package.sh: $dist/$base.tar.gz"
    fi
fi
