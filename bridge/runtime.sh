#!/bin/bash
# runtime.sh - bridge reload/inspection cycle for the Wine tools runtime.
#
# The bridge is the Rust crate `crates/cs2-bridge` (`cs2_bridge.dll`): build it with
# `bridge/build-rs.sh`, install it into the shared tools install with `bridge/install-rs.sh
# ensure` (or `install`; `up` calls ensure itself: read-only when current, install when stale)
# (the sdkenginetools.txt manifest entry is read at toolframework2 Init -> a fresh install needs
# a full runtime restart, i.e. `up`).
#
# Usage: bridge/runtime.sh <command> [args]
#   down               stop the container
#   up                 fresh session: restart runtime, load the bridge, wait for discovery
#   open [asset]       vtmsend open_asset (asset name; default $POC_ASSET)
#   test               run test_client.py against the live bridge
#   cycle [asset]      up -> open -> test   (build + install first: bridge/build-rs.sh,
#                      bridge/install-rs.sh install)
#   status             container / discovery / liveness summary
#   logs [n]           tail the bridge log and the game console (default 40 lines)
#
# Everything is env-overridable:
#   CS2_MCP_PROJECT_ROOT: the project root holding workshop/run/app and .podman/xdg (default:
#   the main checkout of this repo, bridge/lib-root.sh; CS2DOCKER_ROOT is the deprecated alias);
#   CONTAINER;
#   IMG (default ghcr.io/personcopperfine/cs2-docker:latest, or CS2_MCP_IMAGE when set: the
#   client image the Wine tools run in; a custom build from a clone of
#   https://github.com/personcopperfine/cs2-docker, `podman build -t <tag> <clone>/client`, goes here),
#   ADDON (default mcp_test; empty string disables -addon), POC_ASSET,
#   WAIT_CHANNEL_S, WAIT_DISCOVERY_S, WAIT_DOC_S,
#   TOOL (hammer | modeldoc, default hammer; labels the container cs2mcp.tool=<TOOL>).
#   EXTRA_ARGS: extra cs2.exe arguments (they go to the launcher, which passes them verbatim to
#               cs2.exe). -insecure is added by DEFAULT now (Metamod:Source refuses to load
#               without it; already-present occurrences are not duplicated).
#   CS2MCP_TESTPLUGIN_MODE, CS2MCP_TESTPLUGIN_LOG: passed into the container (and so to the
#               game) when set - the coexistence fixture's switches (crates/mm-testplugin,
#               bridge/test_metamod_coexist.py). Unset: not passed.
#   BRIDGE=rust-only: accepted as a no-op alias (older scripts use it). The only bridge is the
#               Rust tool `cs2_bridge` (a vtm tool_request; its Init line lands in
#               $APP/mcp_bridge_rs.log - `up` runs install-rs.sh ensure first, so it is).
#               BRIDGE=rust: the same. Other values are refused.
#   METAMOD=1: a no-op alias (older scripts) - Metamod is installed unconditionally now
#               (bridge/install-rs.sh installs it every time) and -insecure is always added.
#
# One ModelDoc instance at a time (rule, AGENTS.md): `TOOL=modeldoc up` refuses while any
# container labelled cs2mcp.tool=modeldoc is running, under a flock so two `up`s cannot both
# pass the check. No `up` replaces a running ModelDoc container (ours or another agent's);
# stop it on purpose with `CONTAINER=<name> runtime.sh down`. `open <x>.vmdl` only runs in a
# TOOL=modeldoc container.
#
# Notes:
#  - The launch program is cs2mcp_launch.exe (crates/mm-launch), not cs2.exe: inside the container
#    PID 1 is the launcher, which starts cs2.exe suspended, injects cs2mcp_mmload.dll (the Metamod
#    loader path goes in CS2MCP_MM_LOADER), resumes it, waits and exits with the game's exit code.
#    Metamod therefore loads without any gameinfo.gi search path (bridge/metamod/README.md,
#    "Launching"); a launcher/DLL failure exits 3 with one `[cs2mcp-launch] FAIL: <reason>` line.
#  - The manifest (sdkenginetools.txt) is read at toolframework2 Init, so a bridge reinstall only
#    takes effect after a full runtime restart -> use `cycle` (after build-rs.sh/install-rs.sh).
#  - A modal in Hammer stops slot 13; if `up`/`open` times out, check `logs` and
#    take a screenshot (`spectacle -b -n -f -o shot.png`) for a stalled dialog.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib-root.sh
. "$HERE/lib-root.sh"
PROJECT="$(cs2mcp_project_root "$(cd "$HERE/.." && pwd)")"
APP="$PROJECT/workshop/run/app"
CACHE="$PROJECT/workshop/run/cache"
CONTAINER="${CONTAINER:-cs2tools}"
IMG="${IMG:-${CS2_MCP_IMAGE:-ghcr.io/personcopperfine/cs2-docker:latest}}"
PFX="$APP/compatdata/730/pfx"
DISC="$PFX/drive_c/users/steamuser/AppData/Local/cs2mcpbridge/session.json"
VTM_PAYLOAD="$APP/vtm_payload.kv3"
RS_LOG="$APP/mcp_bridge_rs.log"   # the Rust bridge's log file (crates/cs2-bridge/src/log.rs, the Windows path)
# The bridge's log tag is now a ROLE SET `[cs2-bridge <roles>]` with roles from {mm, tool}
# joined by '+' in that order (lane B1): the tool copy logs "[cs2-bridge mm+tool] Init" when
# it is both the Metamod plugin and the tool; old builds wrote "[cs2-bridge tool] Init" (still
# matched). A bare "[cs2-bridge mm] Init" proves nothing about the tool - never wait on it.
RS_INIT_RE='\[cs2-bridge (mm\+)?tool\] Init'
ADDON="${ADDON-mcp_test}"
POC_ASSET="${POC_ASSET:-maps/probe.vmap}"
WAIT_CHANNEL_S="${WAIT_CHANNEL_S:-180}"
WAIT_DISCOVERY_S="${WAIT_DISCOVERY_S:-90}"   # bridge load measured 22-34s (2026-10-12)
WAIT_DOC_S="${WAIT_DOC_S:-60}"
TOOL="${TOOL:-hammer}"
TOOL_LABEL="cs2mcp.tool"
MODELDOC_LOCK="$PROJECT/.podman/cs2mcp-modeldoc.lock"
# podman's state for this project lives here (AGENTS.md rule 7); the ModelDoc check must see it
export XDG_RUNTIME_DIR="${PODMAN_XDG:-$PROJECT/.podman/xdg}"

