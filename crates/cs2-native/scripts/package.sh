#!/bin/bash
# package.sh [--os windows|linux] [--out DIR] [--no-build]: lay out a self-contained cs2mcp-native
# directory (FIXME 13), the layout `Artifacts::resolve` (payload/) and the servers' `<exe dir>/sigs`
# fallback read:
#
#   <out>/cs2mcp-native[.exe]  client-mcp[.exe]  hammer-mcp[.exe]
#   <out>/sigs/                          sigs/signatures (the servers' signature DB)
#   <out>/payload/bridge-rs/<plat>/      bridge/build-rs.sh output (win64 | linuxsteamrt64)
#   <out>/payload/metamod/<plat>/addons/ the Metamod build (win64 | linux)
#   <out>/payload/gbe/<plat>/            scripts/stage-gbe.sh output (win64 | linux64), + GBE_REF.txt
#   <out>/payload/vklayer/<plat>/        the capture layer (crates/cs2-vk-capture, built here):
#                                        VkLayer_cs2mcp_capture.json + cs2_vk_capture.dll | libcs2_vk_capture.so
#   <out>/SHA256SUMS
#
# Inputs (each must exist; nothing is fetched): BRIDGE_DIR (default <repo>/target/bridge-rs),
# METAMOD_DIR (<repo>/target/metamod, else the main checkout's: bridge/lib-root.sh),
# GBE_DIR (<repo>/target/gbe).
# The binaries are built with cargo --release (-j${JOBS:-2}, the OOM guard) unless --no-build.
# The dev tool cs2mcp-native-dev is not shipped. Valve binaries never go in: the payload is ours
# (bridge, launcher, Metamod) and gbe_fork's; the game's own files stay in the user's install.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
os=windows out="" build=1
while [ $# -gt 0 ]; do
    case "$1" in
        --os) os="$2"; shift 2 ;;
        --out) out="$2"; shift 2 ;;
        --no-build) build=0; shift ;;
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
        -p cs2-native -p client-mcp -p hammer-mcp --bin cs2mcp-native --bin client-mcp --bin hammer-mcp)
    (cd "$repo" && cargo build --release -j"${JOBS:-2}" "${tflag[@]}" -p cs2-vk-capture --lib)
fi
for b in cs2mcp-native client-mcp hammer-mcp; do need "$rel/$b$ext" "cargo build --release ${tflag[*]}"; done
need "$rel/$vlib" "cargo build --release ${tflag[*]} -p cs2-vk-capture --lib"

# a fresh directory: a stale payload file from an earlier run must not ship
rm -rf "$out"
mkdir -p "$out/payload/bridge-rs" "$out/payload/metamod/$mplat" "$out/payload/gbe" "$out/payload/vklayer/$vplat"
for b in cs2mcp-native client-mcp hammer-mcp; do cp "$rel/$b$ext" "$out/"; done
cp -r "$repo/sigs/signatures" "$out/sigs"
cp -r "$bridge/$bplat" "$out/payload/bridge-rs/$bplat"
cp -r "$metamod/$mplat/addons" "$out/payload/metamod/$mplat/addons"
cp -f "$metamod"/BUILDINFO-"$mplat".txt "$out/payload/metamod/" 2>/dev/null || true
cp -r "$gbe/$gplat" "$out/payload/gbe/$gplat"
cp -f "$gbe/GBE_REF.txt" "$out/payload/gbe/" 2>/dev/null || true
cp "$rel/$vlib" "$out/payload/vklayer/$vplat/"
cp "$repo/crates/cs2-vk-capture/$vmanifest" "$out/payload/vklayer/$vplat/VkLayer_cs2mcp_capture.json"
(cd "$out" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
echo "package.sh: $os package in $out ($(wc -l < "$out/SHA256SUMS") files, $(du -sh --apparent-size "$out" | cut -f1))"
