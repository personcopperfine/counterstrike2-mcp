#!/bin/bash
# =============================================================================
# client-mcp: headless native-Linux CS2 session (LinuxClient flavour).
#
# Runs inside ghcr.io/personcopperfine/cs2-docker. docker::run_argv starts the container with
#   --entrypoint bash IMAGE /cs2mcp/linux-client.sh [game args...]
# and mounts this file read-only at /cs2mcp/linux-client.sh.
#
# Container runs as root (-u 0): the preamble edits /etc/os-release (run-cs2.sh:115) and the
# game's bin dir (run-cs2.sh:16-25), like the proven harness.
#
# Port of harness/run-cs2.sh (the proven headless boot). Deliberate differences:
#   * no RUNSECS / SNAP_EVERY snapshot loop: the game runs until the container is removed;
#   * no gdb (CS2_GDB), no X11/FMT shims, no EXTERNAL X (harness/x11shim.c, fmtshim.c are not
#     needed by the MCP session; add them here if a workflow needs them);
#   * the game is started with run-cs2.sh's flags, then the caller's extra args;
#   * Metamod loads gameinfo-free: libcs2mcp_mmload.so is prepended to LD_PRELOAD on the cs2.sh
#     command (not exported: the preamble's audio libs / s2sdk probe keep their place and only the
#     game process carries the loader). It redirects the engine's first dlopen of
#     csgo/bin/linuxsteamrt64/libserver.so to csgo/addons/metamod/... . The script refuses to
#     launch when that loader is missing, and nothing edits csgo/gameinfo.gi any more (bridge/metamod/
#     README.md, "Launching");
#   * console.log is mirrored to /work (the host artifacts dir) for the host-side readiness check;
#   * the game's output stays on stdout, so `podman logs` shows it.
# =============================================================================
set -u

# run-cs2.sh:9-10
# CS2_GAME_DIR is set by docker::run_argv from DockerOptions::linux_game_dir (default: this path)
G="${CS2_GAME_DIR:-/home/steam/steamapps/common/Counter-Strike Global Offensive/game}"
B="$G/bin/linuxsteamrt64"
# run-cs2.sh:32 (exported so setup_gbe.sh and the seeding below agree)
GSE_SAVE_DIR="${GSE_SAVE_DIR:-/app}"
OUT="${CS2_OUT_DIR:-/work}"

# shared preamble (functions only; see lib-preamble.sh for the lock and the order)
. "$(dirname "$0")/lib-preamble.sh"

cs2mcp_lock
cs2mcp_preamble_fix
cs2mcp_unlock

# Metamod loads gameinfo-free (nothing edits csgo/gameinfo.gi any more): the loader .so is prepended
# to LD_PRELOAD on the cs2.sh command below. It redirects the engine's first dlopen of
# csgo/bin/linuxsteamrt64/libserver.so to csgo/addons/metamod/... (bridge/metamod/README.md "Launching"). Without it the
# engine loads its own server image and Metamod never loads, so refuse to start instead of
# running a bridge-less session.
MMLOAD="csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so"
if [ ! -f "$G/$MMLOAD" ]; then
    echo "[cs2mcp-mmload] FAIL: $G/$MMLOAD is missing; Metamod would not load - run 'bridge/install-rs.sh --platform linux ensure' (or install) and retry" >&2
    exit 1
fi

echo "=== [2] X display: in-container Xvfb :99 === (run-cs2.sh:67-80)"
# run-cs2.sh:74-77 (XVFB_EXTERNAL branch not used here)
rm -rf /tmp/fbdir && mkdir -p /tmp/fbdir
Xvfb :99 -screen 0 "${XVFB_GEOM:-1280x720x24}" -fbdir /tmp/fbdir -nolisten tcp >/tmp/xvfb.log 2>&1 &
XPID=$!
export DISPLAY=:99
sleep 2                                                                                     # run-cs2.sh:79
xdpyinfo >/dev/null 2>&1 && echo XSERVER-OK || echo XSERVER-FAIL                            # run-cs2.sh:80

cs2mcp_preamble_env

echo "=== [4] launch cs2.sh === (run-cs2.sh:147-175; client/entrypoint.sh:205-250)"
# client/entrypoint.sh:246: remove the game's console.log so a stale 'SwitchToLoop game' from an
# earlier run in the same home volume cannot satisfy readiness
cd "$G" || exit 1
rm -f "$G/csgo/console.log"
# MCP addition: mirror console.log into /work once the engine creates it (client/entrypoint.sh:247-250
# does the same with tail -F). The mirror starts only after the rm above, so its existence means
# the engine started.
mkdir -p "$OUT"
( until [ -f "$G/csgo/console.log" ]; do sleep 1; done
  exec tail -n +1 -F "$G/csgo/console.log" >"$OUT/console.log" ) &
TAILPID=$!
# run-cs2.sh:148 flags, then -insecure: Metamod:Source refuses to load without `-dedicated` or
# `-insecure` ("MMS: Fatal error: Metamod:Source requires -dedicated or -insecure"), and every
# session now loads Metamod with the cs2-mcp bridge as a plugin. -insecure means VAC off and no
# official/secure servers (the main menu says "insecure"); local and LAN servers are unaffected.
# No -steam: the gbe emulator handles that. Testscript runs are refused on Linux before this point.
# LD_PRELOAD is set for THIS command only (the preamble's audio libs + optional s2sdk probe stay
# behind it), so the loader reaches the game process and not the rest of the session.
LD_PRELOAD="$MMLOAD${LD_PRELOAD:+ $LD_PRELOAD}" \
    "$G/cs2.sh" -novid -nosound -condebug -conclearlog -insecure "$@" &                       # run-cs2.sh:174
GPID=$!
# PID 1 ignores SIGTERM without a handler; forward it so a graceful podman stop reaches the game
trap 'kill -TERM $GPID 2>/dev/null' TERM INT
wait $GPID
GAME_RC=$?
echo "GAME_EXIT=$GAME_RC"                                                                   # run-cs2.sh:210

echo "=== [5] teardown + artifacts === (run-cs2.sh:211-226)"
kill "$TAILPID" 2>/dev/null
kill -9 "$XPID" 2>/dev/null                                                                 # run-cs2.sh:211
cp "$G/csgo/console.log" "$OUT/cs2console.log" 2>/dev/null                                  # run-cs2.sh:217
cp /tmp/xvfb.log "$OUT/xvfb.log" 2>/dev/null
exit "$GAME_RC"