say() { printf '%s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

BRIDGE="${BRIDGE:-}"   # accepted: '', rust, rust-only - all mean the same thing now (one bridge)
METAMOD="${METAMOD:-}" # no-op alias since KHOOK-C1 (Metamod is installed unconditionally; -insecure is default)


xauth_file() {
    if [ -n "${XAUTHORITY:-}" ] && [ -f "$XAUTHORITY" ]; then
        printf '%s' "$XAUTHORITY"; return 0
    fi
    ls -t /run/user/"$(id -u)"/xauth_* 2>/dev/null | head -1
}

put_payload() {  # put_payload <kv3-body>
    {
        printf '<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\n'
        printf '{\n'
        printf '%s\n' "$1"
        printf '}\n'
    } > "$VTM_PAYLOAD"
}

vtmsend() {  # post the current payload; prints vtmsend output, nonzero on failure
    podman exec -i "$CONTAINER" bash -c 'cd /app && ./entrypoint.sh vtmsend.exe 2>&1'
}

wait_channel() {  # the VToolMessage mapping exists once toolframework2 Init ran
    put_payload $'\t_message = "command"\n\t_data = "echo mcp_runtime_channel_probe"'
    local i
    for i in $(seq 1 "$WAIT_CHANNEL_S"); do
        if vtmsend | grep -q 'creator version'; then
            say "  channel up after ${i}s"
            return 0
        fi
        sleep 1
    done
    die "VToolMessage channel never appeared (${WAIT_CHANNEL_S}s); the engine may not have started (needs the focus flags: +engine_no_focus_sleep 0 +r_always_render_all_windows 1) - see: podman logs $CONTAINER"
}

# The bridge load: a vtm `tool_request` for `cs2_bridge` (wp0-live.sh). Its proof line is the
# tool role's "[cs2-bridge tool] Init" / "[cs2-bridge mm+tool] Init" in its own log (see
# RS_INIT_RE above; crates/cs2-bridge/src/toolsys.rs `m0_Init`, tag from src/log.rs - the tag
# is a role set since lane B1). The Metamod copy writes to the same file under its own tag, so
# a bare "Init" or the mm-only tag would not prove the tool loaded. The load also waits for
# the discovery file the Rust listener writes on its first tick (WP2.D).
load_rs_bridge() {  # load_rs_bridge [discovery]
    local want_discovery="${1:-}"
    put_payload $'\t_message = "tool_request"\n\ttool_name = "cs2_bridge"\n\t_data = -1'
    local i
    for i in $(seq 1 "$WAIT_DISCOVERY_S"); do
        if grep -qE "$RS_INIT_RE" "$RS_LOG" 2>/dev/null; then
            say "  rust bridge Init after ${i}s"
            if [ "$want_discovery" = discovery ]; then
                for i in $(seq 1 "$WAIT_DISCOVERY_S"); do
                    [ -f "$DISC" ] && { say "  discovery file after ${i}s"; return 0; }
                    sleep 1
                done
                die "no discovery file after ${WAIT_DISCOVERY_S}s - the Rust listener did not start (Init ran, so check $RS_LOG for 'bridge: starting listener' / 'ipc: listening')"
            fi
            return 0
        fi
        if [ $((i % 5)) -eq 1 ]; then
            vtmsend >/dev/null 2>&1 || true
        fi
        sleep 1
    done
    grep -qE "$RS_INIT_RE" "$RS_LOG" 2>/dev/null || die "rust bridge never logged its tool-role Init (matching '$RS_INIT_RE') after ${WAIT_DISCOVERY_S}s - check $RS_LOG"
}

container_tool() {  # container_tool <name> -> its cs2mcp.tool label (empty if unlabelled/absent)
    podman inspect --format "{{ index .Config.Labels \"$TOOL_LABEL\" }}" "$1" 2>/dev/null || true
}

guard_modeldoc() {  # one ModelDoc instance at a time; the caller holds the flock
    local running
    running="$(podman ps --filter "label=$TOOL_LABEL=modeldoc" --format '{{.Names}}')" \
        || die "podman ps failed; cannot check for a running ModelDoc instance"
    [ -z "$running" ] || \
        die "a ModelDoc instance is already running ($(echo $running)); only one at a time - stop it explicitly: CONTAINER=<name> $0 down"
}

cmd_down() {
    podman rm -f "$CONTAINER" >/dev/null 2>&1 || true
    say "container $CONTAINER removed"
}

# pet's crash recovery (live 2026-10-13): a pet session that did not exit cleanly (every killed or
# wedged runtime) leaves `"Closed Correctly" = false` in game/_toolsettings/pet/settings.txt, and the
# next pet.open then blocks the main thread on the modal "Recover Files?". The autosaves themselves
# (content/_toolsautosave/pet/) are kept; only the flags are reset, and only when no other tools
# container shares the tree (it could have pet open and rewrite the file).
clear_pet_crash_flags() {
    local settings="$APP/game/_toolsettings/pet/settings.txt"
    [ -f "$settings" ] || return 0
    grep -q -E '"Closed ?Correctly( [0-9]+)?"[[:space:]]*=[[:space:]]*false' "$settings" || return 0
    local others
    # (a failing `podman ps` reads as "no other container" and the flags are reset: the same as a
    # fresh machine, and the only container we know of was just stopped by cmd_down)
    others="$(podman ps --filter "label=$TOOL_LABEL" --format '{{.Names}}' | grep -v -x "$CONTAINER" || true)"
    if [ -n "$others" ]; then
        say "WARNING: pet's settings flag an unclean exit but other tools containers share the tree ($others);"
        say "WARNING: flags left as is - the first pet.open in $CONTAINER will block on the modal 'Recover Files?'."
        say "WARNING: stop those containers (or reset $settings by hand while no pet runs) before opening particles."
        return 0
    fi
    sed -i -E 's/("Closed ?Correctly( [0-9]+)?"[[:space:]]*=[[:space:]]*)false/\1true/' "$settings"
    say "pet: cleared the unclean-exit flags (no Recover Files? modal); autosaves kept in content/_toolsautosave/pet/"
}

cmd_up() {
    case "$TOOL" in hammer|modeldoc) ;; *) die "TOOL must be hammer or modeldoc (got '$TOOL')" ;; esac
    case "$BRIDGE" in ''|rust|rust-only) ;; *) die "BRIDGE must be rust, rust-only or unset (got '$BRIDGE')" ;; esac
    # METAMOD=1 is a no-op alias now; -insecure is added unconditionally (Metamod:Source
    # refuses without it), unless EXTRA_ARGS already carries it.
    case " ${EXTRA_ARGS:-} " in
        *" -insecure "*) ;;
        *) EXTRA_ARGS="${EXTRA_ARGS:+$EXTRA_ARGS }-insecure"
           say "added -insecure to EXTRA_ARGS (Metamod:Source refuses without it)" ;;
    esac
    mkdir -p "$(dirname "$MODELDOC_LOCK")"
    exec 9>"$MODELDOC_LOCK"
    flock -w 30 9 || die "timed out waiting for $MODELDOC_LOCK (another runtime.sh up holds it)"
    if [ "$(container_tool "$CONTAINER")" = modeldoc ]; then
        die "$CONTAINER is the running ModelDoc instance; stop it explicitly first: CONTAINER=$CONTAINER $0 down"
    fi
    [ "$TOOL" = modeldoc ] && guard_modeldoc
    cmd_down
    # the tree is free of OUR container now: ensure the install is current (read-only check ->
    # repair by install, which refuses exit 4 while ANOTHER session's container mounts the
    # tree - its message explains what is missing/stale)
    say "checking the bridge install (install-rs.sh ensure)..."
    CS2_MCP_PROJECT_ROOT="$PROJECT" CS2_GAME="$APP/game" "$HERE/install-rs.sh" ensure || die "install-rs.sh ensure failed (rc $?) - not starting; fix the artefacts above (or: bridge/install-rs.sh install / --dry-run)"
    clear_pet_crash_flags
    rm -f "$DISC"
    # The Rust log is appended to across sessions, and the Metamod copy writes to it during engine
    # start, so rotate it before the launch: load_rs_bridge must only see this session's lines.
    [ -f "$RS_LOG" ] && mv -f "$RS_LOG" "$RS_LOG.prev"
    local xa; xa="$(xauth_file)"
    [ -n "$xa" ] && [ -f "$xa" ] || die "no XAUTHORITY file found (set XAUTHORITY)"
    local addon_arg=() addon_label=()
    [ -n "$ADDON" ] && addon_arg=(-addon "$ADDON") && addon_label=(--label "cs2mcp.addon=$ADDON")
    local env_arg=() v
    for v in CS2MCP_TESTPLUGIN_MODE CS2MCP_TESTPLUGIN_LOG; do
        [ -n "${!v:-}" ] && env_arg+=(-e "$v=${!v}")
    done
    say "launching $CONTAINER (tool=$TOOL, addon='${ADDON:-none}', xauth=$xa)"
    podman run -d --gpus all --name "$CONTAINER" --network=host \
        --label "$TOOL_LABEL=$TOOL" "${addon_label[@]}" "${env_arg[@]}" \
        -v "$APP:/app" -v "$CACHE:/cache" \
        -v /tmp/.X11-unix:/tmp/.X11-unix \
        -v "$xa:/root/.Xauthority:ro" \
        -e DISPLAY=:0 -e XAUTHORITY=/root/.Xauthority \
        --entrypoint /bin/bash "$IMG" -c \
        "cd /app; exec ./entrypoint.sh cs2mcp_launch.exe -tools -vulkan -console -condebug ${addon_arg[*]:-} ${EXTRA_ARGS:-} +engine_no_focus_sleep 0 +r_always_render_all_windows 1" \
        >/dev/null 9>&-   # 9>&-: the container must not inherit (and hold) the lock fd
    exec 9>&-   # the label is visible to `podman ps` from here on
    say "waiting for the VToolMessage channel (engine start)..."
    wait_channel
    say "loading cs2_bridge..."
    load_rs_bridge discovery
}

