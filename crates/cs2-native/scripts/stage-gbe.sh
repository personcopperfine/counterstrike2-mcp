#!/bin/bash
# stage-gbe.sh [out-dir]: collect the gbe_fork (Goldberg) Steam emulator binaries the launcher's
# `--steam gbe` mode installs, into <out-dir> (default <repo>/target/gbe):
#   win64/steam_api64.dll                      from gbe_fork's own release for GBE_REF (pinned below:
#                                              tag, commit and the archive's SHA-256; downloaded once
#                                              into <repo>/target/gbe-release/<tag>/, checked, then
#                                              release/regular/x64/ extracted with 7z).
#                                              GBE_WIN_DLL=<path>: copy that file instead (offline)
#   linux64/libsteam_api.so, linux64/steamclient.so
#                                              GBE_LINUX_FROM picks the source (default: image when
#                                              podman has the image, else release):
#                                              release: gbe_fork's release for GBE_REF, pinned and
#                                              cached like the DLL; release/regular/x64/ extracted
#                                              with tar (no podman needed)
#                                              image: the client image's /app/ (GBE_IMAGE, default
#                                              $CS2_MCP_IMAGE or ghcr.io/personcopperfine/cs2-docker:latest;
#                                              the image's gbe build is its own, see its
#                                              /app/BUILDINFO.gbe)
#                                              source (or GBE_FROM_SOURCE=1): built here, from a
#                                              client Dockerfile with a `builder` stage at its
#                                              GBE_REF (GBE_DOCKERFILE; or GBE_REF=<commit>), so
#                                              the libs provably match the ref. The published
#                                              cs2-docker Dockerfile has no such stage (it pulls
#                                              prebuilt gbe), so source needs GBE_DOCKERFILE.
#                                              The release and image libs are not byte-identical
#                                              (different compilers; workflows/native-launcher-report.md).
# Each copy is refused unless it is an emulator (gbe names its own settings file, configs.main.ini), so
# Valve's own library can never be staged by mistake. Prints sha256 of what it staged.
#
# The Windows DLL is not built here (FIXME 14): gbe_fork builds Windows only with MSVC (premake
# vs20xx projects), which does not cross-build from Linux. Its release CI builds it at the tagged
# commit; the user chose that release over a Visual Studio build (2026-11-05). The vs22 build of
# release-2026_09_16_2 is byte-identical to the steam_api64.dll the Wine tools install carries.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
# shellcheck source=../../../bridge/lib-root.sh
. "$repo/bridge/lib-root.sh"
project_root="$(cs2mcp_project_root "$repo")"
out="${1:-$repo/target/gbe}"
img="${GBE_IMAGE:-${CS2_MCP_IMAGE:-ghcr.io/personcopperfine/cs2-docker:latest}}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR_PODMAN:-$project_root/.podman/xdg}"

is_gbe() { grep -q -a configs.main.ini "$1"; }   # gbe names its own settings file; Valve's library does not

# gbe_fork's release for GBE_REF: bump the tag and commit together with the Dockerfile's GBE_REF,
# and each asset's SHA-256 with them (gh api repos/Detanup01/gbe_fork/commits/<tag> --jq .sha; the
# asset's "digest"). src/gbe.rs compiles the same pin in (`cs2mcp-native fetch-gbe`); its tests
# hold the two equal.
rel_tag=release-2026_09_16_2
rel_commit=8479ddbc096f7d8a418ec28f463c829b77f66911
win_asset=emu-win-release-vs22.7z
win_sha256=d311deadc2a8a8aed620fe66976646059388123587aa22d408f723c592fc9688
linux_asset=emu-linux-release.tar.bz2
linux_sha256=59b284b3a22513dabde29f2fe4e256e0e76ac154738d13bf46da064c78b9a12e

# GBE_DOCKERFILE: a client Dockerfile (e.g. <clone of personcopperfine/cs2-docker>/client/Dockerfile)
# whose GBE_REF is read and, in source mode, whose builder stage is built. Unset: no Dockerfile
# is read and the ref is GBE_REF, else the pin above.
dockerfile="${GBE_DOCKERFILE:-}"
if [ -n "$dockerfile" ] && [ ! -f "$dockerfile" ]; then
    echo "stage-gbe: GBE_DOCKERFILE $dockerfile does not exist" >&2
    exit 2
fi
if [ -n "${GBE_REF:-}" ]; then
    ref="$GBE_REF"
elif [ -n "$dockerfile" ] && grep -q 'GBE_REF=[0-9a-f]' "$dockerfile"; then
    ref="$(grep -m1 -o 'GBE_REF=[0-9a-f]*' "$dockerfile" | cut -d= -f2)"
else
    ref="$rel_commit"   # no Dockerfile naming a GBE_REF: the pin is the only ref there is
fi
cache="$repo/target/gbe-release/$rel_tag"

