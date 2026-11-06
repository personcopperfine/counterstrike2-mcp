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
#     launch when that loader is missing, and nothing edits csgo/gameinfo.gi any more (crates/metamod/
#     README.md, "Launching");
#   * console.log is mirrored to /work (the host artifacts dir) for the host-side readiness check;
#   * the game's output stays on stdout, so `podman logs` shows it.
#
# Launcher mode (CS2MCP_NATIVE set: docker.rs mounts a cs2mcp package read-only at /cs2mcp/native
# when client-mcp runs with --native-package): `cs2mcp-native run` prepares the volume's game tree
# itself (bridge, Metamod loader, gbe in place, each change in a manifest under
# /home/steam/.cs2mcp-native on the same volume), runs cs2.sh in its own process group, and on
# exit or on SIGTERM (podman stop -t 60) stops the game, restores and verifies the tree, then exits
# with the game's code (3: the tree was not restored; the next run restores it first). Then:
#   * the preamble's gbe steps ([0c] settings + steam_settings link, [0d] setup_gbe.sh) are
#     skipped (CS2MCP_NATIVE_GBE=1);
#   * `cargo bridge install` is not run on the host for this kind, and --cleanup-legacy moves an
#     older `cargo bridge install` (install-rs.sh) / setup_gbe.sh layout out of the volume first
#     (into the state dir; Valve's libsteam_api.so comes back from setup_gbe.sh's own copy in
#     ~/.steam);
#     CS2MCP_NATIVE_CLEANUP_LEGACY=0 leaves it (with CS2MCP_NATIVE_ACCEPT_CONFLICTS=1 the session
#     then runs over it and puts it back as found: the native-test image's acceptance run);
#   * CS2MCP_NATIVE_ACCEPT_CONFLICTS=1 passes --accept-conflicts (leftovers it cannot place are
#     otherwise a stop, exit 2, with the findings as JSON on stdout).
# The rest (Xvfb, preamble env, the console.log mirror, teardown) is the same.
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

NATIVE="${CS2MCP_NATIVE:-}"
if [ -n "$NATIVE" ]; then
    [ -x "$NATIVE/cs2mcp-native" ] || { echo "[cs2mcp-native] FAIL: $NATIVE/cs2mcp-native is missing (CS2MCP_NATIVE)" >&2; exit 1; }
    export CS2MCP_NATIVE_GBE=1
fi

cs2mcp_lock
cs2mcp_preamble_fix
cs2mcp_unlock

# Metamod loads gameinfo-free (nothing edits csgo/gameinfo.gi any more): the loader .so is prepended
# to LD_PRELOAD on the cs2.sh command below. It redirects the engine's first dlopen of
# csgo/bin/linuxsteamrt64/libserver.so to csgo/addons/metamod/... (crates/metamod/README.md "Launching"). Without it the
# engine loads its own server image and Metamod never loads, so refuse to start instead of
# running a bridge-less session.
MMLOAD="csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so"
# launcher mode: cs2mcp-native run installs the loader itself (and prepends it to LD_PRELOAD)
if [ -z "$NATIVE" ] && [ ! -f "$G/$MMLOAD" ]; then
    echo "[cs2mcp-mmload] FAIL: $G/$MMLOAD is missing; Metamod would not load - run 'cargo bridge install --platform linux ensure' (or install) and retry" >&2
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
# earlier run in the same home volume cannot satisfy readiness. Launcher mode keeps it: the log is
# a runtime file of cs2mcp-native's manifest (the user's copy is backed up and put back), so the
# mirror waits for a log written after this point instead.
cd "$G" || exit 1
mkdir -p "$OUT"
STARTED="$OUT/.linux-client-started"
: > "$STARTED"
[ -z "$NATIVE" ] && rm -f "$G/csgo/console.log"
# MCP addition: mirror console.log into /work once the engine writes it (client/entrypoint.sh:247-250
# does the same with tail -F), so its existence means the engine started.
( until [ -f "$G/csgo/console.log" ] && [ "$G/csgo/console.log" -nt "$STARTED" ]; do sleep 1; done
  exec tail -n +1 -F "$G/csgo/console.log" >"$OUT/console.log" ) &
TAILPID=$!
# run-cs2.sh:148 flags, then -insecure: Metamod:Source refuses to load without `-dedicated` or
# `-insecure` ("MMS: Fatal error: Metamod:Source requires -dedicated or -insecure"), and every
# session now loads Metamod with the cs2-mcp bridge as a plugin. -insecure means VAC off and no
# official/secure servers (the main menu says "insecure"); local and LAN servers are unaffected.
# No -steam: the gbe emulator handles that. Testscript runs are refused on Linux before this point.
# LD_PRELOAD is set for THIS command only (the preamble's audio libs + optional s2sdk probe stay
# behind it), so the loader reaches the game process and not the rest of the session.
if [ -n "$NATIVE" ]; then
    # cs2mcp-native adds -console -condebug -insecure (always), the focus flags and the loader;
    # the harness flags and the caller's args follow `--`. Its JSON result goes to /work.
    accept=()
    [ "${CS2MCP_NATIVE_ACCEPT_CONFLICTS:-0}" = 1 ] && accept=(--accept-conflicts)
    cleanup=(--cleanup-legacy)
    [ "${CS2MCP_NATIVE_CLEANUP_LEGACY:-1}" = 0 ] && cleanup=()
    CS2_BRIDGE_LOG="$OUT/mcp_bridge_rs.log" \
        "$NATIVE/cs2mcp-native" --game-root "$(dirname "$G")" --state-dir /home/steam/.cs2mcp-native \
        --gbe-save-dir "$GSE_SAVE_DIR" "${accept[@]}" \
        run --steam gbe --no-capture "${cleanup[@]}" -- -novid -nosound -conclearlog "$@" \
        > "$OUT/cs2mcp-native-run.json" &
    GPID=$!
else
LD_PRELOAD="$MMLOAD${LD_PRELOAD:+ $LD_PRELOAD}" \
    "$G/cs2.sh" -novid -nosound -condebug -conclearlog -insecure "$@" &                       # run-cs2.sh:174
GPID=$!
fi
# PID 1 ignores SIGTERM without a handler; forward it so a graceful podman stop reaches the game
# (launcher mode: cs2mcp-native, which stops the game and restores the volume before it exits)
trap 'kill -TERM $GPID 2>/dev/null' TERM INT
# a trapped signal ends `wait` early: wait again until the child is really gone
while :; do
    wait $GPID
    GAME_RC=$?
    kill -0 "$GPID" 2>/dev/null || break
done
echo "GAME_EXIT=$GAME_RC"                                                                   # run-cs2.sh:210

echo "=== [5] teardown + artifacts === (run-cs2.sh:211-226)"
kill "$TAILPID" 2>/dev/null
kill -9 "$XPID" 2>/dev/null                                                                 # run-cs2.sh:211
cp "$G/csgo/console.log" "$OUT/cs2console.log" 2>/dev/null                                  # run-cs2.sh:217
cp /tmp/xvfb.log "$OUT/xvfb.log" 2>/dev/null
exit "$GAME_RC"
