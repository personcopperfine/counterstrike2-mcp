#!/bin/bash
# =============================================================================
# Build the cs2-vk-capture Vulkan layer and install it where client-mcp mounts it from.
#
#   client-mcp/docker/build-vk-layer.sh [<layer-dir>]
#
# Builds `cargo build --release -p cs2-vk-capture` in this workspace and copies
#   libcs2_vk_capture.so            (target/release, honours CARGO_TARGET_DIR)
#   VkLayer_cs2mcp_capture.json     (crates/cs2-vk-capture/, the explicit manifest)
# into <layer-dir>. The default is <cs2-mcp>/target/vklayer, which is also the default of
# CS2_MCP_VK_LAYER_DIR / --vk-layer-dir. Give another directory to keep several builds apart and
# point the server at it with CS2_MCP_VK_LAYER_DIR=<dir>.
#
# session_status reports runtime.vkLayer.valid: true when both files are present. Sessions launched
# while it is valid get the layer mounted read-only at /cs2mcp/vk and the explicit-layer environment
# (VK_ADD_LAYER_PATH, VK_INSTANCE_LAYERS=VK_LAYER_CS2MCP_capture, CS2MCP_CAPTURE_DIR=/work/capture).
# The layer is not built inside the container image; the host builds it once.
# =============================================================================
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"            # cs2-mcp/ (the workspace root)
DEST="${1:-${CS2_MCP_VK_LAYER_DIR:-$REPO/target/vklayer}}"
cd "$REPO"
# The library's path comes from cargo itself (compiler-artifact messages), so a custom
# CARGO_TARGET_DIR or a different profile layout needs no special case here. With pipefail, a failed
# build stops the script before anything is copied.
SO="$(cargo build --release -p cs2-vk-capture --message-format=json \
  | python3 -c '
import json, sys
found = ""
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    t = m.get("target", {})
    if m.get("reason") == "compiler-artifact" and t.get("name") == "cs2_vk_capture" \
            and "cdylib" in t.get("crate_types", []):
        found = next((f for f in m.get("filenames", []) if f.endswith(".so")), found)
print(found)
')"
MANIFEST="$REPO/crates/cs2-vk-capture/VkLayer_cs2mcp_capture.json"
[ -n "$SO" ] && [ -f "$SO" ] || { echo "build-vk-layer: cargo did not report the cdylib (libcs2_vk_capture.so)" >&2; exit 1; }
[ -f "$MANIFEST" ] || { echo "build-vk-layer: $MANIFEST not found" >&2; exit 1; }

mkdir -p "$DEST"
install -m 0644 "$SO" "$DEST/libcs2_vk_capture.so"
install -m 0644 "$MANIFEST" "$DEST/VkLayer_cs2mcp_capture.json"
echo "build-vk-layer: installed libcs2_vk_capture.so and VkLayer_cs2mcp_capture.json in $DEST"