# release_asset <asset> <sha256>: the cached, hash-checked download of one release asset
release_asset() {
    [ "$ref" = "$rel_commit" ] || {
        echo "stage-gbe: GBE_REF $ref is not $rel_tag ($rel_commit); update the release pin in $0 (or set GBE_WIN_DLL / GBE_LINUX_FROM=image)" >&2
        exit 1
    }
    mkdir -p "$cache/dl"
    if ! echo "$2  $cache/dl/$1" | sha256sum -c --status 2>/dev/null; then
        curl -fL --retry 3 -o "$cache/dl/$1.part" \
            "https://github.com/Detanup01/gbe_fork/releases/download/$rel_tag/$1"
        echo "$2  $cache/dl/$1.part" | sha256sum -c --status || {
            echo "stage-gbe: $1 does not match its pinned SHA-256" >&2
            exit 1
        }
        mv -f "$cache/dl/$1.part" "$cache/dl/$1"
    fi
}

mkdir -p "$out/win64" "$out/linux64"
if [ -n "${GBE_WIN_DLL:-}" ]; then
    win_src="$GBE_WIN_DLL" win_from="$GBE_WIN_DLL"
else
    release_asset "$win_asset" "$win_sha256"
    rm -rf "$cache/extract"
    7z x -y -o"$cache/extract" "$cache/dl/$win_asset" 'release/regular/x64/steam_api64.dll' >/dev/null
    win_src="$cache/extract/release/regular/x64/steam_api64.dll"
    win_from="gbe_fork $rel_tag $win_asset (sha256 $win_sha256)"
fi
if [ -f "$win_src" ] && is_gbe "$win_src"; then
    cp -f "$win_src" "$out/win64/steam_api64.dll"
else
    echo "stage-gbe: $win_src is missing or not the emulator" >&2
    exit 1
fi
linux_from="${GBE_LINUX_FROM:-}"
[ "${GBE_FROM_SOURCE:-0}" = 1 ] && linux_from="${linux_from:-source}"
if [ -z "$linux_from" ]; then
    if command -v podman >/dev/null 2>&1 && podman image exists "$img" 2>/dev/null; then
        linux_from=image
    else
        linux_from=release
        echo "stage-gbe: no podman image $img; the Linux libs come from gbe_fork's release" >&2
    fi
fi
case "$linux_from" in
    release)
        release_asset "$linux_asset" "$linux_sha256"
        rm -rf "$cache/extract-linux"
        mkdir -p "$cache/extract-linux"
        tar -xjf "$cache/dl/$linux_asset" -C "$cache/extract-linux" \
            release/regular/x64/libsteam_api.so release/regular/x64/steamclient.so
        for f in libsteam_api.so steamclient.so; do
            cp -f "$cache/extract-linux/release/regular/x64/$f" "$out/linux64/$f"
        done
        linux_desc="gbe_fork $rel_tag $linux_asset (sha256 $linux_sha256)"
        ;;
    image|source)
        if [ "$linux_from" = source ]; then
            [ -n "$dockerfile" ] || {
                echo "stage-gbe: GBE_LINUX_FROM=source needs GBE_DOCKERFILE (a client Dockerfile with a builder stage)" >&2
                exit 2
            }
            # the image's own builder stage (steamrt4 SDK, premake, make), nothing else; its
            # context is unused by that stage, so the Dockerfile's directory is passed as it is
            builder="localhost/cs2mcp-gbe-builder:$ref"
            podman build --target builder --build-arg GBE_REF="$ref" -t "$builder" -f "$dockerfile" "$(dirname "$dockerfile")"
            src=/artifacts/linux/gmake/release/regular/x64
            img="$builder"
            linux_desc="built from source at $ref"
        else
            src=/app
            # an image built from an older Dockerfile would not say so: use source to be sure
            linux_desc="image $img $src"
        fi
        cid="$(podman create "$img" true)"
        trap 'podman rm -f "$cid" >/dev/null 2>&1 || true' EXIT
        for f in libsteam_api.so steamclient.so; do
            podman cp "$cid:$src/$f" "$out/linux64/$f"
        done
        ;;
    *) echo "stage-gbe: GBE_LINUX_FROM=release|image|source, not $linux_from" >&2; exit 2 ;;
esac
for f in libsteam_api.so steamclient.so; do
    is_gbe "$out/linux64/$f" || { echo "stage-gbe: linux64/$f ($linux_desc) is not the emulator" >&2; exit 1; }
done

# GBE_REF.txt: GBE_REF, else the ref GBE_DOCKERFILE names, else the pin; and where each half came from
{
    echo "GBE_REF=$ref"
    echo "win64: $win_from"
    echo "linux64: $linux_desc"
} > "$out/GBE_REF.txt"
sha256sum "$out"/win64/* "$out"/linux64/*
echo "stage-gbe: staged into $out ($(tr '\n' ';' < "$out/GBE_REF.txt"))"