wait_doc() {  # poll the tool's active-document method until a document is open
    local i method=doc.active
    [ "$(container_tool "$CONTAINER")" = modeldoc ] && method=modeldoc.active
    for i in $(seq 1 "$WAIT_DOC_S"); do
        if python3 - "$DISC" "$method" <<'EOF' | grep -q '^open:'
import json, socket, sys, os
disc = json.load(open(sys.argv[1]))
s = socket.create_connection((disc["host"], disc["port"]), timeout=30)
f = s.makefile("rb")
def rpc(o):
    s.sendall((json.dumps(o) + "\n").encode())
    return json.loads(f.readline())
rpc({"jsonrpc": "2.0", "id": "h", "method": "control.hello",
     "params": {"protocol": 1, "token": disc["token"]}})
r = rpc({"jsonrpc": "2.0", "id": "d", "method": sys.argv[2], "params": {}})
d = (r.get("result") or {}).get("doc")  # an error reply (e.g. -32017) is not an open document
print("open:" + str(d.get("id", "?")) if isinstance(d, dict) else "none")
EOF
        then
            say "  document open after ${i}s"
            return 0
        fi
        sleep 1
    done
    say "warn: no document after ${WAIT_DOC_S}s (a modal may be up; screenshot to check)"
    return 1
}

