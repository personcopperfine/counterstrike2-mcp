#!/bin/bash
# =============================================================================
# Build the s2sdk-probe LD_PRELOAD payload and install it where client-mcp mounts it from.
#
#   crates/s2sdk/docker/build-probe.sh [<probe-dir>]
#
# Builds `cargo build --release -p s2sdk-probe` in this workspace and copies
#   libs2sdk_probe.so               (target/release, honours CARGO_TARGET_DIR)
# into <probe-dir> (default: $CS2_MCP_S2SDK_PROBE_DIR, else <cs2-mcp>/target/s2sdk-probe). The
# server only preloads the probe when --s2sdk-probe-dir / CS2_MCP_S2SDK_PROBE_DIR is set (no
# default), so point it at the same directory.
#
# The payload is loaded into the game process of the Linux sessions
# (docker.rs: -v <dir>:/cs2mcp/s2sdk:ro, CS2MCP_PRELOAD prepended to LD_PRELOAD by
# lib-preamble.sh) — the container image's glibc, never the host's. Unlike build-vk-layer.sh this
# script can pre-check that: it prints the newest GLIBC symbol version the .so needs and refuses
# when that is newer than the image's (the loader would abort the game with "version GLIBC_x.y
# not found"). S2SDK_PROBE_MAX_GLIBC overrides the image version.
# =============================================================================
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"            # cs2-mcp/ (the workspace root)
DEST="${1:-${CS2_MCP_S2SDK_PROBE_DIR:-$REPO/target/s2sdk-probe}}"
MAX_GLIBC="${S2SDK_PROBE_MAX_GLIBC:-2.41}"   # ghcr.io/personcopperfine/cs2-docker's glibc (checked with a run)
cd "$REPO"
# The library's path comes from cargo itself (compiler-artifact messages), so a custom
# CARGO_TARGET_DIR or a different profile layout needs no special case here. With pipefail, a failed
# build stops the script before anything is copied.
SO="$(cargo build --release -p s2sdk-probe --message-format=json \
  | python3 -c '
import json, sys
found = ""
for line in sys.stdin:
    try:
        m = json.loads(line)
    except ValueError:
        continue
    t = m.get("target", {})
    if m.get("reason") == "compiler-artifact" and t.get("name") == "s2sdk_probe" \
            and "cdylib" in t.get("crate_types", []):
        found = next((f for f in m.get("filenames", []) if f.endswith(".so")), found)
print(found)
')"
[ -n "$SO" ] && [ -f "$SO" ] || { echo "build-probe: cargo did not report the cdylib (libs2sdk_probe.so)" >&2; exit 1; }

mkdir -p "$DEST"
install -m 0644 "$SO" "$DEST/libs2sdk_probe.so"

# The newest GLIBC symbol version the installed .so resolves against. Empty when the payload needs
# no versioned glibc symbols at all (grep finds nothing; the `|| true` keeps that non-fatal under
# pipefail).
NEWEST="$(objdump -T "$DEST/libs2sdk_probe.so" \
    | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 || true)"           # GLIBC_2.34 (newest needed)
if [ -n "$NEWEST" ] \
    && [ "$(printf '%s\n' "$MAX_GLIBC" "${NEWEST#GLIBC_}" | sort -V | tail -1)" != "$MAX_GLIBC" ]; then
  echo "build-probe: $DEST/libs2sdk_probe.so needs $NEWEST, newer than the image's GLIBC_$MAX_GLIBC (S2SDK_PROBE_MAX_GLIBC): the game would fail to preload it" >&2
  exit 1
fi
echo "build-probe: installed libs2sdk_probe.so in $DEST (glibc needed: ${NEWEST:-none}, image tolerates GLIBC_$MAX_GLIBC)"
