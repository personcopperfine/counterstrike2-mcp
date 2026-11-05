#!/bin/bash
# =============================================================================
# client-mcp: headless native-Linux CS2 DEDICATED SERVER (LinuxDedicated flavour).
#
# Runs inside ghcr.io/personcopperfine/cs2-docker, `--network=host`, the same cs2-home volume as a LinuxClient.
# docker::run_argv starts it with
#   --entrypoint bash IMAGE /cs2mcp/linux-dedicated.sh -dedicated -insecure -console -usercon \
#       +sv_lan 1 +game_type 0 +game_mode 0 +map <map> [+rcon_password <pw>] \
#       [+sv_hibernate_when_empty 0] [+hostport <port>] [...]
# (the game arguments come from the tool, see server_up in client-mcp/src/tools/multiplayer.rs;
# +sv_hibernate_when_empty 0 is server_up's default — an empty server hibernates otherwise and
# its bridge never ticks — and is passed only when the caller did not opt out via hibernate: true).
#
# Differences from linux-client.sh (the client):
#   * no Xvfb, no shader cache, no video config overrides: a server never draws;
#   * no -condebug. CS2 has no `con_logfile` command. Without -condebug the FULL server log goes to
#     stdout, so this script tees stdout + stderr to /work/server.log. That file is what the host-side
#     ServerLive check reads (readiness markers are plain substrings, no timestamps or channel prefixes).
#   * the shared csgo/console.log is not touched (a client in the same volume owns that file).
#   * Metamod loads gameinfo-free, like the client: libcs2mcp_mmload.so is prepended to LD_PRELOAD
#     on the cs2.sh/`cs2 -dedicated` command only (not exported), and the script refuses to launch
#     when it is missing. nothing edits csgo/gameinfo.gi any more (crates/metamod/README.md,
#     "Launching").
#
# Preamble: sourced from lib-preamble.sh, run under the home-volume flock (see that file for why a
# client and a server can share one volume).
# =============================================================================
set -u

# run-cs2.sh:9-10 (same defaults as linux-client.sh)
G="${CS2_GAME_DIR:-/home/steam/steamapps/common/Counter-Strike Global Offensive/game}"
B="$G/bin/linuxsteamrt64"
GSE_SAVE_DIR="${GSE_SAVE_DIR:-/app}"
OUT="${CS2_OUT_DIR:-/work}"

# shared preamble (functions only)
. "$(dirname "$0")/lib-preamble.sh"

cs2mcp_lock
cs2mcp_preamble_fix
cs2mcp_unlock

# Metamod loader (gameinfo-free; nothing edits csgo/gameinfo.gi any more): prepended to LD_PRELOAD on
# the cs2.sh command only (see linux-client.sh for why it is relative and why a missing loader
# aborts instead of running without Metamod).
MMLOAD="csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so"
if [ ! -f "$G/$MMLOAD" ]; then
    echo "[cs2mcp-mmload] FAIL: $G/$MMLOAD is missing; Metamod would not load - run 'cargo bridge install --platform linux ensure' (or install) and retry" >&2
    exit 1
fi
# no X display for a server; the env block is the same one the client gets
cs2mcp_preamble_env

echo "=== [4] launch cs2.sh -dedicated (full log on stdout -> $OUT/server.log) ==="
cd "$G" || exit 1
mkdir -p "$OUT"

# stdout + stderr -> /work/server.log (tee keeps podman logs too); LD_PRELOAD is set for this
# command only, so the loader reaches the server process and not the rest of the session
LD_PRELOAD="$MMLOAD${LD_PRELOAD:+ $LD_PRELOAD}" \
    "$G/cs2.sh" "$@" > >(tee -a "$OUT/server.log") 2>&1 &
GPID=$!
# PID 1 ignores SIGTERM without a handler; forward it so a graceful podman stop reaches the server
trap 'kill -TERM $GPID 2>/dev/null' TERM INT
# wait returns early when a trapped signal arrives: keep waiting until the server is really gone
RC=0
while kill -0 "$GPID" 2>/dev/null; do
  wait "$GPID"; RC=$?
done
echo "SERVER_EXIT=$RC"

echo "=== [5] teardown ==="
sleep 1   # let tee flush the last lines into server.log
exit "$RC"