cmd_open() {
    local asset="${1:-$POC_ASSET}"
    case "$asset" in
        *.vmdl|*.vmdl_c)
            [ "$(container_tool "$CONTAINER")" = modeldoc ] || \
                die "$asset is a model: open it in the ModelDoc instance (TOOL=modeldoc $0 up), not in $CONTAINER" ;;
    esac
    put_payload $'\t_message = "open_asset"\n\t_data = "'"$asset"'"'
    say "open_asset $asset"
    vtmsend >/dev/null
    wait_doc
    wait_modules
}

wait_modules() {  # wait until module identity checks reach a terminal state
    local i tool_dll=hammer.dll
    [ "$(container_tool "$CONTAINER")" = modeldoc ] && tool_dll=modeldoc_editor.dll
    for i in $(seq 1 60); do
        local out
        out=$(python3 - "$DISC" "$tool_dll" <<'EOF'
import json, socket, sys
disc = json.load(open(sys.argv[1]))
s = socket.create_connection((disc["host"], disc["port"]), timeout=10)
f = s.makefile("r")
def rpc(o):
    s.sendall((json.dumps(o) + "\n").encode())
    return json.loads(f.readline())
rpc({"jsonrpc": "2.0", "id": "h", "method": "control.hello",
     "params": {"protocol": 1, "token": disc["token"]}})
r = rpc({"jsonrpc": "2.0", "id": "m", "method": "control.capabilities", "params": {}})
mods = r["result"]["modules"]
states = {k: v["state"] for k, v in mods.items()}
# every tool loads these plus its own DLL (argv[2]); the rest (pet, the other tool, ...) load on
# demand, so not_loaded there is a final state, not a pending one
core = ("tier0.dll", "engine2.dll", "toolframework2.dll", "scenesystem.dll", "schemasystem.dll",
        "rendersystemvulkan.dll", sys.argv[2])
pending = [k for k, v in states.items()
           if v == "verifying" or (v == "not_loaded" and k in core)]
print("pending:" + ",".join(pending) if pending else "done:" + ",".join("%s=%s" % kv for kv in states.items()))
EOF
        )
        case "$out" in
            done:*) say "  modules: ${out#done:}"; return 0 ;;
            pending:*) sleep 1 ;;
            *) sleep 1 ;;
        esac
    done
    say "warn: module verification did not settle in 60s"
    return 1
}

