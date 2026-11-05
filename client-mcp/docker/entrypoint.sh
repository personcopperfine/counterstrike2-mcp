#!/bin/bash
# STUB entrypoint for the cs2-mcp layer. Dispatches on $1:
#   (both wine-* lines go through cs2mcp_launch.exe, which starts cs2.exe and injects the
#    Metamod loader DLL - crates/mm-launch, bridge/metamod/README.md "Launching")
#   wine-tools   -> cd /app && ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug "$@" +engine_no_focus_sleep 0 +r_always_render_all_windows 1
#   wine-client  -> cd /app && ./entrypoint.sh cs2mcp_launch.exe -vulkan -console -condebug "$@"
#   linux-client -> /app/entrypoint.sh "$@"        (parent client image entrypoint: cs2.sh -steam ...)
#   headless     -> start the in-container display (see workflows) and exec one of the above
set -euo pipefail
echo "[cs2-mcp] stub entrypoint: $*" >&2
exit 64
