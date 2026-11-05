#!/usr/bin/env bash
# Live check of VK_LAYER_CS2MCP_capture against a real Vulkan loader and ICD.
#
# App modes (CS2VK_APP):
#   harness (default) vk_present_harness: plays the server itself, verifies every capture pixel
#                     for pixel against its frame pattern, and exits 0 only if all pass.
#   vkcube            vkcube --c <frames> on a private Xvfb :93 (needs Xvfb and vkcube). The
#                     script checks the result files only; vkcube draws no known pattern.
#
# Env:
#   CS2VK_LAYER_SO    libcs2_vk_capture.so (required)
#   CS2VK_HARNESS     vk_present_harness binary (required for harness mode)
#   CS2VK_ICD         ICD manifest, e.g. /usr/share/vulkan/icd.d/lvp_icd.json (required)
#   CS2VK_WORK        scratch dir (default: $CARGO_TARGET_DIR or <repo>/target/vk, then live-work/<pid>)
#   CS2VK_VALIDATION_DIR  dir holding VkLayer_khronos_validation.json and its .so; when set, the
#                     validation layer (incl. synchronization validation) runs below the capture
#                     layer and any VUID/SYNC message fails the run
#   CS2VK_FRAMES      frames for vkcube mode (default 300)
#   CS2VK_ID          request id for vkcube mode (default live1)
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
: "${CS2VK_LAYER_SO:?set CS2VK_LAYER_SO to libcs2_vk_capture.so}"
: "${CS2VK_ICD:?set CS2VK_ICD to an ICD manifest}"
app_mode=${CS2VK_APP:-harness}
frames=${CS2VK_FRAMES:-300}
id=${CS2VK_ID:-live1}

# scratch lives under the repo's target dir, never under /tmp
target=${CARGO_TARGET_DIR:-$here/../../../target/vk}
work=${CS2VK_WORK:-$target/live-work/$$}
cap=$work/capture
mkdir -p "$cap"
chmod 700 "$cap"

# explicit layer: the manifest's library_path is relative to the manifest's own directory
layers=$work/layers
mkdir -p "$layers"
cp "$here/../VkLayer_cs2mcp_capture.json" "$layers/"
ln -sf "$CS2VK_LAYER_SO" "$layers/libcs2_vk_capture.so"
layer_path=$layers
instance_layers=VK_LAYER_CS2MCP_capture

if [ -n "${CS2VK_VALIDATION_DIR:-}" ]; then
    layer_path="$layers:$CS2VK_VALIDATION_DIR"
    instance_layers="VK_LAYER_CS2MCP_capture:VK_LAYER_KHRONOS_validation"
fi

echo "work dir: $work"
echo "capture dir: $cap"
echo "app mode: $app_mode"

xpid=""
cleanup() {
    if [ -n "$xpid" ]; then kill "$xpid" 2>/dev/null || true; fi
}
trap cleanup EXIT

case "$app_mode" in
harness)
    : "${CS2VK_HARNESS:?set CS2VK_HARNESS to the vk_present_harness binary}"
    app=("$CS2VK_HARNESS")
    ;;
vkcube)
    if ! command -v Xvfb >/dev/null 2>&1 || ! command -v vkcube >/dev/null 2>&1; then
        echo "SKIPPED: vkcube mode needs Xvfb and vkcube"
        exit 3
    fi
    Xvfb :93 -screen 0 640x480x24 >/dev/null 2>&1 &
    xpid=$!
    export DISPLAY=:93
    sleep 1
    app=(vkcube --c "$frames" --width 640 --height 480)
    # the server side asks for one frame before the app starts
    printf '{"id":"%s","format":"png"}' "$id" > "$cap/.request.tmp"
    mv "$cap/.request.tmp" "$cap/request.json"
    ;;
*)
    echo "FAIL: unknown CS2VK_APP=$app_mode"
    exit 1
    ;;
esac

export VK_DRIVER_FILES="$CS2VK_ICD"
export VK_ADD_LAYER_PATH="$layer_path"
export VK_INSTANCE_LAYERS="$instance_layers"
export CS2MCP_CAPTURE_DIR="$cap"
export CS2MCP_CAPTURE_DEBUG=1
if [ -n "${CS2VK_VALIDATION_DIR:-}" ]; then
    # synchronization validation as well as core validation
    export VK_LAYER_VALIDATE_SYNC=1
    # the loader's layer trace proves the validation layer was inserted (a missing layer would
    # otherwise look like "no errors")
    export VK_LOADER_DEBUG=layer
fi

log=$work/app.log
set +e
"${app[@]}" 2>"$log" | tee "$work/app.stdout"
app_rc=${PIPESTATUS[0]}
set -e
cat "$log" >&2
echo "app exit: $app_rc"
if [ "$app_rc" -eq 3 ]; then
    echo "SKIPPED"
    exit 3
fi
if [ "$app_rc" -ne 0 ]; then
    echo "FAIL: app exited $app_rc"
    exit 1
fi

if [ -n "${CS2VK_VALIDATION_DIR:-}" ]; then
    if grep -E "VUID|SYNC-|Validation Error" "$log" "$work/app.stdout" >/dev/null 2>&1; then
        echo "FAIL: validation reported errors:"
        grep -E "VUID|SYNC-|Validation Error" "$log" "$work/app.stdout" | head -20
        exit 1
    fi
    if ! grep -q 'Insert instance layer "VK_LAYER_KHRONOS_validation"' "$log"; then
        echo "FAIL: the validation layer was not loaded (see $log)"
        grep -m3 "failed to load\|ERROR" "$log" || true
        exit 1
    fi
    echo "validation: loaded, no VUID or SYNC errors"
fi

if [ "$app_mode" = vkcube ]; then
    if [ ! -f "$cap/$id.json" ] || [ ! -f "$cap/$id.png" ]; then
        echo "FAIL: no result for $id in $cap"
        ls -la "$cap"
        exit 1
    fi
    if grep -q '"error"' "$cap/$id.json"; then
        echo "FAIL: layer reported an error: $(cat "$cap/$id.json")"
        exit 1
    fi
    echo "OK $cap/$id.png"
fi
echo "DONE $work"