cmd_test() {
    python3 "$HERE/test_client.py"
}

cmd_status() {
    say "container: $(podman ps --filter "name=^${CONTAINER}$" --format '{{.Status}}' || true)"
    if [ -f "$DISC" ]; then
        say "discovery: $(cat "$DISC" | tr -d '\n ')"
        cmd_test || true
    else
        say "discovery: absent"
    fi
}

cmd_logs() {
    local n="${1:-40}"
    say "=== bridge log (last $n) ==="
    tail -n "$n" "$RS_LOG" 2>/dev/null || say "(no bridge log)"
    say "=== game console (last $n) ==="
    tail -n "$n" "$APP/game/csgo/console.log" 2>/dev/null || say "(no console log)"
}

cmd_cycle() {
    local asset="${1:-$POC_ASSET}"
    cmd_up
    cmd_open "$asset"
    cmd_test
    say "cycle complete (rebuild + reinstall first: bridge/build-rs.sh, bridge/install-rs.sh install)"
}

cmd="${1:-}"; shift || true
case "$cmd" in
    down)    cmd_down ;;
    up)      cmd_up ;;
    open)    cmd_open "$@" ;;
    test)    cmd_test ;;
    cycle)   cmd_cycle "$@" ;;
    status)  cmd_status ;;
    logs)    cmd_logs "$@" ;;
    *) sed -n '2,30p' "${BASH_SOURCE[0]}"; exit 1 ;;
esac
