#!/bin/bash
# build.sh [--pkg DIR] [--base IMAGE] [--tag TAG]: build the native-test image (Containerfile
# next to this file) from a Linux launcher package. The build context is staged under
# target/native-test/ctx (never /tmp): the package, linux-client.sh, lib-preamble.sh.
#   --pkg   the package dir (default target/cs2mcp-native-linux: package.sh --os linux)
#   --base  the client image (default $CS2_MCP_IMAGE or ghcr.io/personcopperfine/cs2-docker:latest)
#   --tag   default localhost/cs2mcp-native-test:latest
# podman runs with XDG_RUNTIME_DIR=<project root>/.podman/xdg (crates/cs2-native/scripts/lib-root.sh).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
pkg="$repo/target/cs2mcp-native-linux"
base="${CS2_MCP_IMAGE:-ghcr.io/personcopperfine/cs2-docker:latest}"
tag="localhost/cs2mcp-native-test:latest"
while [ $# -gt 0 ]; do
    case "$1" in
        --pkg) pkg="$2"; shift 2 ;;
        --base) base="$2"; shift 2 ;;
        --tag) tag="$2"; shift 2 ;;
        *) echo "build.sh: unknown argument $1" >&2; exit 2 ;;
    esac
done
[ -x "$pkg/cs2mcp-native" ] && [ -f "$pkg/SHA256SUMS" ] \
    || { echo "build.sh: $pkg is not a package (crates/cs2-native/scripts/package.sh --os linux)" >&2; exit 1; }
grep -q '^target=x86_64-unknown-linux-gnu' "$pkg/BUILDINFO.txt" \
    || { echo "build.sh: $pkg is not a Linux package" >&2; exit 1; }
(cd "$pkg" && sha256sum --quiet -c SHA256SUMS) || { echo "build.sh: $pkg fails its SHA256SUMS" >&2; exit 1; }
# shellcheck source-path=SCRIPTDIR source=../../../crates/cs2-native/scripts/lib-root.sh
. "$repo/crates/cs2-native/scripts/lib-root.sh"
root="$(cs2mcp_project_root "$repo")"
export XDG_RUNTIME_DIR="$root/.podman/xdg"

ctx="$repo/target/native-test/ctx"
rm -rf "$ctx"
mkdir -p "$ctx"
cp -r "$pkg" "$ctx/native"
cp "$repo/client-mcp/docker/linux-client.sh" "$repo/client-mcp/docker/lib-preamble.sh" "$here/Containerfile" "$ctx/"
podman build --network=host --build-arg "BASE=$base" -t "$tag" -f "$ctx/Containerfile" "$ctx"
echo "build.sh: $tag from $base with $(head -n1 "$pkg/BUILDINFO.txt")"
