#!/bin/bash
# install-rs.sh - install / uninstall / status / ensure for the Rust bridge and the
# Metamod:Source 2.0 layout, GAMEINFO-FREE (mm-launch lanes, 2026-10-20; DESIGN §2, §6):
#   --platform wine (default)  the shared Wine tools install. Metamod lives in the STANDARD
#                              game/csgo/addons/metamod/bin/win64 layout now; the launcher
#                              (cs2mcp_launch.exe) starts cs2.exe suspended, injects
#                              cs2mcp_mmload.dll and passes the loader path in CS2MCP_MM_LOADER
#                              (target/CONTRACT.md "Runtime"). The standard addons layout works
#                              under -tools now that NO search path is added: the old -tools
#                              abort was caused by the search path, not the files (spike
#                              2026-10-20, DESIGN §0; live-verified: bridge + tool_request on
#                              ONE image, gameinfo pristine).
#   --platform linux           the cs2-home volume's Linux game dir, the STANDARD
#                              csgo/addons/metamod layout (client + dedicated), loaded by
#                              LD_PRELOAD=.../libcs2mcp_mmload.so on the cs2.sh command (the
#                              launch scripts' lane, not this script).
# The gameinfo path is REMOVED entirely (user decision 2026-10-20, DESIGN §6): no SearchPaths
# edit, no `Write csgo`, no VSP loader vdfs. Metamod's loader finds the real game DLL itself
# by parsing the UNMODIFIED gameinfo (loader/gamedll.cpp mm_DetectGameInformation: every
# `Game` path's bin/<os>/server, skipping its own path - DESIGN §0), and the spike showed
# DEFAULT_WRITE_PATH lands on csgo/ with no `Write csgo` line.
# The layouts and the launch side are documented in bridge/metamod/README.md: "Launching",
# "The layout (both platforms)" and "Why the old `-tools` layout was different".
# (`--metamod` is still accepted: a no-op alias - Metamod is now installed unconditionally,
# custom gamemode plugins are the end goal and load through it.)
#
#   bridge/install-rs.sh [--dry-run] [--platform wine|linux] install | uninstall | status | ensure
#
# install (wine) - ONE bridge image, NO gameinfo edit:
#   target/bridge-rs/win64/cs2_bridge.dll ->  <game>/bin/win64/tools/cs2_bridge.dll
#       (the DLL is refused unless check-exports.sh finds exactly the CreateInterface export)
#   + the sdkenginetools.txt entry `m_Name = "cs2_bridge"` inserted before the hammer entry
#     (exact text live-verified in WP0: worktrees/rust-bridge/target/orch/rust-bridge/wp0-live.sh;
#     the ONE remaining game-file edit)
#   + the launcher artefacts (NOT export-gated: the launcher is an .exe and the mmload DLL is
#     an IAT hook, neither has the tool-DLL CreateInterface contract):
#     target/bridge-rs/win64/cs2mcp_launch.exe -> game/bin/win64/cs2mcp_launch.exe
#     target/bridge-rs/win64/cs2mcp_mmload.dll -> game/bin/win64/cs2mcp_mmload.dll
#     (both missing with nothing installed -> the "run bridge/build-rs.sh" refusal; an
#     installed copy with no build counts as current, like the tool DLL below)
#   + the Metamod 2.0 files, STANDARD layout (build source unchanged: $MM_BUILD/metamod/...):
#     game/csgo/addons/metamod/bin/win64/server.dll            # the loader
#     game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll     # the core
#     game/csgo/addons/metamod/metaplugins.ini                 # config stays where the core looks
#     game/csgo/addons/metamod/cs2mcp.vdf                      # Metamod 2.0 plugin registration
#   ...and NOTHING else: no gameinfo.gi edit, no game/metamod/, no content/metamod/ (the old
#   -tools layout; the migration below removes them from old trees).
#
#   The vdf's "file" is `../bin/win64/tools/cs2_bridge.dll`, i.e. the SAME tools-DLL copy the
#   manifest entry loads, so Metamod and toolframework2 hold ONE image. The format the core
#   parses is cited below; the resolution rule (spike wine-single-image, 2026-10-20):
#     core/metamod.cpp:1103-1150  MetamodSource::GetFullPluginPath resolves a relative "file"
#         verbatim against the mod dir (game/csgo), so `../bin/win64/tools/cs2_bridge.dll`
#         lands on <game>/bin/win64/tools/cs2_bridge.dll. Metamod's provider (loaded first,
#         -insecure) dlopens the tool DLL, and toolframework2's manifest-driven load then
#         refcounts the same HMODULE instead of mapping a second image.
#     That is also why install never copies the DLL to
#     game/csgo/addons/cs2mcp/bin/win64/cs2_bridge.dll (the pre-KHOOK-C1 second copy): a second
#     copy would map a SECOND image. Uninstall and status still handle that legacy path for
#     trees an install made before that change left behind (LEGACY_* below).
#
#   The vdf is the Metamod 2.0 format the core actually parses - the root key is not checked,
#   the keys are (comment cites the source):
#     core/provider/source2/provider_source2.cpp:170  Source2Provider::ProcessVDF loads KeyValues
#         "Metamod Plugin" and reads "file" (+ optional "alias"); anything else is ignored.
#     core/metamod.cpp:483  the vdf dir is <mod>/addons/metamod (mm_basedir default
#         "addons/metamod" in DoInitialPluginLoads), with mod = the csgo mod dir
#         -> game/csgo/addons/metamod/.
#
# install (linux) - add files only, NO gameinfo edit. The vdf there DOES point at the addons
# copy: on Linux there is no `-tools` manifest, Metamod (standard layout) dlopens the plugin
# straight out of addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so - a single image again.
#   target/bridge-rs/linuxsteamrt64/libcs2_bridge.so -> <game>/csgo/addons/cs2mcp/bin/linuxsteamrt64/
#       (export gate first, like the DLL; check-exports.sh runs on it)
#   target/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so -> the same dir (the LD_PRELOAD
#       loader; NOT gated: it exports dlopen, not CreateInterface)
#   + csgo/addons/metamod/ = the linux metamod build (<cs2-mcp>/target/metamod/linux/addons/
#       metamod: bin/linux64/server.so, bin/linuxsteamrt64/{libserver.so,metamod.2.cs2.so},
#       metaplugins.ini, README.txt), copied whole
#   + csgo/addons/metamod/cs2mcp.vdf: "file" "addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so"
#   ...and NOTHING else: no gameinfo.gi lines, no csgo/addons/metamod.vdf /
#   csgo/addons/metamod_x64.vdf (the VSP loader files - unused without the search path,
#   DESIGN §2; migration removes them from old trees).
#   The whole Linux exchange runs as `podman unshare bash -s <args> <<heredoc` (the volume's
#   files are owned by a subuid): the script body is fixed text, every path is passed as a
#   positional argument - never interpolated into the script body.
#   Linux CLIENT launches need -insecure: Metamod:Source refuses without `-dedicated` or
#   `-insecure` (spike client-noinsecure: "MMS: Fatal error: Metamod:Source requires -dedicated
#   or -insecure"); the flag belongs to the client launch lane (client-mcp), not this script.
#
# migration (both platforms; install - and ensure's repair - run it BEFORE the new backup is
# taken and before the new set is laid down): a tree carrying the OLD (gameinfo-era) layout -
# our `Game metamod` + `Write csgo` (wine) / `Game csgo/addons/metamod` + `Write csgo` (linux)
# gameinfo lines, the Wine game/metamod/ + content/metamod/, the Linux loader vdfs - is
# migrated: exactly what an OLD install of this tree recorded as ours is removed, nothing else.
# old_classify decides per old file, from the backups' records (the same reader uninstall uses):
#   delete   INSTALLED by us and not owned before our first install (the pre-first-install rule:
#            an INSTALLED entry the oldest backup's SHA256SUMS lists is the owner's, never deleted)
#   restore  WROTE_OVER by us and the current sha256 matches the WROTE_OVER chain (the same chain
#            walk uninstall uses: the owner's bytes come back)
#   leave    already the owner's bytes / already restored (no marker, so ensure never loops)
#   unknown  no record attributes it to us -> REFUSE
# A gameinfo line is removed ONLY when some backup's EDITS_WROTE recorded its kind (the anchored
# per-line regexes of the old install - a line we did not write is never touched; an unrecorded
# `Write csgo` is left alone). Anything unattributable (a foreign `Game metamod` line, a file in
# an old location no INSTALLED/WROTE_OVER entry lists) REFUSES with a clear message BEFORE
# anything is touched and before a backup exists - no guessing; uninstall first, or remove the
# foreign artefacts by hand. The emptied old dirs are rmdir'd; the gameinfo revert is the last
# step. After a migration the NEW backup's EDITS_WROTE gets a `reverted:<kind>` line per
# gameinfo kind that was removed: it CANCELS that kind in the old records, so a same-text line
# somebody else adds later is foreign (refused / left alone), not ours. Old backups are NOT
# retired by a migration: uninstall keeps working from them (it removes any remaining
# old-layout artefacts it owns and restores what they say). Fault points (INSTALL_RS_FAULT,
# exit 99): `pre-mv:migrate:<dest>` inside the migration.
#
# ensure (launch paths call it before every start):
#   Read-only check first (needs neither write access nor the container guard): every install
#   artefact present and current:
#     - the bridge DLL/SO and the launcher exe/dll/mmload-so sha256-equal to the built
#       artefact WHEN THE ARTEFACT EXISTS; no build artefact + an installed copy counts as
#       current; no build artefact + nothing installed fails "build it first" (exit 1)
#     - the manifest edit present ("current" = the file is what install's edit function would
#       produce from the CURRENT file - run the edit on a copy and cmp, item 11 - AND the
#       Valve-owned anchors install relies on still there: the manifest's pre-install anchor
#       entries (e.g. the hammer entry), recorded as stripped-line fingerprints in the backup's
#       ANCHORS file at install time, defaults when no backup exists. The anchor comparison is
#       NORMALISED (r4 item 2): \r stripped, whitespace runs collapsed to one space, blank and
#       // comment lines ignored. "game file damaged" is reported ONLY when an anchor the edit
#       needs is missing; the message names the anchor and says: verify the game files; if
#       Valve changed this line, update install-rs.sh's anchors (apply_manifest_edit).)
#     - the vdf byte-equal to what install writes
#     - every Metamod file sha256-equal to the metamod build dir
#     - NO old-layout marker left: our old gameinfo lines, the Wine game/metamod/ +
#       content/metamod/, the Linux loader vdfs. Any of them reads STALE ("old layout -
#       repair migrates it away"); the repair (install) migrates or refuses per the
#       attribution rules above.
#   It prints one line per artefact (ok / missing / stale: <why>) and exits 0 when all are ok
#   without touching anything. Only when something is missing/stale: refuse (exit 4) if any
#   running container mounts the tree, else run `install` (which backs up first, as install
#   always does).
#   Exit codes: 0 ok/repaired, 1 failure (fatal: "build it first" or "game file damaged"), 2
#   usage, 3 --dry-run stale (dry-run reports and exits 3 instead of repairing), 4 refused
#   (busy). "Current" no longer involves gameinfo.gi at all: a Steam update that rewrites it
#   cannot undo any of our edits (we make none), so there is nothing to re-apply.
#
# Every file that exists BEFORE install is backed up first, under
#     <cs2-mcp>/target/bridge/backup-rs-<UTC>/ (wine) / backup-linux-<UTC>/ (linux)
# with APP_TREE (+ SHA256SUMS listing only the backed-up files), paths relative to the game
# tree, and a WROTE_OVER list: the exact paths install REPLACED (existed before, written by
# us) with the sha256 of the bytes WE wrote (planned before the backup is finalised, rewritten
# as the actual list after a successful install - a killed install still uninstalls). Linux
# backup copies are taken inside `podman unshare`, so the volume's subuid-owned files come out
# readable by the host user. uninstall restores a path ONLY when it is in WROTE_OVER AND its
# current sha256 still equals what we wrote in SOME install of this tree (r5, the chain rule:
# the NEWEST backup whose WROTE_OVER sha matches the current sha is the starting point; if its
# copy is itself what an OLDER backup recorded as written, step to that older backup's copy,
# and repeat - the last copy reached is the bytes that existed before we FIRST wrote the
# path); a path gone or changed after our LAST write (removed by its owner, Steam/user edit)
# is LEFT ALONE with a message, and a failed restore is a warning
# for that path, never a stuck uninstall. Files that existed but that we did NOT write are
# never touched (no "restore every file the backup lists"); metaplugins.ini is never in
# WROTE_OVER (create-if-absent only) and never restored or deleted unless we created it
# unchanged. The INSTALLED union (the files install CREATED, over every backup of this tree)
# drives the deletions - and it never deletes a PRE-INSTALL file: an entry the OLDEST backup's
# SHA256SUMS lists is skipped (the WROTE_OVER restore puts the owner's bytes back instead).
# Every backup that contributes a WROTE_OVER/INSTALLED entry is SHA256SUMS-verified first
# (not only the oldest).
# The manifest edit revert (EDITS_WROTE) runs LAST, after the file work succeeded. The
# gameinfo-era backups also recorded gameinfo kinds in EDITS_WROTE and gameinfo anchors in
# ANCHORS: this script still HONOURS those records (uninstall and the migration revert the
# lines they name) while writing no new ones - a new backup's EDITS_WROTE carries the manifest
# edit only, and a new linux backup's is empty (linux edits no game file at all now). The
# gameinfo kinds a tree's backups record are taken as the UNION over every backup of the tree
# (a line any install of ours wrote is ours; the oldest backup alone would miss a line a
# later repair re-wrote after a Steam update wiped it).
# The oldest backup is the pre-first-install image (see oldest_backup below; verified with
# cmp); an existing dir is never created or deleted - only the specific dirs install created,
# and rmdir only unlinks them if empty.
# ACCEPTED LIMITATION: a foreign file at one of our OWN dest paths (e.g. another plugin's
# cs2mcp.vdf / metamod.2.cs2.so) is overwritten by install/ensure - ensure owns the Metamod
# core files. Uninstall gives the owner's bytes back (the WROTE_OVER restore), but while
# installed, ours win.
#
# install/uninstall (and ensure when it must repair) refuse while ANY running container mounts
# the tree (read-only `podman ps` + a mounts check; wine: a bind whose source is the game's
# app dir or a parent/child of it; linux: a container using the volume cs2-home - `podman ps
# --filter volume=...` plus a mounts check). A FAILING podman refuses - it never guesses
# "nothing is running". The refusal message names the containers. status and ensure's
# read-only check work with no container running and without write access.
#
#   CS2_GAME            wine game dir  (default .../workshop/run/app/game; fake trees in tests)
#   CS2_LINUX_GAME      linux game dir (default the cs2-home volume's .../Counter-Strike
#                        Global Offensive/game; when set the volume is NOT inspected; fake
#                        trees in tests)
#   CS2_MCP_DIR         cs2-mcp root   (backup dir + default metamod builds; default the
#                        repo's main checkout, bridge/lib-root.sh: a linked worktree uses the
#                        main one's target/, where the shared builds are)
#   CS2_MCP_PROJECT_ROOT the project root (default the main checkout; CS2DOCKER_ROOT is the
#                        deprecated alias): its workshop/run/app is the REAL Wine tree, its
#                        .podman/ holds podman's XDG dir and the install lock
#   CS2_BRIDGE_DLL      overrides the wine tool DLL (default: $ROOT/target/bridge-rs/win64/cs2_bridge.dll)
#   CS2_BRIDGE_LINUX_SO overrides the linux .so (default: $ROOT/target/bridge-rs/linuxsteamrt64/libcs2_bridge.so)
#   CS2_MM_EXE          overrides the wine launcher exe (default: $ROOT/target/bridge-rs/win64/cs2mcp_launch.exe)
#   CS2_MM_DLL          overrides the wine mmload DLL (default: $ROOT/target/bridge-rs/win64/cs2mcp_mmload.dll)
#   CS2_MMLOAD_SO       overrides the linux mmload .so (default: $ROOT/target/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so)
#   CS2_MM              overrides the wine metamod build (default: <cs2-mcp>/target/metamod/win64/addons)
#   CS2_MM_LINUX        overrides the linux metamod build (default: <cs2-mcp>/target/metamod/linux/addons)
#   PODMAN              overrides the podman binary (default podman; test stubs take this)
#   PODMAN_XDG          podman's XDG_RUNTIME_DIR (default <project root>/.podman/xdg)
#   GUARD_FAKE=1        run the busy-container guard even on an overridden (fake) tree - the
#                        host test bridges this with a stub podman to exercise the guard
#   CS2DOCKER_ROOT      deprecated alias of CS2_MCP_PROJECT_ROOT (the root for the install lock);
#                        the lock file is <root>/.podman/install-rs-<wine|linux>.lock, the SAME
#                        file whatever checkout runs the script. install/uninstall/ensure hold
#                        it exclusive, status/--dry-run shared; a 90 s wait (below C2's 120 s)
#                        then exit 4 "another install-rs.sh holds <lockfile>".
#   INSTALL_RS_LOCK_WAIT  test-only override of the lock wait seconds
#   INSTALL_RS_REAL_APP  TEST-ONLY: overrides the real Wine app dir (the realpath'd guard
#                        decision needs a "real" tree; the host test points it at a fake root)
#   INSTALL_RS_FAULT=<point>  TEST-ONLY (bridge/test_install_rs.sh): simulate a kill - exit 99
#                        at that point ("pre-mv:<app-relative-dest>" [a tree file's temp is
#                        written, the rename is NOT done], "mid-backup", and the uninstall/
#                        migration rename points "pre-mv:uninstall:<dest>" /
#                        "pre-mv:uninstall-restore:<dest>" / "pre-mv:migrate:<dest>"); used to
#                        prove a killed run leaves the original file intact, none of our temps
#                        or *.partial backup dirs behind, and the next ensure repairs
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
# shellcheck source=lib-root.sh
. "$HERE/lib-root.sh"
MCP_DIR="${CS2_MCP_DIR:-$(cs2mcp_main_checkout "$ROOT")}"   # backups + the shared metamod builds live in the canonical target/
PROJECT="$(cs2mcp_project_root "$ROOT")"

# the REAL trees (test-only override: INSTALL_RS_REAL_APP, used by bridge/test_install_rs.sh to
# exercise the realpath'd real-tree decision without ever touching the real trees)
REAL_APP="${INSTALL_RS_REAL_APP:-$PROJECT/workshop/run/app}"
REAL_GAME="$REAL_APP/game"
PODMAN="${PODMAN:-podman}"
BACKUP_ROOT="$MCP_DIR/target/bridge"
export XDG_RUNTIME_DIR="${PODMAN_XDG:-$PROJECT/.podman/xdg}"
LINUX_GAME="${CS2_LINUX_GAME:-}"
LINUX_VNAME="cs2-home"

usage() { echo "usage: $0 [--dry-run] [--metamod] [--platform wine|linux] install|uninstall|status|ensure" >&2; }

# --- args -----------------------------------------------------------------------------------
DRY=0 METAMOD=0 PLATFORM="" CMD=""
consume_platform=0
while [ $# -gt 0 ]; do
    if [ "$consume_platform" = 1 ]; then
        consume_platform=0
        case "$1" in
            wine|linux) PLATFORM="$1" ;;
            *) echo "install-rs.sh: --platform must be wine or linux (got '$1')" >&2; exit 2 ;;
        esac
        shift
        continue
    fi
    case "$1" in
        --dry-run) DRY=1 ;;
        --metamod) METAMOD=1 ;;   # no-op alias (old scripts); Metamod is always installed
        --platform) consume_platform=1 ;;
        --platform=*) PLATFORM="${1#*=}" ;;
        install|uninstall|status|ensure)
            [ -z "$CMD" ] || { echo "install-rs.sh: one command only (already '$CMD', also got '$1')" >&2; exit 2; }
            CMD=$1 ;;
        *) echo "install-rs.sh: unknown arg '$1'" >&2; usage; exit 2 ;;
    esac
    shift
done
[ -n "$CMD" ] || { usage; exit 2; }
[ -z "$PLATFORM" ] && PLATFORM=wine

say() { printf '[install-rs] %s\n' "$*" >&2; }
die() { say "FAIL: $*"; exit 1; }
die4() { say "REFUSED(busy): $*"; exit 4; }
sha_of() { sha256sum "$1" | awk '{print $1}'; }

# --- our temp files (always under the checkout's target/, never /tmp) -------------------------
# Every temp install-rs.sh creates is registered here and removed by the EXIT/TERM/INT trap -
# a killed run leaves none of our temps behind. (Backup PARTIAL dirs register in PARTIAL_BK.)
TEMPS=()
PARTIAL_BK=""
FATAL_MSG=""   # ensure's explanation for a fatal (exit 1): "build it first" vs "game file damaged"
cleanup_temps() {
    local t
    if [ "${#TEMPS[@]}" -gt 0 ]; then
        for t in "${TEMPS[@]}"; do [ -n "$t" ] && rm -f "$t"; done
    fi
    [ -n "$PARTIAL_BK" ] && rm -rf "$PARTIAL_BK"
    return 0
}
trap cleanup_temps EXIT
trap 'trap - EXIT; cleanup_temps; exit 130' INT
trap 'trap - EXIT; cleanup_temps; exit 143' TERM

mktemp_under_target() {  # sets TMP_PATH: a temp file under <checkout>/target/ (procedure: see
                         # tmp_for - never call in a $(...), the registration would be lost)
    mkdir -p "$ROOT/target"
    if ! TMP_PATH="$(mktemp "$ROOT/target/${1}.XXXXXX")"; then
        say "FAIL: cannot create a temp file under $ROOT/target (mktemp failed) - check that the directory exists and is writable"
        exit 1
    fi
}

# INSTALL_RS_FAULT=<point> (TEST-ONLY, not an API): simulate a kill - exit 99 at the named
# point - so bridge/test_install_rs.sh can prove a killed run leaves the original file intact,
# none of our temps/partials behind, and the next ensure repairs. Points:
#   "pre-mv:<app-relative-dest>"  a tree file's temp content is WRITTEN, the rename is NOT done
#   "mid-backup"                  the backup copies are written, the backup is NOT finalised
fault() {
    if [ "${INSTALL_RS_FAULT:-}" = "$1" ]; then
        say "FAULT HOOK (test-only): simulating a kill at '$1'"
        exit 99
    fi
}

# --- atomic writes into a game tree ------------------------------------------------------------
# Every file written into a tree goes to a temp in the SAME dir, then mv -f: a killed run can
# never leave a half-written game file. To keep owner/mode (linux inside podman unshare: the
# file belongs to the container uid) the temp is pre-seeded with `cp -p <orig>` when the
# original exists, the new content is written INTO the temp, then mv -f. Temps are registered
# in TEMPS and the EXIT/TERM/INT trap removes them. NOTE: tmp_for/linux_vdf_file are
# PROCEDURES (they set TMP_PATH/VDF_TMP) - running them in a $(...) would lose the TEMPS
# registration to the subshell.
TMP_PATH=""
tmp_for() {  # tmp_for <dest>: sets TMP_PATH = a pre-seeded temp in dest's dir (owner/mode kept)
    local dest="$1" t
    t="$dest.tmp.$$"
    if [ -e "$dest" ]; then cp -p "$dest" "$t"; else : > "$t"; fi
    TEMPS+=("$t")
    TMP_PATH="$t"
}
commit_tmp() {  # commit_tmp <tmp> <dest> <fault-point>: the (fault-hooked) rename
    fault "pre-mv:$3"
    mv -f "$1" "$2" || { rm -f "$1"; die "rename over $2 failed"; }
}

# r3 NIT: a SIGKILLed run's temps (<dest>.tmp.<pid>) survive (no trap runs) - the next run
# removes OURS, matching our exact temp naming only: <dest>.tmp.<all-digits>.
sweep_dest_temps() {  # sweep_dest_temps <app-dir> <dest-relative-to-app>... : prints/removes
    local app="$1"; shift
    local rel t s
    for rel in "$@"; do
        [ -d "$app/$(dirname "$rel")" ] || continue
        for t in "$app/$rel".tmp.*; do
            [ -e "$t" ] || continue
            s="${t##*.}"
            case "$s" in ''|*[!0-9]*) continue ;; esac   # digits only = ours
            rm -f "$t" && say "  removed a killed run's stray temp: $rel.tmp.$s"
        done
    done
    return 0
}
is_our_temp_name() {  # rc 0 when the path is <something>.tmp.<all-digits> (our temp naming)
    case "$1" in
        *.tmp.*) case "${1##*.}" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac ;;
        *) return 1 ;;
    esac
}

# --- per-platform config --------------------------------------------------------------------
if [ "$PLATFORM" = wine ]; then
    GAME="${CS2_GAME:-$REAL_GAME}"
    APP="$(dirname "$GAME")"
    MAN="$GAME/bin/sdkenginetools.txt"
    GI="$GAME/csgo/gameinfo.gi"   # not edited any more; only the migration/uninstall REVERT of
                                  # our old lines touches it (old-layout trees only)
    GI_LABEL="game/csgo/gameinfo.gi"
    RS_LIB="${CS2_BRIDGE_DLL:-$ROOT/target/bridge-rs/win64/cs2_bridge.dll}"
    LAUNCH_EXE="${CS2_MM_EXE:-$ROOT/target/bridge-rs/win64/cs2mcp_launch.exe}"
    LOAD_DLL="${CS2_MM_DLL:-$ROOT/target/bridge-rs/win64/cs2mcp_mmload.dll}"
    MM_BUILD="${CS2_MM:-$MCP_DIR/target/metamod/win64/addons}"
    BK_PREFIX="backup-rs"
else
    RS_LIB="${CS2_BRIDGE_LINUX_SO:-$ROOT/target/bridge-rs/linuxsteamrt64/libcs2_bridge.so}"
    MMLOAD_SO="${CS2_MMLOAD_SO:-$ROOT/target/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so}"
    MM_BUILD="${CS2_MM_LINUX:-$MCP_DIR/target/metamod/linux/addons}"
    BK_PREFIX="backup-linux"
    GI_LABEL="csgo/gameinfo.gi"
fi

lrun() {  # lrun <args...> <<heredoc: run a fixed-text script with positional args. Real podman
          # runs it inside `podman unshare bash -s` (the volume's subuid-owned files); the test
          # stub's `unshare` just runs "$@". The script body is a constant heredoc; paths NEVER
          # go into the body, only into the "$1"... arguments. The lock fd is NOT inherited.
    "$PODMAN" unshare bash -s "$@" 8>&-
}

# r3 NIT: RS_LIB defaults to THIS checkout's build while the backups + the metamod builds come
# from CS2_MCP_DIR (the canonical checkout) - kept (documented in client-mcp/AGENT.md by lane
# C2), but every install/ensure says which checkout's build it is installing, and status shows
# which build the installed lib matches.
say_build_checkout() {
    say "bridge build: $RS_LIB (this checkout: $ROOT; backups + metamod builds: $MCP_DIR)"
}
lib_build_match() {  # lib_build_match <installed-lib> <candidate-build...> -> text: which
                     # build dir's bytes the installed lib is (or 'matches no build here')
    local sha cand d
    [ -f "$1" ] || return 0
    sha="$(sha_of "$1")"
    local -a hits=()
    for cand in "${@:2}"; do
        [ -f "$cand" ] || continue
        [ "$(sha_of "$cand")" = "$sha" ] || continue
        for d in ${hits[@]+"${hits[@]}"}; do [ "$d" = "$cand" ] && continue 2; done
        hits+=("$cand")
    done
    if [ "${#hits[@]}" -gt 0 ]; then
        printf 'matches build %s' "${hits[0]}"
        if [ "${#hits[@]}" -gt 1 ]; then
            local i
            for ((i = 1; i < ${#hits[@]}; i++)); do printf ' and %s' "${hits[$i]}"; done
        fi
    else
        printf 'matches no build here'
    fi
}

# --- single-writer lock (item 8) ------------------------------------------------------------------
# Same file whatever checkout runs the script: `<project root>/.podman/install-rs-<platform>.lock`
# (the project root of bridge/lib-root.sh: a linked worktree resolves to the main checkout). install/uninstall/ensure hold it EXCLUSIVE (client-mcp's
# C2 lane runs ensure under its own launch lock and waits max 120 s - we wait 90 s below that,
# then exit 4, which C2 reads as busy). status / --dry-run take it SHARED. INSTALL_RS_LOCK_WAIT
# overrides the wait seconds (test-only; documented in the header).
LOCK_WAIT="${INSTALL_RS_LOCK_WAIT:-90}"
# r3 NIT: a non-numeric wait is a USAGE error (exit 2), not a silent 90
case "$LOCK_WAIT" in
    ''|*[!0-9]*) say "FAIL: INSTALL_RS_LOCK_WAIT must be an integer number of seconds (got '$LOCK_WAIT')"; exit 2 ;;
esac
LOCK_ROOT="$PROJECT/.podman"
LOCK_FILE="$LOCK_ROOT/install-rs-$PLATFORM.lock"
mkdir -p "$LOCK_ROOT" 2>/dev/null || die "cannot create $LOCK_ROOT (needed for the install-rs lock)"
exec 8>>"$LOCK_FILE"
case "$CMD" in
    status) flock -s -w "$LOCK_WAIT" 8 || die4 "another install-rs.sh holds $LOCK_FILE (waited ${LOCK_WAIT}s; status needs it shared)" ;;
    *)
        if [ "$DRY" = 1 ]; then
            flock -s -w "$LOCK_WAIT" 8 || die4 "another install-rs.sh holds $LOCK_FILE (waited ${LOCK_WAIT}s; dry-run needs it shared)"
        else
            flock -x -w "$LOCK_WAIT" 8 || die4 "another install-rs.sh holds $LOCK_FILE (waited ${LOCK_WAIT}s)"
        fi ;;
esac
say "lock: $LOCK_FILE (${LOCK_WAIT}s wait)"

# --- wine dest lists ------------------------------------------------------------------------
# Everything wine install writes, paths relative to the app dir. The uninstall remove-list and
# the status list come from these arrays so they stay in sync with the install steps below.
TOOL_DESTS=(game/bin/win64/tools/cs2_bridge.dll)
EDIT_DESTS=(game/bin/sdkenginetools.txt)   # the ONE remaining game-file edit (gameinfo.gi is
                                           # NOT touched any more - header "install (wine)")
MM_DESTS=(game/csgo/addons/metamod/bin/win64/server.dll
          game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll
          game/csgo/addons/metamod/metaplugins.ini
          game/csgo/addons/metamod/cs2mcp.vdf)
# the launcher artefacts (mm-launch lane L1; NOT export-gated - not tool DLLs, header)
LAUNCH_DESTS=(game/bin/win64/cs2mcp_launch.exe
              game/bin/win64/cs2mcp_mmload.dll)
# LEGACY: installs made before KHOOK-C1 ALSO copied the bridge DLL a second time where the old
# vdf pointed (game/csgo/addons/cs2mcp/...). The vdf resolves ../bin/win64/tools/cs2_bridge.dll
# onto the tool copy (ONE image - header comment), so no install ever creates any of this, but
# uninstall/migration must still REMOVE it from a tree an old script installed. Never touched
# by the new install.
LEGACY_MM_DESTS=(game/csgo/addons/cs2mcp/bin/win64/cs2_bridge.dll)
# LEGACY dirs: rmdir-if-empty after the legacy file is gone (bottom-up).
LEGACY_MM_DIRS=(game/csgo/addons/cs2mcp/bin/win64 game/csgo/addons/cs2mcp/bin game/csgo/addons/cs2mcp)
# OLD (gameinfo-era) layout: game/metamod/ + content/metamod/ + our gameinfo lines. Never
# created by this script; migration removes what old installs recorded as ours and then these
# dirs are rmdir'd if empty (uninstall does the same). game/metamod/_iconcache is not ours
# either: the engine creates it (empty) under every search-path root, so the old `Game metamod`
# line left one in every real tree - rmdir'd like the rest when empty. content/ itself is NOT in
# the list: it is Valve's content root, never ours to remove, even when empty.
OLD_MM_DIRS=(game/metamod/_iconcache game/metamod/bin/win64 game/metamod/bin game/metamod
             content/metamod)
# bottom-up so rmdir (which only succeeds when empty) unwinds exactly what install created.
# game/bin/win64/tools (+ its parents) are in the list for completeness: in the REAL tree they
# pre-exist (the other tools' DLLs live there) so rmdir just fails - but on a minimal test tree
# install created them and uninstall must remove them again.
MM_DIRS=(game/csgo/addons/metamod/bin/win64 game/csgo/addons/metamod/bin
         game/csgo/addons/metamod game/csgo/addons
         game/bin/win64/tools game/bin/win64)

# --- linux dest lists (relative to the Linux GAME dir) ----------------------------------------
# install copies the metamod build dir WHOLE; its file list is enumerated from the build dir at
# run time (backup pre-listing + uninstall), plus these flat destinations:
LN_FLAT_DESTS=(csgo/addons/metamod/cs2mcp.vdf
               csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so
               csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so)
# OLD (gameinfo-era) layout (never created by this script): the VSP loader vdfs
# csgo/addons/metamod{,_x64}.vdf + our gameinfo lines; linux_old_scan looks for them, migration
# removes what old installs recorded as ours (uninstall does the same through the records).
# bottom-up so rmdir (only succeeds when empty) unwinds exactly what install created. Keep in
# sync with the copy layout in linux_install below. csgo and csgo/addons usually pre-exist -
# rmdir simply fails on them, which is fine.
# csgo/addons/metamod/_iconcache: created empty by the engine while the old layout's
# `Game csgo/addons/metamod` search path existed (real trees have one); rmdir'd when empty so
# uninstall can get back to the pre-install tree.
LN_DIRS=(csgo/addons/metamod/_iconcache csgo/addons/metamod/bin/linuxsteamrt64 csgo/addons/metamod/bin/linux64 csgo/addons/metamod/bin
         csgo/addons/metamod csgo/addons/cs2mcp/bin/linuxsteamrt64 csgo/addons/cs2mcp/bin
         csgo/addons/cs2mcp csgo/addons csgo)

# --- the busy-container guard ----------------------------------------------------------------
# The real game trees are shared with running containers (AGENTS.md rule 7 et al.): install may
# not rewrite files a live engine is reading. The guard applies to the real trees only - an
# override (CS2_GAME / CS2_LINUX_GAME) skips it unless GUARD_FAKE=1 (the host test).
linux_vmp() {  # the cs2-home volume's mountpoint (read-only podman call)
    "$PODMAN" volume inspect "$LINUX_VNAME" --format '{{.Mountpoint}}'
}

guard_needed() {  # rc 0: the guard must run; rc 1: skip (overridden test tree).
                  # The REAL-tree decision is made on realpath -m of both sides (item 10): a
                  # trailing slash, ".." segments or a symlink spelling of the real tree still
                  # gets the guard; only a genuinely different tree skips it.
    if [ "${GUARD_FAKE:-0}" = 1 ]; then return 0; fi
    case "$PLATFORM" in
        wine)  [ "$(realpath -m -- "$GAME")" = "$(realpath -m -- "$REAL_GAME")" ] ;;
        linux)
            if [ -z "$LINUX_GAME" ]; then return 0; fi
            local vmp
            vmp="$(linux_vmp)" || return 0   # cannot resolve the volume -> assume the guard must
                                             # run (its own podman failure then refuses loudly)
            [ "$(realpath -m -- "$LINUX_GAME")" = "$(realpath -m -- "$vmp/steamapps/common/Counter-Strike Global Offensive/game")" ] ;;
    esac
}

busy_probe() {  # prints one container name per line; rc>0 -> the caller REFUSES (never guessing
                # "nothing is running")
    local id name mj matched rc=0
    local -a ids=() busy_names=()
    local probe=""
    case "$PLATFORM" in
        wine) probe="$APP" ;;
        linux) probe="$(linux_vmp)" || return 3 ;;
    esac
    local ids_out extra=""
    ids_out="$("$PODMAN" ps -q)" || return 3
    if [ "$PLATFORM" = linux ]; then
        extra="$("$PODMAN" ps -q --filter "volume=$LINUX_VNAME")" || return 3
    fi
    if [ "$PLATFORM" = wine ]; then
        probe="$(realpath -m -- "$probe")"   # the mount probe compares realpath'd sources (item 10)
    fi
    # union (dedup) of both ids lists; a failing inspect also refuses
    local -a order=() tmp
    tmp="$(printf '%s %s' "$ids_out" "$extra")"
    local seen=" "
    for id in $tmp; do
        case "$seen" in *" $id "*) continue ;; esac
        seen="$seen$id "
        order+=("$id")
    done
    for id in "${order[@]:-}"; do
        [ -n "$id" ] || continue
        name="$("$PODMAN" inspect --format '{{.Name}}' "$id" 2>/dev/null)" || name=""
        if [ -z "$name" ]; then
            # a container that exited between `ps` and `inspect` is SKIPPED, not a refusal -
            # but an uninspectable container that is still listed stays a refusal (never guess)
            local now
            now="$("$PODMAN" ps -q)" || return 3
            case "$now" in
                *"$id"*) return 4 ;;
                *) say "  (container $id exited between ps and inspect - skipped)"; continue ;;
            esac
        fi
        mj="$("$PODMAN" inspect --format '{{json .Mounts}}' "$id")" || return 4
        matched="$(python3 -I - "$PLATFORM" "$probe" "$LINUX_VNAME" "$mj" <<'PY'
import json, os, sys
# argv (after the "-"): 1 platform, 2 probe (app dir on wine / volume mountpoint on linux),
# 3 volume name, 4 mounts json
pl = sys.argv[1]
try:
    mounts = json.loads(sys.argv[4])
except Exception:
    sys.exit(3)
def rel(a, b):  # same path, or a parent/child of it: a mount of the tree's parent also holds the
                # tree. A "/" bind matches NOTHING: "source /" is the whole host, not a marker
                # for this game tree (a "/"-matching rule would refuse on every container).
    if not a or not b or a == "/" or b == "/":
        return False
    return a == b or a.startswith(b + "/") or b.startswith(a + "/")
def rpath(p):  # symlink-resolved (item 10): the probe and the mount sources are compared
    p = os.path.realpath(p)
    return p if p != "/" else ""   # a "/" source/mountpoint matches nothing (see rel)
BUSY = False
if pl == "wine":
    # any bind whose source is the app tree (or a parent/child of it)
    probe = rpath(sys.argv[2])
    for m in mounts:
        t = m.get("Type") or m.get("MountType") or ""
        s = rpath(m.get("Source") or "")
        if s and t in ("bind", "") and rel(s, probe):
            BUSY = True
else:
    # any container using the volume cs2-home: a named-volume mount, or a bind whose source
    # is the volume's mountpoint (or a parent/child of it)
    vmp = rpath(sys.argv[2] or "")
    for m in mounts:
        t = m.get("Type") or m.get("MountType") or ""
        s = rpath(m.get("Source") or "")
        if t == "volume" and (m.get("Name") == sys.argv[3] or s == sys.argv[3]):
            BUSY = True
        elif t == "bind" and s and vmp and rel(s, vmp):
            BUSY = True
if BUSY:
    print("MATCH")
PY
)" || return 5
        if [ -n "$matched" ]; then
            busy_names+=("$name")
        fi
    done
    if [ "${#busy_names[@]}" -gt 0 ]; then
        printf '%s\n' "${busy_names[@]}"
    fi
    return 0
}

guard_live_game() {
    guard_needed || return 0
    say "checking for running containers mounting the tree (read-only podman)${GUARD_FAKE:+ [GUARD_FAKE]}"
    local busy rc=0
    busy="$(busy_probe)" || rc=$?
    if [ "$rc" != 0 ]; then
        die "podman failed while probing running containers (rc $rc); cannot see whether a container mounts the tree - refusing to touch the game tree (set CS2_GAME / CS2_LINUX_GAME for a test tree)"
    fi
    if [ -n "$busy" ]; then
        say "these running containers mount the tree:"
        while IFS= read -r n; do [ -n "$n" ] && say "  - $n"; done <<<"$busy"
        die4 "refusing to touch the game tree while the containers above mount it (stop them first; for tests use CS2_GAME / CS2_LINUX_GAME)"
    fi
}

# --- wine preflight (read-only; every refusal happens before anything is touched) -------------

check_manifest() {  # rc 0: entry present (idempotent re-run) or anchor found; rc 3: anchor missing
    python3 -I - "$MAN" <<'PY'
import sys
# newline="": read/write byte-exact (the real tree's files mix LF and CRLF - gameinfo.gi is CRLF)
t = open(sys.argv[1], encoding="utf-8", newline="").read()
if 'm_Name = "cs2_bridge"' not in t:
    eol = "\r\n" if "\r\n" in t else "\n"
    anchor = "\t\t{" + eol + '\t\t\tm_Name = "hammer"'
    if anchor not in t:
        sys.exit(3)
sys.exit(0)
PY
}

preflight_artefact() {  # preflight_artefact <build-path> <installed-app-relative-dest> <what>: the
                        # build artefact is required only when it must be (re)copied: an
                        # installed copy + no build = current (ensure still repairs the config)
    if [ -f "$1" ]; then return 0; fi
    if [ -f "$APP/$2" ]; then
        say "no built $3 at $1 - the INSTALLED copy counts as current (only config is repaired)"
    else
        die "no $3 at $1 and none installed at $APP/$2 - BUILD IT FIRST (run bridge/build-rs.sh)"
    fi
}

preflight_wine() {
    say_build_checkout
    # the build artefact is required only when the DLL must be (re)copied (item 5): an installed
    # tool DLL + no build = current, and ensure still repairs the config
    if [ -f "$RS_LIB" ]; then
        say "export gate on $RS_LIB:"
        "$HERE/check-exports.sh" "$RS_LIB" || die "check-exports.sh refused $RS_LIB (the engine calls a tool DLL's exports; refusing)"
    elif [ -f "$APP/game/bin/win64/tools/cs2_bridge.dll" ]; then
        say "no built DLL at $RS_LIB - the INSTALLED tool DLL counts as current (only config is repaired)"
    else
        die "no Rust DLL at $RS_LIB and none installed at $APP/game/bin/win64/tools/cs2_bridge.dll - BUILD IT FIRST (bridge/build-rs.sh, or point CS2_BRIDGE_DLL at one)"
    fi
    # the launcher artefacts (NOT export-gated - header "install (wine)")
    preflight_artefact "$LAUNCH_EXE" game/bin/win64/cs2mcp_launch.exe "launcher exe"
    preflight_artefact "$LOAD_DLL" game/bin/win64/cs2mcp_mmload.dll "mmload DLL"
    # the copies land in (possibly new) dirs, but the file we EDIT (and keep as backup) must exist
    [ -f "$APP/${EDIT_DESTS[0]}" ] || die "missing $APP/${EDIT_DESTS[0]} (game tree incomplete: install only EDITS existing files)"
    check_manifest || die "$MAN: no 'hammer' entry found to insert the cs2_bridge entry before (unexpected manifest layout)"
    local f
    for f in "$MM_BUILD/metamod/bin/win64/server.dll" "$MM_BUILD/metamod/bin/win64/metamod.2.cs2.dll" \
             "$MM_BUILD/metamod/metaplugins.ini"; do
        [ -f "$f" ] || die "missing metamod build file: $f (build metamod per bridge/metamod/README.md, or point CS2_MM at the build)"
    done
}

# the vdf bytes each install writes, shared by the writers and by ensure
wine_vdf_text() {  # relative to game/csgo -> the tools copy: ONE image (header comment)
    cat <<'VDF'
"Metamod Plugin"
{
	"alias"		"cs2mcp"
	"file"		"../bin/win64/tools/cs2_bridge.dll"
}
VDF
}

linux_vdf_text() {  # relative to mod dir game/csgo -> the addons copy (linux has no tools manifest)
    # byte-form per the spike's live-verified linux-mm.sh install (single tabs)
    cat <<'VDF'
"Metamod Plugin"
{
	"alias"	"cs2mcp"
	"file"	"addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so"
}
VDF
}

# BLOCKER fix (round 2): ONE function produces the linux vdf bytes - install writes exactly
# this file and ensure cmps the installed bytes against it (FULL bytes incl. the trailing
# newline; never a $(...) string, which strips it). Before, install's printf and ensure's
# command-substituted text drifted, so a linux install never read "current".
VDF_TMP=""
linux_vdf_file() {  # sets VDF_TMP: a registered temp file holding exactly linux_vdf_text's bytes
    mktemp_under_target install-rs-vdf
    VDF_TMP="$TMP_PATH"
    linux_vdf_text > "$VDF_TMP"
    TEMPS+=("$VDF_TMP")
}

# --- wine install helpers ----------------------------------------------------------------------

backup() {  # backup <bk-dir>: app-dir-relative copies of every dest that already exists.
            # ATOMIC: everything is built into <bk-dir>.partial and the finished backup is
            # renamed into place - a killed install never leaves a half-written backup usable
            # (the trap removes the partial). Also records EDITS_WROTE (whether our manifest
            # edit was ABSENT before this install - uninstall removes it only then; the
            # gameinfo-era kinds are never written any more, see the header)
            # and, at the end of install, INSTALLED (the files install CREATED - see below).
    local bk="$1" rel
    PARTIAL_BK="$bk.partial"
    mkdir -p "$PARTIAL_BK"
    printf '%s\n' "$APP" > "$PARTIAL_BK/APP_TREE"   # which tree this image belongs to (backups_for_app)
    local all=("${TOOL_DESTS[@]}" "${EDIT_DESTS[@]}" "${MM_DESTS[@]}" "${LAUNCH_DESTS[@]}")
    local out=()
    for rel in "${all[@]}"; do
        [ -e "$APP/$rel" ] || continue
        mkdir -p "$PARTIAL_BK/$(dirname "$rel")"
        cp -p "$APP/$rel" "$PARTIAL_BK/$rel"
        out+=("$rel")
    done
    : > "$PARTIAL_BK/SHA256SUMS"
    if [ "${#out[@]}" -gt 0 ]; then
        local f
        for f in "${out[@]}"; do (cd "$PARTIAL_BK" && sha256sum "$f"); done >> "$PARTIAL_BK/SHA256SUMS"
        for rel in "${out[@]}"; do say "  backed up: $rel"; done
    else
        say "  (nothing pre-existed; the backup still records EDITS_WROTE/INSTALLED)"
    fi
    : > "$PARTIAL_BK/EDITS_WROTE"
    if [ -f "$MAN" ] && ! grep -q 'm_Name = "cs2_bridge"' "$MAN"; then
        printf 'manifest:cs2_bridge\n' >> "$PARTIAL_BK/EDITS_WROTE"
    fi
    for rel in ${CANCEL_KINDS[@]+"${CANCEL_KINDS[@]}"}; do printf 'reverted:%s\n' "$rel" >> "$PARTIAL_BK/EDITS_WROTE"; done
    # ANCHORS (r3 item 6): a fingerprint of the Valve-owned line install relies on - the
    # manifest's pre-existing anchor entry (the hammer entry), recorded as stripped text.
    # ensure re-checks it: a file truncated AFTER our entry (our entry present, the anchor gone)
    # must read "game file damaged", not current.
    : > "$PARTIAL_BK/ANCHORS"
    rec_anchor() {  # rec_anchor <file> <kind> <ERE for a whole line>: record the line's stripped text
        local a
        a="$(grep -Em1 -- "$3" "$1" 2>/dev/null | sed 's/^[[:space:]]*//;s/[[:space:]]*$//')" || true
        [ -n "$a" ] && printf '%s\t%s\n' "$2" "$a" >> "$PARTIAL_BK/ANCHORS"
        return 0
    }
    if [ -f "$MAN" ]; then
        rec_anchor "$MAN" manifest 'm_Name[[:space:]]*=[[:space:]]*"hammer"'
    fi
    # PLANNED INSTALLED (r3 item 3): the dests ABSENT at backup time are the ones this install
    # would CREATE. The planned list goes into the .partial BEFORE it is finalised, so a kill
    # between the backup and the ACTUAL list (write_installed, end of install) still leaves an
    # uninstallable tree. (EDITS_WROTE above is already the planned AND actual edit list - the
    # edits are deterministic.) A completed install rewrites INSTALLED as the actual list.
    : > "$PARTIAL_BK/INSTALLED"
    for rel in "${TOOL_DESTS[@]}" "${MM_DESTS[@]}" "${LAUNCH_DESTS[@]}"; do
        [ -e "$APP/$rel" ] && continue
        case "$rel" in
            */metaplugins.ini) printf '%s %s\n' "$rel" "$(sha_of "$MM_BUILD/metamod/metaplugins.ini")" >> "$PARTIAL_BK/INSTALLED" ;;
            *) printf '%s\n' "$rel" >> "$PARTIAL_BK/INSTALLED" ;;
        esac
    done
    # PLANNED WROTE_OVER (r4 item 1): the pre-existing paths install would OVERWRITE, with the
    # sha256 of the bytes WE would write. uninstall restores a path only when it is in
    # WROTE_OVER AND its current sha still equals what we wrote - files that existed but that
    # we did NOT write (metaplugins.ini, a foreign file) are never restored. The EDITED
    # manifest is NOT here (uninstall reverts its edit from the CURRENT file, EDITS_WROTE);
    # metaplugins.ini is never here (create-if-absent only). Planned before the finalise (r3
    # item 3 parity: a killed install still uninstalls); a successful install rewrites the
    # ACTUAL list (write_wrote_over).
    : > "$PARTIAL_BK/WROTE_OVER"
    wro_plan() {  # wro_plan <app-relative-dest> <sha-of-bytes-we-would-write>
        if [ -e "$APP/$1" ]; then printf '%s %s\n' "$1" "$2" >> "$PARTIAL_BK/WROTE_OVER"; fi
        return 0
    }
    if [ -f "$RS_LIB" ]; then   # no build artefact -> the installed copy stays (never written)
        wro_plan game/bin/win64/tools/cs2_bridge.dll "$(sha_of "$RS_LIB")"
    fi
    if [ -f "$LAUNCH_EXE" ]; then
        wro_plan game/bin/win64/cs2mcp_launch.exe "$(sha_of "$LAUNCH_EXE")"
    fi
    if [ -f "$LOAD_DLL" ]; then
        wro_plan game/bin/win64/cs2mcp_mmload.dll "$(sha_of "$LOAD_DLL")"
    fi
    wro_plan game/csgo/addons/metamod/bin/win64/server.dll        "$(sha_of "$MM_BUILD/metamod/bin/win64/server.dll")"
    wro_plan game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll "$(sha_of "$MM_BUILD/metamod/bin/win64/metamod.2.cs2.dll")"
    wro_plan game/csgo/addons/metamod/cs2mcp.vdf                  "$(wine_vdf_text | sha256sum | awk '{print $1}')"
    fault mid-backup
    mv -f "$PARTIAL_BK" "$bk" || die "cannot finalise the backup $bk"
    PARTIAL_BK=""
}

write_installed() {  # write_installed <bk-dir>: the CREATED files of this install (atomically).
                     # metaplugins.ini records its sha256: uninstall deletes it only when it is
                     # still byte-identical to what we created (item 6).
    local bk="$1" rel t sha
    tmp_for "$bk/INSTALLED"
    t="$TMP_PATH"
    : > "$t"
    for rel in ${CREATED_LIST[@]+"${CREATED_LIST[@]}"}; do
        case "$rel" in
            *" "*) printf '%s\n' "$rel" >> "$t" ;;   # linux lines already carry their sha256
            game/csgo/addons/metamod/metaplugins.ini)
                if [ -f "$APP/$rel" ]; then
                    printf '%s %s\n' "$rel" "$(sha_of "$APP/$rel")" >> "$t"
                else
                    printf '%s\n' "$rel" >> "$t"
                fi ;;
            *) printf '%s\n' "$rel" >> "$t" ;;
        esac
    done
    mv -f "$t" "$bk/INSTALLED"
}

# WROTE_OVER: files this install OVERWROTE (dest pre-existed, our bytes written), as
# "<rel> <sha256-of-what-we-wrote>" lines. Planned into the .partial by backup(); rewritten as
# the ACTUAL list here after a successful install. uninstall restores a path only when it is
# in WROTE_OVER AND its current sha still equals what we wrote (r4 item 1).
WROTE_OVER_LIST=()   # entries "rel sha"; filled by copy_in / write_vdf / the linux install
write_wrote_over() {  # write_wrote_over <bk-dir>: the ACTUAL WROTE_OVER list, atomically
    local bk="$1" rel t
    tmp_for "$bk/WROTE_OVER"
    t="$TMP_PATH"
    : > "$t"
    for rel in ${WROTE_OVER_LIST[@]+"${WROTE_OVER_LIST[@]}"}; do
        printf '%s\n' "$rel" >> "$t"
    done
    mv -f "$t" "$bk/WROTE_OVER"
}

CREATED_LIST=()   # files this install CREATED (dest did not pre-exist); written to the backup's INSTALLED

copy_in() {  # copy_in <src> <app-relative-dest>: atomic copy; the dest path prints either way
    local src="$1" rel="$2"  # no self-references in the same local list (bash expands all words first)
    local dest="$APP/$rel" t created=0
    if [ "$DRY" = 1 ]; then say "DRY: cp $src -> $rel"; return 0; fi
    mkdir -p "$(dirname "$dest")"
    tmp_for "$dest"
    t="$TMP_PATH"
    if [ -e "$dest" ]; then cat "$src" > "$t"; else cp -p "$src" "$t"; created=1; fi
    commit_tmp "$t" "$dest" "$rel"
    if [ "$created" = 1 ]; then
        CREATED_LIST+=("$rel")
    else
        WROTE_OVER_LIST+=("$rel $(sha_of "$src")")   # pre-existing + our bytes written (r4 item 1)
    fi
    say "  installed: $rel"
}

apply_manifest_edit() {  # apply_manifest_edit <in> <out>: THE manifest edit, one producer -
                         # install writes it into a temp it renames over the file, ensure runs
                         # it on a copy and cmps (item 11: "current" = the file is what this
                         # would produce). rc 3 when the hammer anchor is missing.
    python3 -I - "$1" "$2" <<'PY'
import sys
# newline="": byte-exact read/write; the entry text uses the file's own line ending
p, outp = sys.argv[1], sys.argv[2]
t = open(p, encoding="utf-8", newline="").read()
if 'm_Name = "cs2_bridge"' not in t:
    eol = "\r\n" if "\r\n" in t else "\n"
    anchor = '\t\t{' + eol + '\t\t\tm_Name = "hammer"'
    if anchor not in t:
        sys.exit(3)
    entry = ('\t\t{' + eol + '\t\t\tm_Name = "cs2_bridge"' + eol
             + '\t\t\tm_FriendlyName = "CS2 Bridge (Rust)"' + eol
             + '\t\t\tm_Library = "tools/cs2_bridge.dll"' + eol
             + '\t\t\tm_bShowInToolsMenu = false' + eol + '\t\t},' + eol)
    t = t.replace(anchor, entry + anchor, 1)
open(outp, "w", encoding="utf-8", newline="").write(t)
PY
}

edit_manifest() {  # insert the cs2_bridge entry before the hammer entry (exact WP0 text, idempotent)
    if [ "$DRY" = 1 ]; then say "DRY: game/bin/sdkenginetools.txt - insert m_Name = \"cs2_bridge\" entry before the hammer entry"; return 0; fi
    local t
    tmp_for "$MAN"
    t="$TMP_PATH"
    apply_manifest_edit "$MAN" "$t" || { rm -f "$t"; die "$MAN: cannot insert the cs2_bridge entry (no 'hammer' anchor?)"; }
    commit_tmp "$t" "$MAN" game/bin/sdkenginetools.txt
    say "  edited: game/bin/sdkenginetools.txt"
}

write_vdf() {  # Metamod 2.0 plugin registration; the format + resolution rules are cited in the header
    local rel=game/csgo/addons/metamod/cs2mcp.vdf t created=1
    if [ "$DRY" = 1 ]; then say "DRY: $rel - vdf \"Metamod Plugin\" { alias, file ../bin/win64/tools/cs2_bridge.dll }"; return 0; fi
    mkdir -p "$APP/$(dirname "$rel")"
    [ -e "$APP/$rel" ] && created=0   # decided BEFORE the write: create or overwrite?
    tmp_for "$APP/$rel"
    t="$TMP_PATH"
    wine_vdf_text > "$t"   # the ONE producer of the wine vdf bytes (ensure compares against it)
    commit_tmp "$t" "$APP/$rel" "$rel"
    if [ "$created" = 1 ]; then
        CREATED_LIST+=("$rel")
    else
        WROTE_OVER_LIST+=("$rel $(wine_vdf_text | sha256sum | awk '{print $1}')")   # r4 item 1
    fi
    say "  created: $rel"
}

oldest_backup() {  # the pre-FIRST-install image; UTC stamps are lexicographically ordered
    # WHY OLDEST, not newest: the brief says "newest", which is right only for the wp0 flow
    # (one install, then restore). install-rs.sh is idempotent, so a second install (the manifest
    # entry skip path makes it a no-op) would otherwise snapshot the already-installed tree, and
    # "uninstall removes everything it added" would then keep install #1's entries forever.
    # The oldest backup is by definition the tree before the first install; re-running uninstall
    # after it restores from that same image (deterministic), always byte-exact.
    # Fault-tolerant: a missing BACKUP_ROOT yields EMPTY (so cmd_uninstall can refuse with a
    # message naming the path) rather than a pipefail kill - the caller prints the error here.
    local all; all="$(backups_for_app)"   # no `| head`: under pipefail a closed pipe kills the producer (SIGPIPE, 141)
    [ -n "$all" ] && printf '%s\n' "${all%%$'\n'*}"
    return 0
}

backup_app_key() {  # the key the backing APP_TREE records, and what backups_for_app matches
    if [ "$PLATFORM" = wine ]; then printf '%s' "$APP"; else printf '%s' "$LINUX_G"; fi
}

backups_for_app() {  # this tree's live backups, oldest first; a backup of another tree (a fake test tree) never matches.
                     # *.partial dirs are skipped: an unfinished backup is NOT a usable backup (it is trap-cleaned).
    local b
    while IFS= read -r b; do
        case "$b" in *.partial) continue ;; esac
        [ -n "$b" ] && [ "$(cat "$b/APP_TREE" 2>/dev/null)" = "$(backup_app_key)" ] && printf '%s\n' "$b"
    done < <({ find "$BACKUP_ROOT" -maxdepth 1 -name "$BK_PREFIX-*" 2>/dev/null || true; } | sort)
    return 0
}

verify_backup_sha() {  # verify_backup_sha <bk-dir>: the backup's SHA256SUMS must be present and
                       # intact before any of its lists/copies are used (r5 NIT: EVERY backup
                       # that contributes a WROTE_OVER/INSTALLED entry, not only the oldest)
    if [ ! -f "$1/SHA256SUMS" ]; then
        die "no SHA256SUMS in $1 - cannot trust this backup, refusing"
    fi
    # an EMPTY list is valid (a gameinfo-free install on a tree where nothing of ours pre-existed
    # backs up no file); `sha256sum -c` refuses an empty list, so there is nothing to check
    [ -s "$1/SHA256SUMS" ] || return 0
    (cd "$1" && sha256sum -c --quiet SHA256SUMS) || die "backup SHA256SUMS mismatch in $1 (it was modified since install; refusing)"
}

# --- the records of this tree's backups ----------------------------------------------------------
# ONE reader for everything that needs to know what an earlier install of ours did: uninstall
# (both platforms) and the migration of an old-layout tree (below) + ensure's read-only
# old-layout check. Filled by records_load:
#   REC_BKS    this tree's live backups, oldest first
#   REC_INST   every INSTALLED line ("rel" or "rel sha") of every backup (files install CREATED)
#   REC_PRE    the files the OLDEST backup's SHA256SUMS lists (the pre-first-install image)
#   REC_WRO    WROTE_OVER triplets "rel" "sha-of-what-we-wrote" "backup-dir", oldest backup first
#   REC_KINDS  the UNION of every backup's EDITS_WROTE lines (a line ANY install of ours wrote
#              is ours: the oldest backup alone would miss a line a later repair re-wrote)
#              A `reverted:<kind>` line (written by a gameinfo-free backup after a migration, see
#              reverted_kinds) CANCELS <kind>: that line is gone from the tree, so a same-text line
#              that shows up later is not ours and uninstall/migration must not remove it.
REC_BKS=() REC_INST=() REC_PRE=() REC_WRO=() REC_KINDS=()
records_load() {  # records_load [verify]: with `verify` every backup that contributes a list is
                  # SHA256SUMS-checked (dies on a mismatch) and a missing list is WARNed about
                  # (an old-format backup); without it the read is silent and never dies
                  # (ensure's read-only check)
    local verify="${1:-}" bb ln rel sha k first=1 sline
    local -a all_kinds=() cancelled=()
    REC_BKS=() REC_INST=() REC_PRE=() REC_WRO=() REC_KINDS=()
    while IFS= read -r bb; do [ -n "$bb" ] && REC_BKS+=("$bb"); done < <(backups_for_app)
    for bb in ${REC_BKS[@]+"${REC_BKS[@]}"}; do
        if [ -f "$bb/INSTALLED" ]; then
            [ "$verify" = verify ] && verify_backup_sha "$bb"   # a contributing backup is verified first
            while IFS= read -r ln; do [ -n "$ln" ] && REC_INST+=("$ln"); done < "$bb/INSTALLED"
        elif [ "$verify" = verify ]; then
            say "  WARN: no INSTALLED list in $bb (an old-format backup) - what IT created stays"
        fi
        if [ -f "$bb/WROTE_OVER" ]; then
            [ "$verify" = verify ] && verify_backup_sha "$bb"
            while IFS= read -r ln; do
                [ -n "$ln" ] || continue
                rel="${ln%% *}"
                sha="${ln#"$rel" }"
                REC_WRO+=("$rel" "$sha" "$bb")
            done < "$bb/WROTE_OVER"
        elif [ "$verify" = verify ]; then
            say "  WARN: no WROTE_OVER list in $bb (an old-format backup) - what IT overwrote cannot be restored"
        fi
        if [ -f "$bb/EDITS_WROTE" ]; then
            while IFS= read -r k; do
                [ -n "$k" ] || continue
                case "$k" in reverted:*) cancelled+=("${k#reverted:}") ;; *) all_kinds+=("$k") ;; esac
            done < "$bb/EDITS_WROTE"
        fi
    done
    local c skip
    for k in ${all_kinds[@]+"${all_kinds[@]}"}; do
        skip=0
        for c in ${cancelled[@]+"${cancelled[@]}"}; do [ "$c" = "$k" ] && skip=1; done
        [ "$skip" = 1 ] && continue
        rec_has_kind "$k" || REC_KINDS+=("$k")
    done
    if [ "${#REC_BKS[@]}" -gt 0 ] && [ -f "${REC_BKS[0]}/SHA256SUMS" ]; then
        while IFS= read -r sline; do
            [ -n "$sline" ] || continue
            REC_PRE+=("${sline#*  }")   # sha256sum's "hash  path"
        done < "${REC_BKS[0]}/SHA256SUMS"
    fi
    return 0
}
records_verify() {  # the verifying pass over an already-loaded set (migration: only when it acts)
    records_load verify
}

rec_pre_first() {  # rc 0: <rel> pre-existed the FIRST install (the oldest backup lists it)
    local pf
    for pf in ${REC_PRE[@]+"${REC_PRE[@]}"}; do [ "$pf" = "$1" ] && return 0; done
    return 1
}
rec_created() {  # rc 0: install CREATED <rel> (an INSTALLED entry) and it did not pre-exist the
                 # first install (r4 item 3: an entry the oldest backup lists is the owner's)
    local ln
    rec_pre_first "$1" && return 1
    for ln in ${REC_INST[@]+"${REC_INST[@]}"}; do [ "${ln%% *}" = "$1" ] && return 0; done
    return 1
}
rec_has_kind() {  # rc 0: some install of ours recorded the edit <kind> in its EDITS_WROTE
    local k
    for k in ${REC_KINDS[@]+"${REC_KINDS[@]}"}; do [ "$k" = "$1" ] && return 0; done
    return 1
}
rec_wrote() {  # rc 0: some install of ours overwrote <rel> (a WROTE_OVER entry)
    local j=0
    while [ "$j" -lt "${#REC_WRO[@]}" ]; do
        [ "${REC_WRO[$j]}" = "$1" ] && return 0
        j=$((j+3))
    done
    return 1
}

CHAIN_SRC=""
chain_src_for() {  # chain_src_for <rel> <current-sha>: sets CHAIN_SRC to the backup copy to
                   # restore <rel> from (r5 item S1, the chain rule). Start from the NEWEST
                   # WROTE_OVER entry whose sha equals the CURRENT sha (a path written more than
                   # once - a rebuild between installs makes ensure reinstall it - has the OLDEST
                   # entry describing our FIRST write, not our last); take that backup's copy;
                   # while that copy is itself what the next-OLDER entry's install wrote, step
                   # to the older backup's copy. The last copy reached is the bytes that existed
                   # before we FIRST wrote the path. rc 0 found, 1 no entry's sha equals the
                   # current sha (changed after our last write), 2 the backup copy is missing.
    local rel="$1" cur="$2" mi=-3 j=0 older copy_sha
    CHAIN_SRC=""
    while [ "$j" -lt "${#REC_WRO[@]}" ]; do
        if [ "${REC_WRO[$j]}" = "$rel" ] && [ "${REC_WRO[$((j+1))]}" = "$cur" ]; then mi=$j; fi
        j=$((j+3))
    done
    [ "$mi" != -3 ] || return 1
    while [ -f "${REC_WRO[$((mi+2))]}/$rel" ]; do
        copy_sha="$(sha_of "${REC_WRO[$((mi+2))]}/$rel")"
        older=-3
        j=$mi
        while [ "$j" -ge 3 ]; do
            j=$((j-3))
            if [ "${REC_WRO[$j]}" = "$rel" ]; then older=$j; break; fi
        done
        [ "$older" -ge 0 ] || break
        if [ "$copy_sha" = "${REC_WRO[$((older+1))]}" ]; then mi=$older; else break; fi
    done
    [ -f "${REC_WRO[$((mi+2))]}/$rel" ] || return 2
    CHAIN_SRC="${REC_WRO[$((mi+2))]}/$rel"
    return 0
}

OLD_CLASS=""
old_classify() {  # old_classify <rel> <current-sha-or-"-"for-a-non-regular-file>: sets OLD_CLASS
                  # for a file found at an OLD-layout location (the attribution rule, header):
                  #   delete   an install of ours CREATED it (INSTALLED, not a pre-first-install file)
                  #   restore  an install of ours OVERWROTE it and it still carries OUR bytes
                  #            (a WROTE_OVER sha equals the current one): the owner's come back
                  #   leave    it was ours-to-overwrite but the bytes are no longer ours (the owner
                  #            changed it, or a previous migration already restored it), or it
                  #            pre-existed and was never overwritten: the owner's, not touched,
                  #            not a marker
                  #   unknown  no backup of this tree mentions it: cannot be attributed -> refuse
    local rel="$1" cur="$2"
    if rec_created "$rel"; then OLD_CLASS=delete
    elif rec_wrote "$rel"; then
        local crc=0
        if [ "$cur" = "-" ]; then OLD_CLASS=leave
        else
            chain_src_for "$rel" "$cur" || crc=$?
            case "$crc" in
                0) OLD_CLASS=restore ;;
                1) OLD_CLASS=leave ;;
                *) OLD_CLASS=unknown ;;   # the backup copy to restore from is gone: cannot restore
            esac
        fi
    elif rec_pre_first "$rel"; then OLD_CLASS=leave
    else OLD_CLASS=unknown
    fi
}

# --- reverted kinds: what a gameinfo-free backup records after a migration ----------------------
# The gameinfo-era backups still say "an install of ours wrote `Game metamod`" in EDITS_WROTE for
# as long as they exist. Once the migration has taken the line out, a line with the same text that
# appears LATER is somebody else's and must survive uninstall: every gameinfo-free backup records
# `reverted:<kind>` for each recorded gameinfo kind whose line is gone from the tree (computed
# after the migration, so a kill between the revert and the backup is healed by the next run).
CANCEL_KINDS=()
reverted_kinds() {  # fills CANCEL_KINDS from the CURRENT tree + records (after the migration)
    CANCEL_KINDS=()
    local k m present
    records_load
    if [ "$PLATFORM" = wine ]; then wine_old_scan; else linux_old_scan; fi
    for k in ${REC_KINDS[@]+"${REC_KINDS[@]}"}; do
        case "$k" in gameinfo:*) ;; *) continue ;; esac
        present=0
        for m in ${MIG_GI_KINDS[@]+"${MIG_GI_KINDS[@]}"}; do [ "$m" = "$k" ] && present=1; done
        [ "$present" = 1 ] || CANCEL_KINDS+=("$k")
    done
    return 0
}

# --- the old (gameinfo-era) layout: detection + migration (wine) ---------------------------------
# What an OLD install of this script left in a wine tree: game/metamod/ + content/metamod/ and
# our two gameinfo lines. install (and ensure's repair, which runs install) removes exactly what
# the backups of THIS tree record as ours - see old_classify for the file rule; a gameinfo line
# only when some backup's EDITS_WROTE recorded it - and REFUSES, before anything is touched,
# when an old artefact cannot be attributed (header "migration").
MIG_DELETE=() MIG_RESTORE=() MIG_UNKNOWN=() MIG_GI_KINDS=() MIG_GI_FOREIGN=()
mig_reset() { MIG_DELETE=() MIG_RESTORE=() MIG_UNKNOWN=() MIG_GI_KINDS=() MIG_GI_FOREIGN=(); }
mig_note_file() {  # mig_note_file <rel> <sha-or-"-">: classify one file found at an old location
    old_classify "$1" "$2"
    case "$OLD_CLASS" in
        delete)  MIG_DELETE+=("$1") ;;
        restore) MIG_RESTORE+=("$1") ;;
        unknown) MIG_UNKNOWN+=("$1") ;;
    esac
}
mig_note_gameinfo() {  # mig_note_gameinfo <distinctive-line-kind> <has-it 0|1> <has-write-csgo 0|1>
    # the distinctive `Game ...metamod` line is a search path that would put a SECOND Metamod
    # (or the game DLL redirect) in play: ours (recorded) -> reverted; not ours -> refuse. A
    # `Write csgo` line is only ours when recorded; an unrecorded one is the owner's/Valve's
    # and stays, silently (it breaks nothing).
    if [ "$2" = 1 ]; then
        if rec_has_kind "$1"; then MIG_GI_KINDS+=("$1"); else MIG_GI_FOREIGN+=("${1#gameinfo:}"); fi
    fi
    if [ "$3" = 1 ] && rec_has_kind "gameinfo:Write csgo"; then MIG_GI_KINDS+=("gameinfo:Write csgo"); fi
}
wine_old_scan() {  # read-only: fills MIG_* from the CURRENT tree + the loaded records
    mig_reset
    local root p rel sha hg=0 hw=0
    for root in game/metamod content/metamod; do
        [ -e "$APP/$root" ] || [ -L "$APP/$root" ] || continue
        while IFS= read -r -d '' p; do
            rel="${p#"$APP"/}"
            if [ -f "$p" ] && [ ! -L "$p" ]; then sha="$(sha_of "$p")"; else sha="-"; fi
            mig_note_file "$rel" "$sha"
        done < <(find "$APP/$root" ! -type d -print0 | sort -z)
    done
    if [ -f "$GI" ]; then
        grep -Eq $'^[[:space:]]*Game[[:space:]]+metamod[[:space:]]*$' "$GI" && hg=1
        grep -Eq $'^[[:space:]]*Write[[:space:]]+csgo[[:space:]]*$' "$GI" && hw=1
        mig_note_gameinfo "gameinfo:Game metamod" "$hg" "$hw"
    fi
    return 0
}
mig_count() { printf '%s' $(( ${#MIG_DELETE[@]} + ${#MIG_RESTORE[@]} + ${#MIG_UNKNOWN[@]} + ${#MIG_GI_KINDS[@]} + ${#MIG_GI_FOREIGN[@]} )); }
mig_refuse_if_foreign() {  # the no-guessing refusal; touches nothing
    local x
    [ "$(( ${#MIG_UNKNOWN[@]} + ${#MIG_GI_FOREIGN[@]} ))" -gt 0 ] || return 0
    say "this tree carries OLD-layout artefacts that no install of ours recorded:"
    for x in ${MIG_UNKNOWN[@]+"${MIG_UNKNOWN[@]}"}; do
        say "  - $x (no backup of this tree under $BACKUP_ROOT lists it as created or overwritten by us)"
    done
    for x in ${MIG_GI_FOREIGN[@]+"${MIG_GI_FOREIGN[@]}"}; do
        say "  - ${GI_LABEL:-gameinfo.gi}: a \`$x\` line that no install of ours wrote"
    done
    die "refusing to migrate: cannot attribute the artefacts above to this script's installs (no guessing, nothing was touched). Remove them by hand (or uninstall the tool that put them there), then run install/ensure again"
}

migrate_wine() {
    migrate_wine_do
    if [ "$DRY" != 1 ]; then reverted_kinds; fi
}
migrate_wine_do() {
    records_load
    wine_old_scan
    mig_refuse_if_foreign
    local nf=$(( ${#MIG_DELETE[@]} + ${#MIG_RESTORE[@]} )) ng=${#MIG_GI_KINDS[@]}
    [ $((nf + ng)) -gt 0 ] || return 0
    say "migration: the old (gameinfo-era) layout is on this tree - removing what the backups record as ours"
    local rel
    if [ "$DRY" = 1 ]; then
        for rel in ${MIG_DELETE[@]+"${MIG_DELETE[@]}"}; do say "DRY: migrate: remove $rel (created by an old install of ours)"; done
        for rel in ${MIG_RESTORE[@]+"${MIG_RESTORE[@]}"}; do say "DRY: migrate: restore $rel (the owner's bytes; the old install overwrote it)"; done
        for rel in ${MIG_GI_KINDS[@]+"${MIG_GI_KINDS[@]}"}; do say "DRY: migrate: revert game/csgo/gameinfo.gi line ${rel#gameinfo:}"; done
        return 0
    fi
    # the records drive deletions + restores: every backup that contributes is SHA256SUMS-verified
    # first (a mismatch dies here, nothing touched), then the plan is recomputed from them
    records_verify
    wine_old_scan
    mig_refuse_if_foreign
    sweep_dest_temps "$APP" game/csgo/gameinfo.gi ${MIG_DELETE[@]+"${MIG_DELETE[@]}"} ${MIG_RESTORE[@]+"${MIG_RESTORE[@]}"}
    local rt cur
    for rel in ${MIG_RESTORE[@]+"${MIG_RESTORE[@]}"}; do
        cur="$(sha_of "$APP/$rel")"
        if ! chain_src_for "$rel" "$cur"; then
            say "  WARN: not restored: $rel (the backup copy is missing)"
            continue
        fi
        rt="$APP/$rel.tmp.$$"
        cp -p "$CHAIN_SRC" "$rt"
        TEMPS+=("$rt")
        commit_tmp "$rt" "$APP/$rel" "migrate:$rel"
        if cmp -s "$CHAIN_SRC" "$APP/$rel"; then
            say "  restored (the old install overwrote it): $rel"
        else
            say "  WARN: restore of $rel differs from the backup copy"
        fi
    done
    for rel in ${MIG_DELETE[@]+"${MIG_DELETE[@]}"}; do
        rm -f "$APP/$rel"
        say "  removed (old layout, created by an install of ours): $rel"
    done
    local d
    if [ "$nf" -gt 0 ]; then
        for d in "${OLD_MM_DIRS[@]}"; do
            if [ -d "$APP/$d" ] && rmdir "$APP/$d" 2>/dev/null; then say "  removed empty dir: $d"; fi
        done
    fi
    # the gameinfo revert runs LAST, after the file work succeeded (as uninstall does)
    if [ "$ng" -gt 0 ]; then
        local grc=0
        remove_gameinfo_lines migrate "$GI" "${MIG_GI_KINDS[@]}" || grc=$?
        case "$grc" in
            0) say "  reverted: game/csgo/gameinfo.gi (our old lines removed, your other changes kept)" ;;
            1) say "  (gameinfo: our old lines already absent)" ;;
            *) die "removing our old gameinfo lines failed (rc $grc) - refusing to continue (verify the game files)" ;;
        esac
    fi
    say "migration done: the tree no longer carries the old layout"
}

# --- wine commands -----------------------------------------------------------------------------

cmd_install_wine() {
    guard_live_game
    preflight_wine
    migrate_wine   # refuses (before anything is touched) or migrates the old layout; --dry-run only reports
    CREATED_LIST=()
    WROTE_OVER_LIST=()
    if [ "$DRY" = 1 ]; then
        say "dry-run: nothing below changes anything"
        say "  would back up first under: $BACKUP_ROOT/$BK_PREFIX-<UTC>"
        local all=("${TOOL_DESTS[@]}" "${EDIT_DESTS[@]}" "${MM_DESTS[@]}" "${LAUNCH_DESTS[@]}")
        local rel
        for rel in "${all[@]}"; do
            if [ -e "$APP/$rel" ]; then say "dry-run: would back up $rel before overwriting"; fi
        done
    fi
    local bk
    bk="$BACKUP_ROOT/$BK_PREFIX-$(date -u '+%Y%m%dT%H%M%S.%NZ')"   # ns: two cycles in one second must not collide
    if [ "$DRY" != 1 ]; then
        sweep_dest_temps "$APP" "${TOOL_DESTS[@]}" "${EDIT_DESTS[@]}" "${MM_DESTS[@]}" "${LAUNCH_DESTS[@]}" "${LEGACY_MM_DESTS[@]}"
        backup "$bk"
    fi
    if [ -f "$RS_LIB" ]; then
        copy_in "$RS_LIB" game/bin/win64/tools/cs2_bridge.dll
    else
        say "  kept: game/bin/win64/tools/cs2_bridge.dll (no built DLL; the installed copy stays)"
    fi
    edit_manifest
    # the launcher artefacts (preflight guaranteed: built, or already installed)
    if [ -f "$LAUNCH_EXE" ]; then copy_in "$LAUNCH_EXE" game/bin/win64/cs2mcp_launch.exe
    else say "  kept: game/bin/win64/cs2mcp_launch.exe (no built exe; the installed copy stays)"; fi
    if [ -f "$LOAD_DLL" ]; then copy_in "$LOAD_DLL" game/bin/win64/cs2mcp_mmload.dll
    else say "  kept: game/bin/win64/cs2mcp_mmload.dll (no built DLL; the installed copy stays)"; fi
    copy_in "$MM_BUILD/metamod/bin/win64/server.dll"         game/csgo/addons/metamod/bin/win64/server.dll
    copy_in "$MM_BUILD/metamod/bin/win64/metamod.2.cs2.dll"  game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll
    # metaplugins.ini is METAMOD's config, not ours (we load through addons/metamod/cs2mcp.vdf):
    # create it only when absent, never overwrite it, never delete it in uninstall unless the
    # INSTALLED list says we created it AND it is still byte-identical to what we created
    if [ -f "$APP/game/csgo/addons/metamod/metaplugins.ini" ]; then
        say "  kept: game/csgo/addons/metamod/metaplugins.ini (NOT ours - Metamod's config; never overwritten)"
    else
        copy_in "$MM_BUILD/metamod/metaplugins.ini" game/csgo/addons/metamod/metaplugins.ini
    fi
    write_vdf
    say "install done (wine: tool + launcher + metamod, one image, gameinfo untouched)"
    if [ "$DRY" != 1 ]; then
        write_installed "$bk"   # the INSTALLED list drives uninstall's deletion (item 7)
        write_wrote_over "$bk"  # the ACTUAL WROTE_OVER list drives uninstall's restore (r4 item 1)
        say "restart the runtime to load it: bridge/runtime.sh up (its 'up' now calls install-rs.sh ensure and adds -insecure itself)"
    fi
}

# --- uninstall's edit-revert helpers -----------------------------------------------------------
# A Steam update can rewrite gameinfo.gi / sdkenginetools.txt AFTER install: uninstall must
# remove exactly OUR lines from the CURRENT file - never restore a stale Valve file from the
# backup. The backup copies are evidence + fallback only; when the removal cannot find our
# text, the uninstall REFUSES with a message instead of restoring.

remove_manifest_entry() {  # remove_manifest_entry <in> <out>: rc 0 removed/not present,
                           # rc 1 unchanged, rc 3 our entry is there but not in the exact
                           # inserted form -> refuse. The caller writes <out> to a temp and
                           # renames (r3 item 4: never an in-place rewrite of a game file).
    python3 -I - "$1" "$2" <<'PY'
import sys
p, outp = sys.argv[1], sys.argv[2]
t = open(p, encoding="utf-8", newline="").read()
if 'm_Name = "cs2_bridge"' not in t:
    sys.exit(1)   # already gone (idempotent)
eol = "\r\n" if "\r\n" in t else "\n"
entry = ('\t\t{' + eol + '\t\t\tm_Name = "cs2_bridge"' + eol
         + '\t\t\tm_FriendlyName = "CS2 Bridge (Rust)"' + eol
         + '\t\t\tm_Library = "tools/cs2_bridge.dll"' + eol
         + '\t\t\tm_bShowInToolsMenu = false' + eol + '\t\t},' + eol)
if entry in t:
    t = t.replace(entry, "", 1)
if 'm_Name = "cs2_bridge"' in t:
    sys.exit(3)   # still there, not our exact block: the file changed since install
open(outp, "w", encoding="utf-8", newline="").write(t)
sys.exit(0)
PY
}

remove_gameinfo_lines() {  # remove_gameinfo_lines <tag uninstall|migrate> <file> <kind...>: delete OUR
                           # recorded lines (first occurrence each, anchored per-line) from the
                           # CURRENT file; rc 1 when nothing matched (already gone)
    local tag="$1" f="$2"; shift 2
    local t
    tmp_for "$f"
    t="$TMP_PATH"
    python3 -I - "$f" "$t" "$@" <<'PY'
import re, sys
p, outp, kinds = sys.argv[1], sys.argv[2], sys.argv[3:]
t = open(p, encoding="utf-8", newline="").read()
orig = t
for kind in kinds:
    pat = {"gameinfo:Game metamod": r"^[ \t]*Game[ \t]+metamod[ \t]*\r?\n",
           "gameinfo:Write csgo":   r"^[ \t]*Write[ \t]+csgo[ \t]*\r?\n"}.get(kind)
    if pat:
        t = re.sub(pat, "", t, count=1, flags=re.M)
if t == orig:
    sys.exit(1)
open(outp, "w", encoding="utf-8", newline="").write(t)
PY
    local rc=$?
    [ "$rc" = 0 ] || { rm -f "$t"; return "$rc"; }
    commit_tmp "$t" "$f" "$tag:${f#"$APP"/}"   # app-relative, like every other fault point
    return 0
}

cmd_uninstall_wine() {
    guard_live_game
    # uninstall contract: the tree goes back to what it was before the FIRST install, byte for
    # byte, and only what install added is removed - hence the OLDEST backup (see oldest_backup)
    local bk; bk="$(oldest_backup)"
    [ -n "$bk" ] || die "no $BK_PREFIX-* directory under $BACKUP_ROOT - refusing to uninstall without a backup"
    verify_backup_sha "$bk"
    if [ ! -f "$bk/EDITS_WROTE" ]; then
        die "no EDITS_WROTE in $bk (the install that wrote this backup did not complete) - refusing to uninstall: nothing would be removed"
    fi
    # the records of EVERY backup of this tree (r3 item 3, NIT S4): the INSTALLED union is the
    # oldest backup's list plus what a later ensure repair created - files a later repair
    # created are removed too. A backup without a list (an old-format backup) is warned about
    # and skipped - what IT created is unknown; the refusal (r2's contract, nothing removed
    # silently) happens only when NO backup of this tree carries a list. Validated BEFORE
    # anything is touched; every contributing backup is SHA256SUMS-verified (r5 NIT).
    records_load verify
    if [ "${#REC_INST[@]}" -eq 0 ]; then
        die "no INSTALLED list in any backup of this tree under $BACKUP_ROOT (the installs that wrote them did not complete) - refusing to uninstall: nothing would be removed"
    fi
    # r4 item 3: the INSTALLED union never deletes a PRE-INSTALL file - an entry the OLDEST
    # backup's SHA256SUMS lists pre-existed the first install (e.g. deleted by its owner, then
    # recreated by a later ensure repair) and is SKIPPED here; the WROTE_OVER restore below
    # puts the owner's pre-install bytes back if install overwrote it.
    local -a del_list=()
    local line rel
    for line in ${REC_INST[@]+"${REC_INST[@]}"}; do
        [ -n "$line" ] || continue
        rel="${line%% *}"
        if rec_pre_first "$rel"; then
            say "  (INSTALLED: $rel pre-existed the first install - NOT deleted; the WROTE_OVER restore puts the owner's bytes back if install overwrote it)"
        else
            del_list+=("$line")
        fi
    done
    if [ "$DRY" = 1 ]; then
        say "dry-run: would uninstall from $bk (nothing changes)"
        return 0
    fi
    sweep_dest_temps "$APP" "${TOOL_DESTS[@]}" "${EDIT_DESTS[@]}" "${MM_DESTS[@]}" "${LAUNCH_DESTS[@]}" "${LEGACY_MM_DESTS[@]}"
    say "restoring from $bk (pre-first-install image)"
    # 1) restore ONLY what install OVERWROTE (r4 item 1): a path is restored only when it is
    #    in a backup's WROTE_OVER (existed before THAT install, our bytes written). Files that
    #    existed but that we did NOT write are NEVER touched (no "restore every file the
    #    backup lists"). A failed restore is a warning for that path, never a stuck uninstall.
    #    The CHAIN rule (r5 item S1, chain_src_for): install may have written the same path
    #    more than once - a path's FIRST entry (the oldest backup's) does NOT describe our LAST
    #    write. If no backup's WROTE_OVER sha matches the current sha, the file changed after
    #    our LAST write: leave it, message.
    local -a wro_paths=()
    local handled=" " j=0 wro_rel
    while [ "$j" -lt "${#REC_WRO[@]}" ]; do
        wro_rel="${REC_WRO[$j]}"
        case "$handled" in
            *" $wro_rel "*) ;;
            *) handled="$handled$wro_rel "; wro_paths+=("$wro_rel") ;;
        esac
        j=$((j+3))
    done
    local wro_p wro_cur rt crc
    for wro_p in ${wro_paths[@]+"${wro_paths[@]}"}; do
        if [ ! -f "$APP/$wro_p" ]; then
            say "  not restored: $wro_p (gone after install - removed by its owner?; not recreated)"
            continue
        fi
        wro_cur="$(sha_of "$APP/$wro_p")"
        crc=0
        chain_src_for "$wro_p" "$wro_cur" || crc=$?
        if [ "$crc" = 1 ]; then
            say "  not restored: $wro_p (changed after install - Steam/user?; left as it is now)"
            continue
        elif [ "$crc" != 0 ]; then
            say "  WARN: not restored: $wro_p (the backup copy is missing)"
            continue
        fi
        rt="$APP/$wro_p.tmp.$$"
        cp -p "$CHAIN_SRC" "$rt"
        TEMPS+=("$rt")
        commit_tmp "$rt" "$APP/$wro_p" "uninstall-restore:$wro_p"
        if cmp -s "$CHAIN_SRC" "$APP/$wro_p"; then
            say "  restored (install overwrote it): $wro_p"
        else
            say "  WARN: restore of $wro_p differs from the backup copy"
        fi
    done
    # 2) delete EXACTLY what install created, per the INSTALLED union MINUS the pre-install
    #    paths (r4 item 3, see above) - never a dest-list guess: a file the install did not
    #    create is never touched here. metaplugins.ini is deleted only when still
    #    byte-identical to what we created (item 6). Duplicate lines across backups are
    #    harmless (the second sees the file already gone). The gameinfo-era paths an old
    #    install recorded (game/metamod/..., content/metamod/.keep) are in the union too.
    local psha
    for line in ${del_list[@]+"${del_list[@]}"}; do
        [ -n "$line" ] || continue
        rel="${line%% *}"
        psha=""
        case "$line" in *" "*) psha="${line#"$rel" }" ;; esac
        if [ "$rel" = game/csgo/addons/metamod/metaplugins.ini ]; then
            if [ -n "$psha" ] && [ -f "$APP/$rel" ] && [ "$(sha_of "$APP/$rel")" = "$psha" ]; then
                rm -f "$APP/$rel"
                say "  removed (created by install, byte-identical to what we created): $rel"
            else
                say "  kept: $rel (NOT ours - deleted only when we created it and it is unchanged)"
            fi
            continue
        fi
        if [ -e "$APP/$rel" ]; then
            rm -f "$APP/$rel"
            say "  removed (created by install): $rel"
        else
            say "  (INSTALLED: $rel already absent)"
        fi
    done
    # LEGACY: the second copy an OLD script's install left behind is never in INSTALLED - it is
    # still removed explicitly here (see LEGACY_MM_DESTS), and its dirs rmdir'd if empty below.
    for rel in "${LEGACY_MM_DESTS[@]}"; do
        if [ -e "$APP/$rel" ]; then
            rm -f "$APP/$rel"
            say "  removed (LEGACY copy from an old install): $rel"
        fi
    done
    local d
    for d in "${MM_DIRS[@]}" "${LEGACY_MM_DIRS[@]}" "${OLD_MM_DIRS[@]}"; do
        if [ -d "$APP/$d" ]; then
            if rmdir "$APP/$d" 2>/dev/null; then say "  removed empty dir: $d"; fi
        fi
    done
    # 4) revert OUR EDITS in the CURRENT files LAST (r4 item 1: after the file work
    #    succeeded): remove exactly the lines EDITS_WROTE recorded (lines that pre-existed
    #    install are NOT recorded and survive), keeping any Steam-update modifications. The
    #    kinds are the UNION over every backup of the tree (header); the gameinfo kinds exist
    #    only in gameinfo-era backups. If a current file is gone or our text can no longer be
    #    found: REFUSE - never silently restore an old Valve file from the backup.
    local kind gi_kinds=()
    for kind in ${REC_KINDS[@]+"${REC_KINDS[@]}"}; do
        case "$kind" in
            manifest:cs2_bridge)
                [ -f "$MAN" ] || die "cannot revert the manifest edit: $MAN is GONE (game file damaged? refusing - verify the game files; nothing restored)"
                # r3 item 4: the revert is atomic too - temp in the same dir + mv, like every
                # other tree write; the fault point makes the rename testable
                local mt mrc=0
                tmp_for "$MAN"
                mt="$TMP_PATH"
                remove_manifest_entry "$MAN" "$mt" || mrc=$?
                case "$mrc" in
                    0) commit_tmp "$mt" "$MAN" uninstall:game/bin/sdkenginetools.txt
                       say "  reverted: game/bin/sdkenginetools.txt (our cs2_bridge entry removed)" ;;
                    1) rm -f "$mt"; say "  (manifest: our entry already absent)" ;;
                    *) rm -f "$mt"
                       die "$MAN: our cs2_bridge entry is there but no longer in the exact form install wrote - refusing to restore a stale Valve file (verify the game files)" ;;
                esac ;;
            "gameinfo:Game metamod"|"gameinfo:Write csgo") gi_kinds+=("$kind") ;;
            *) say "  (unknown EDITS_WROTE line ignored: $kind)" ;;
        esac
    done
    if [ "${#gi_kinds[@]}" -gt 0 ]; then
        [ -f "$GI" ] || die "cannot revert the gameinfo edits: $GI is GONE (game file damaged? refusing - verify the game files; nothing restored)"
        # r3 NIT: only rc 1 is "already absent" - any other failure is an ERROR, not a silent skip
        local grc=0
        remove_gameinfo_lines uninstall "$GI" "${gi_kinds[@]}" || grc=$?
        case "$grc" in
            0) say "  reverted: game/csgo/gameinfo.gi (our lines removed, your other changes kept)" ;;
            1) say "  (gameinfo: our lines already absent)" ;;
            *) die "removing our gameinfo lines failed (rc $grc) - refusing to continue (verify the game files)" ;;
        esac
    fi
    # retire this cycle's backups: a later install/uninstall cycle must restore ITS pre-install image, not
    # this one (a game update may have changed sdkenginetools.txt in between)
    mkdir -p "$BACKUP_ROOT/retired"
    local b
    while IFS= read -r b; do
        if [ -n "$b" ] && mv "$b" "$BACKUP_ROOT/retired/" 2>/dev/null; then
            say "  retired backup: ${b##*/}"
        else
            say "  WARN: could not retire backup ${b##*/} (mv failed) - it stays in place; uninstall of the NEXT cycle restores from the oldest backup that matches this tree"
        fi
    done < <(backups_for_app)
    say "uninstall complete (restore from the oldest backup + remove what install added)"
}

mig_lines() {  # prints one line per old-layout marker found by the last scan (status + ensure)
    local x
    for x in ${MIG_DELETE[@]+"${MIG_DELETE[@]}"}; do echo "$x - created by an old install of ours (migration removes it)"; done
    for x in ${MIG_RESTORE[@]+"${MIG_RESTORE[@]}"}; do echo "$x - an old install of ours overwrote the owner's file (migration restores it)"; done
    for x in ${MIG_UNKNOWN[@]+"${MIG_UNKNOWN[@]}"}; do echo "$x - NOT attributable to any install of ours (install/ensure will REFUSE to migrate)"; done
    for x in ${MIG_GI_KINDS[@]+"${MIG_GI_KINDS[@]}"}; do echo "${GI_LABEL}: \`${x#gameinfo:}\` line written by an old install of ours (migration removes it)"; done
    for x in ${MIG_GI_FOREIGN[@]+"${MIG_GI_FOREIGN[@]}"}; do echo "${GI_LABEL}: \`$x\` line NOT written by us (install/ensure will REFUSE to migrate)"; done
    return 0
}

cmd_status_wine() {
    # r3 NIT: the REAL-tree label is decided on realpath (a trailing slash / .. / a symlink
    # spelling of the real tree must still say REAL)
    say "game: $GAME  [$([ "$(realpath -m -- "$GAME")" = "$(realpath -m -- "$REAL_GAME")" ] && echo 'REAL tools install' || echo test tree)]"
    local rel
    for rel in "${TOOL_DESTS[@]}"; do
        if [ -f "$APP/$rel" ]; then
            # r3 NIT: the sha + which build dir the installed lib's bytes come from
            say "  tool dll   $rel  present  sha256=$(sha_of "$APP/$rel")  $(lib_build_match "$APP/$rel" "$RS_LIB" "$ROOT/target/bridge-rs/win64/cs2_bridge.dll" "$MCP_DIR/target/bridge-rs/win64/cs2_bridge.dll")"
        else
            say "  tool dll   $rel  absent"
        fi
    done
    for rel in "${LAUNCH_DESTS[@]}"; do
        if [ -f "$APP/$rel" ]; then
            local built="$LAUNCH_EXE" ; case "$rel" in *mmload.dll) built="$LOAD_DLL" ;; esac
            say "  launcher   $rel  present  sha256=$(sha_of "$APP/$rel")  $(lib_build_match "$APP/$rel" "$built")"
        else
            say "  launcher   $rel  absent"
        fi
    done
    rel=game/bin/sdkenginetools.txt
    if [ -f "$APP/$rel" ] && grep -q 'm_Name = "cs2_bridge"' "$APP/$rel"; then
        say "  manifest   $rel  cs2_bridge entry present"
    else
        say "  manifest   $rel  cs2_bridge entry absent"
    fi
    for rel in "${MM_DESTS[@]}"; do
        local line
        if [ -f "$APP/$rel" ]; then
            line="$rel  present"
            case "$rel" in *.dll) line="$line  sha256=$(sha_of "$APP/$rel")" ;; esac
        elif [ -e "$APP/$rel" ]; then
            line="$rel  present (non-file)"
        else
            line="$rel  absent"
        fi
        say "  metamod    $line"
    done
    # the gameinfo path is gone: gameinfo.gi is not ours to look at, except for the OLD layout's
    # leftovers (an old install's lines + game/metamod + content/metamod)
    records_load
    wine_old_scan
    if [ "$(mig_count)" = 0 ]; then
        say "  old layout none (gameinfo untouched, no game/metamod, no content/metamod)"
        local od
        for od in game/metamod content/metamod; do
            # not attributable to us any more (no recorded file left), so never removed - but say so
            [ -d "$APP/$od" ] && say "  old layout dir still present (not ours, left alone): $od ($(find "$APP/$od" | tail -n +2 | wc -l) entries)"
        done
    else
        local l
        while IFS= read -r l; do say "  old layout $l"; done < <(mig_lines)
    fi
    # LEGACY: the second copy an old --metamod install may have left behind (see the header)
    for rel in "${LEGACY_MM_DESTS[@]}"; do
        [ -f "$APP/$rel" ] && say "  legacy     $rel  present (installed by an OLD script; uninstall removes it)"
    done
    return 0
}

# --- linux commands -----------------------------------------------------------------------------

LINUX_G=""
linux_game_root() {  # sets LINUX_G (absolute game dir) or fails loudly; CS2_LINUX_GAME bypasses the volume
    if [ -n "$LINUX_GAME" ]; then
        LINUX_G="$LINUX_GAME"
        return 0
    fi
    local vmp
    vmp="$(linux_vmp)" || die "podman volume inspect $LINUX_VNAME failed (rc $?) - cannot resolve the Linux game dir (set CS2_LINUX_GAME for a test tree)"
    LINUX_G="$vmp/steamapps/common/Counter-Strike Global Offensive/game"
}

linux_installed_p() {  # linux_installed_p <game-relative-path>: rc 0 when the regular file exists
    lrun "$LINUX_G" "$1" <<'EOF'
set -euo pipefail
[ -f "$1/$2" ]
EOF
}

linux_preflight() {
    say_build_checkout
    # the build artefacts are required only when the file must be (re)copied (item 5): an
    # installed copy + no build = current, and ensure still repairs the config
    if [ -f "$RS_LIB" ]; then
        say "export gate on $RS_LIB:"
        "$HERE/check-exports.sh" "$RS_LIB" || die "check-exports.sh refused $RS_LIB (the engine calls a tool .so's exports; refusing)"
    elif linux_installed_p csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so; then
        say "no built .so at $RS_LIB - the INSTALLED .so counts as current (only config is repaired)"
    else
        die "no built .so at $RS_LIB and none installed - BUILD IT FIRST (bridge/build-rs.sh, or point CS2_BRIDGE_LINUX_SO at one)"
    fi
    # the LD_PRELOAD loader (NOT export-gated - it exports dlopen, not CreateInterface)
    if [ -f "$MMLOAD_SO" ]; then
        :
    elif linux_installed_p csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so; then
        say "no built mmload .so at $MMLOAD_SO - the INSTALLED one counts as current (only config is repaired)"
    else
        die "no mmload .so at $MMLOAD_SO and none installed - BUILD IT FIRST (run bridge/build-rs.sh, or point CS2_MMLOAD_SO at one)"
    fi
    local mf
    for mf in "$MM_BUILD/metamod/bin/linux64/server.so" "$MM_BUILD/metamod/bin/linuxsteamrt64/libserver.so" \
              "$MM_BUILD/metamod/bin/linuxsteamrt64/metamod.2.cs2.so" "$MM_BUILD/metamod/metaplugins.ini"; do
        [ -f "$mf" ] || die "missing metamod build file: $mf (build metamod per bridge/metamod/README.md, or point CS2_MM_LINUX at the build)"
    done
    # install only ADDS files, but it must never invent a game tree: csgo/ has to be there
    local out rc=0
    out="$(lrun "$LINUX_G" <<'EOF'
set -euo pipefail
G="$1"
[ -d "$G" ] || { echo "MISSING: whole game dir $G absent"; exit 3; }
[ -d "$G/csgo" ] || { echo "MISSING: $G/csgo absent"; exit 3; }
echo "game tree present (csgo/)"
EOF
)" || rc=$?
    [ "$rc" = 0 ] || { say "linux preflight: $out"; die "refusing (game tree incomplete, see the MISSING line above)"; }
    say "  preflight: $out"
}

# --- the old (gameinfo-era) layout: detection + migration (linux) -------------------------------
# What an OLD install left on a linux tree: the VSP loader vdfs csgo/addons/metamod{,_x64}.vdf and
# our two gameinfo lines (`Game csgo/addons/metamod`, `Write csgo`). Same attribution rules as the
# wine migration (header "migration"); the tree is read through `podman unshare`, so the probe
# reports what is there and the classification runs here against the host-readable backups.
linux_old_scan() {
    mig_reset
    local out l hg=0 hw=0 a b
    out="$(lrun "$LINUX_G" <<'EOF'
set -euo pipefail
G="$1"
[ -d "$G" ] || exit 0
for rel in csgo/addons/metamod.vdf csgo/addons/metamod_x64.vdf; do
    if [ -L "$G/$rel" ] || [ -e "$G/$rel" ]; then
        if [ -f "$G/$rel" ] && [ ! -L "$G/$rel" ]; then
            echo "file $(sha256sum "$G/$rel" | awk '{print $1}') $rel"
        else
            echo "file - $rel"
        fi
    fi
done
gi="$G/csgo/gameinfo.gi"
if [ -f "$gi" ]; then
    if grep -Eq $'^[[:space:]]*Game[[:space:]]+csgo/addons/metamod[[:space:]]*$' "$gi"; then echo "line Game csgo/addons/metamod"; fi
    if grep -Eq $'^[[:space:]]*Write[[:space:]]+csgo[[:space:]]*$' "$gi"; then echo "line Write csgo"; fi
fi
exit 0
EOF
)" || die "linux: reading the tree for the old layout failed"
    while IFS= read -r l; do
        case "$l" in
            "file "*) a="${l#file }"; b="${a#* }"; a="${a%% *}"; mig_note_file "$b" "$a" ;;
            "line Game csgo/addons/metamod") hg=1 ;;
            "line Write csgo") hw=1 ;;
        esac
    done <<<"$out"
    mig_note_gameinfo "gameinfo:Game csgo/addons/metamod" "$hg" "$hw"
    return 0
}

UW_KINDS=() UW_RESTORE=() UW_RELS=() UW_DIRS=()
uw_restore_triplets() {  # uw_restore_triplets <rel>...: append the WROTE_OVER triplets of each path
                         # (rel sha backup-dir), that path's entries NEWEST backup first - what
                         # linux_unwind's chain walk expects
    local p idx
    for p in "$@"; do
        idx=$((${#REC_WRO[@]} - 3))
        while [ "$idx" -ge 0 ]; do
            if [ "${REC_WRO[$idx]}" = "$p" ]; then
                UW_RESTORE+=("$p" "${REC_WRO[$((idx+1))]}" "${REC_WRO[$((idx+2))]}")
            fi
            idx=$((idx-3))
        done
    done
}

migrate_linux() {
    migrate_linux_do
    if [ "$DRY" != 1 ]; then reverted_kinds; fi
}
migrate_linux_do() {
    records_load
    linux_old_scan
    mig_refuse_if_foreign
    local nf=$(( ${#MIG_DELETE[@]} + ${#MIG_RESTORE[@]} )) ng=${#MIG_GI_KINDS[@]} rel
    [ $((nf + ng)) -gt 0 ] || return 0
    say "migration: the old (gameinfo-era) layout is on this tree - removing what the backups record as ours"
    if [ "$DRY" = 1 ]; then
        for rel in ${MIG_DELETE[@]+"${MIG_DELETE[@]}"}; do say "DRY: migrate: remove $rel (created by an old install of ours)"; done
        for rel in ${MIG_RESTORE[@]+"${MIG_RESTORE[@]}"}; do say "DRY: migrate: restore $rel (the owner's bytes; the old install overwrote it)"; done
        for rel in ${MIG_GI_KINDS[@]+"${MIG_GI_KINDS[@]}"}; do say "DRY: migrate: revert csgo/gameinfo.gi line ${rel#gameinfo:}"; done
        return 0
    fi
    # every contributing backup is SHA256SUMS-verified first (a mismatch dies here, nothing
    # touched), then the plan is recomputed from the verified records
    records_verify
    linux_old_scan
    mig_refuse_if_foreign
    UW_KINDS=() UW_RESTORE=() UW_RELS=() UW_DIRS=()
    for rel in ${MIG_GI_KINDS[@]+"${MIG_GI_KINDS[@]}"}; do UW_KINDS+=("$rel"); done
    uw_restore_triplets ${MIG_RESTORE[@]+"${MIG_RESTORE[@]}"}
    for rel in ${MIG_DELETE[@]+"${MIG_DELETE[@]}"}; do UW_RELS+=("$rel"); done
    linux_unwind migrate "${REC_BKS[0]}"
    say "migration done: the tree no longer carries the old layout"
}

linux_backup() {  # linux_backup <bk-dir> <dry 0|1>: copies every pre-existing dest out of the tree.
                  # ATOMIC (like wine's backup): built into <bk-dir>.partial, renamed when complete;
                  # EDITS_WROTE is written EMPTY (a linux install edits no game file any more);
                  # INSTALLED (created files) is written by cmd_install_linux.
    local bk="$1" dry="$2" rel out=()
    if [ "$dry" = 1 ]; then
        say "dry-run: would back up first under $BACKUP_ROOT/$BK_PREFIX-<UTC>; pre-existing (would-be-backed-up) files:"
        lrun "$LINUX_G" <<'EOF' || true
set -euo pipefail
G="$1"
ourtmp() { case "${1##*/}" in *.tmp.*) case "${1##*.}" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac ;; *) return 1 ;; esac; }
if [ -d "$G/csgo/addons/metamod" ]; then
    while IFS= read -r f; do ourtmp "$f" && continue; echo "would back up: ${f#"$G"/}"; done < <(find "$G/csgo/addons/metamod" -type f)
fi
if [ -d "$G/csgo/addons/cs2mcp" ]; then
    while IFS= read -r f; do ourtmp "$f" && continue; echo "would back up: ${f#"$G"/}"; done < <(find "$G/csgo/addons/cs2mcp" -type f)
fi
EOF
        return 0
    fi
    # r4 item 1: the planned WROTE_OVER needs the sha of the bytes we would write - the built
    # .so's sha when the .so will be (re)copied (no build artefact -> the installed copy
    # stays, never written), and the vdf producer's sha
    local has=0 hasmm=0; if [ -f "$RS_LIB" ]; then has=1; fi; if [ -f "$MMLOAD_SO" ]; then hasmm=1; fi
    local vdfsha; vdfsha="$(linux_vdf_text | sha256sum | awk '{print $1}')"
    PARTIAL_BK="$bk.partial"
    mkdir -p "$PARTIAL_BK"
    printf '%s\n' "$(backup_app_key)" > "$PARTIAL_BK/APP_TREE"
    local rc=0
    out="$(lrun "$LINUX_G" "$bk.partial" "$MM_BUILD" "$has" "$RS_LIB" "$vdfsha" "$hasmm" "$MMLOAD_SO" <<'EOF'
set -euo pipefail
G="$1"; BK="$2"; MM="$3"; HASLIB="$4"; SO="$5"; VDFSHA="$6"; HASMM="$7"; MMSO="$8"
ourtmp() { case "${1##*/}" in *.tmp.*) case "${1##*.}" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac ;; *) return 1 ;; esac; }
have=()   # rel list, also the output order (tree files first, then the flats)
add() {
    local r
    for r in "${have[@]:-}"; do [ "$r" = "$1" ] && return 0; done
    have+=("$1")
}
if [ -d "$G/csgo/addons/metamod" ]; then
    while IFS= read -r f; do ourtmp "$f" && continue; add "${f#"$G"/}"; done < <(find "$G/csgo/addons/metamod" -type f | sort)
fi
if [ -d "$G/csgo/addons/cs2mcp" ]; then
    while IFS= read -r f; do ourtmp "$f" && continue; add "${f#"$G"/}"; done < <(find "$G/csgo/addons/cs2mcp" -type f | sort)
fi
: > "$BK/SHA256SUMS"
for rel in "${have[@]:-}"; do
    [ -n "$rel" ] || continue   # (an empty array expands to one empty word: nothing pre-existed)
    mkdir -p "$BK/$(dirname "$rel")"
    cp -p "$G/$rel" "$BK/$rel"
    echo "backed up: $rel"
    (cd "$BK" && sha256sum "$rel" >> SHA256SUMS)
done
# ANCHORS: a linux install relies on no Valve-owned line (no game file is edited) - the file
# stays, empty, so every backup has the same shape
: > "$BK/ANCHORS"
# PLANNED INSTALLED (r3 item 3, like wine's backup): the dests ABSENT at backup time are the
# ones install would CREATE - the list goes into the .partial BEFORE it is finalised, so a
# kill between the backup and the ACTUAL list (write_installed) still leaves an uninstallable
# tree. A created metaplugins.ini records the sha of the build's file (what we would write).
: > "$BK/INSTALLED"
plan() {
    local r="$1"
    if [ "$(basename "$r")" = metaplugins.ini ]; then
        echo "$r $(sha256sum "$MM/metamod/${r#csgo/addons/metamod/}" | awk '{print $1}')"
    else
        echo "$r"
    fi >> "$BK/INSTALLED"
}
for rel in csgo/addons/metamod/cs2mcp.vdf csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so \
           csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so; do
    [ -e "$G/$rel" ] || plan "$rel"
done
while IFS= read -r f; do
    rel="csgo/addons/metamod/${f#"$MM/metamod/"}"
    [ -e "$G/$rel" ] || plan "$rel"
done < <(find "$MM/metamod" -type f | sort)
# PLANNED WROTE_OVER (r4 item 1): pre-existing paths install would OVERWRITE + the sha256 of
# the bytes we would write (the SOURCE file's sha - what the temp will hold).
# metaplugins.ini is never here (create-if-absent only). uninstall restores a path only when
# it is in WROTE_OVER AND its current sha still equals what we wrote.
: > "$BK/WROTE_OVER"
if [ -d "$G/csgo/addons/metamod" ]; then
    while IFS= read -r f; do
        rel="csgo/addons/metamod/${f#"$MM/metamod/"}"
        [ "$(basename "$rel")" = metaplugins.ini ] && continue
        [ -e "$G/$rel" ] || continue
        echo "$rel $(sha256sum "$f" | awk '{print $1}')" >> "$BK/WROTE_OVER"
    done < <(find "$MM/metamod" -type f | sort)
fi
if [ -e "$G/csgo/addons/metamod/cs2mcp.vdf" ]; then
    echo "csgo/addons/metamod/cs2mcp.vdf $VDFSHA" >> "$BK/WROTE_OVER"
fi
if [ "$HASLIB" = 1 ] && [ -e "$G/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so" ]; then
    echo "csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so $(sha256sum "$SO" | awk '{print $1}')" >> "$BK/WROTE_OVER"
fi
if [ "$HASMM" = 1 ] && [ -e "$G/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so" ]; then
    echo "csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so $(sha256sum "$MMSO" | awk '{print $1}')" >> "$BK/WROTE_OVER"
fi
exit 0
EOF
)" || rc=$?
    say "$out"
    [ "$rc" = 0 ] || die "linux backup failed inside the tree (rc $rc, see the output above)"
    : > "$PARTIAL_BK/EDITS_WROTE"
    for rel in ${CANCEL_KINDS[@]+"${CANCEL_KINDS[@]}"}; do printf 'reverted:%s\n' "$rel" >> "$PARTIAL_BK/EDITS_WROTE"; done
    fault mid-backup
    mv -f "$PARTIAL_BK" "$bk" || die "cannot finalise the backup $bk"
    PARTIAL_BK=""
    return 0
}

linux_install_capture() {  # -> stdout: the inner script's output; rc: the inner script's rc
                           # (99 = the test fault hook simulated a kill)
    local vdfsrc
    linux_vdf_file
    vdfsrc="$VDF_TMP"
    local rc=0
    lrun "$LINUX_G" "$MM_BUILD" "$RS_LIB" "$MMLOAD_SO" "$vdfsrc" "${INSTALL_RS_FAULT:-}" <<'EOF' || rc=$?
set -euo pipefail
G="$1"; MM="$2"; SO="$3"; MMSO="$4"; VDFSRC="$5"; FAULT="$6"
# fault hook (TEST-ONLY): INSTALL_RS_FAULT passed positionally; see fault() on the host side
faultpt() { if [ -n "$FAULT" ] && [ "$FAULT" = "$1" ]; then echo "FAULT HOOK (test-only): simulating a kill at '$1'"; exit 99; fi; return 0; }
# our temps (host-pid-named <dest>.tmp.$$) are registered and removed by the trap - a killed
# run leaves none of them behind (never rm by glob outside our own names)
TEMPS=()
cleanup_temps() { local t; for t in "${TEMPS[@]:-}"; do [ -n "$t" ] && rm -f "$t"; done; return 0; }
trap cleanup_temps EXIT
trap 'trap - EXIT; cleanup_temps; exit 130' INT
trap 'trap - EXIT; cleanup_temps; exit 143' TERM
# put: atomic per-file write into the tree - temp in the dest's dir, owner/mode kept via a
# `cp -p` of the original when it exists, new content written INTO the temp, then mv -f
ourtmp() { case "${1##*/}" in *.tmp.*) case "${1##*.}" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac ;; *) return 1 ;; esac; }
sweep_dest() {  # r3 NIT: remove the SIGKILLed previous run's temps of THIS dest - our exact
                # naming (<dest>.tmp.<all-digits>) only
    local t
    for t in "$1".tmp.*; do
        [ -e "$t" ] || return 0
        ourtmp "$t" || continue
        rm -f "$t"; echo "removed a killed run's stray temp: ${t#"$G"/}"
    done
    return 0
}
put() {
    local src="$1" rel="$2" t
    local dest="$G/$rel"
    mkdir -p "$(dirname "$dest")"
    sweep_dest "$dest"
    t="$dest.tmp.$$"
    if [ -e "$dest" ]; then
        cp -p "$dest" "$t"; cat "$src" > "$t"
        # r4 item 1: record the sha of the bytes WE wrote (the temp, before the mv) - uninstall
        # restores this path only when its current sha still equals it
        echo "wrote-over: $rel $(sha256sum "$t" | awk '{print $1}')"   # pre-existed: install OVERWROTE it
    else
        cp -p "$src" "$t"
        # a CREATED metaplugins.ini records the sha of the bytes WE wrote - the temp's (the
        # dest does not exist until the mv below; r3 item 2: the old code hashed $dest here,
        # got "No such file", an empty sha, and uninstall then deleted another plugin's
        # registration). uninstall deletes it only when it is still byte-identical (item 6).
        if [ "$(basename "$rel")" = metaplugins.ini ]; then
            echo "wrote-created: $rel $(sha256sum "$t" | awk '{print $1}')"
        else
            echo "wrote-created: $rel"   # install CREATED it (uninstall deletes it - INSTALLED)
        fi
    fi
    TEMPS+=("$t")
    faultpt "pre-mv:$rel"
    mv -f "$t" "$dest"
}
mkdir -p "$G/csgo/addons"
if [ -d "$G/csgo/addons/metamod" ]; then
    echo "existing: csgo/addons/metamod/ (only our files overwrite ours)"
else
    echo "created: csgo/addons/metamod/ (from the metamod build)"
fi
while IFS= read -r f; do
    # metaplugins.ini is METAMOD's config, not ours: create only when absent, never overwrite
    if [ "$(basename "$f")" = metaplugins.ini ] && [ -f "$G/csgo/addons/metamod/metaplugins.ini" ]; then
        echo "kept: csgo/addons/metamod/metaplugins.ini (NOT ours - Metamod's config; never overwritten)"
    else
        put "$f" "csgo/addons/metamod/${f#"$MM/metamod/"}"
    fi
done < <(find "$MM/metamod" -type f | sort)
# the two libraries under csgo/addons/cs2mcp/bin/linuxsteamrt64/: the bridge (Metamod loads it
# through cs2mcp.vdf) and the LD_PRELOAD loader. Built -> (re)copied; else an installed copy stays
put_lib() {  # put_lib <host-src> <game-relative-dest>
    if [ -f "$1" ]; then
        put "$1" "$2"
        echo "installed: $2"
    elif [ -f "$G/$2" ]; then
        echo "kept: $2 (no built copy; the installed one stays)"
    else
        echo "FATAL: no built ${2##*/} at the host and none installed - build it first (bridge/build-rs.sh)"
        exit 2
    fi
}
put_lib "$SO" csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so
put_lib "$MMSO" csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so
# the vdf: exactly the bytes the host produced (VDFSRC, from linux_vdf_text) - install and
# ensure share one producer, so an install can never leave a vdf ensure reads as stale
put "$VDFSRC" csgo/addons/metamod/cs2mcp.vdf
echo "created: csgo/addons/metamod/cs2mcp.vdf"
# (no gameinfo edit: the tree's game files are not touched, header "install (linux)")
EOF
    return "$rc"
}

linux_unwind() {  # linux_unwind <tag uninstall|migrate> <bk>: restore what install overwrote
                  # (the UW_RESTORE triplets: rel sha bk..., each path's entries NEWEST backup
                  # first - the WROTE_OVER union, r4 item 1; the per-path chain walk, r5 item
                  # S1), remove only what install added (UW_RELS: the INSTALLED union minus the
                  # pre-install paths, r4 item 3), rmdir the UW_DIRS that came out empty, then
                  # revert our gameinfo edits (UW_KINDS) in the CURRENT file LAST - after the file
                  # work succeeded (r5 item S2, as wine does). uninstall passes everything the
                  # records say; the migration passes just the OLD-layout paths + lines. <bk> is
                  # the OLDEST backup (SHA256SUMS-checked inside).
    local tag="$1" bk="$2"
    if [ ! -f "$bk/EDITS_WROTE" ]; then
        die "no EDITS_WROTE in $bk (the install that wrote this backup did not complete) - refusing to uninstall: nothing would be removed"
    fi
    # every file install CREATED, per the caller's INSTALLED union (item 7) - NOT enumerated
    # from the metamod build dir: the build may be gone by now, and the recorded list is what
    # install actually did. The gameinfo is not among them: it is EDITED, reverted from the
    # current file via EDITS_WROTE, never restored from the backup.
    # args: G BK FAULT TAG KINDS... "--" RESTORE... "--" RELS... "--" DIRS...
    if lrun "$LINUX_G" "$bk" "${INSTALL_RS_FAULT:-}" "$tag" ${UW_KINDS[@]+"${UW_KINDS[@]}"} "--" ${UW_RESTORE[@]+"${UW_RESTORE[@]}"} "--" ${UW_RELS[@]+"${UW_RELS[@]}"} "--" ${UW_DIRS[@]+"${UW_DIRS[@]}"} <<'EOF'
set -euo pipefail
G="$1"; BK="$2"; FAULT="$3"; TAG="$4"; shift 4
# fault points: uninstall -> pre-mv:uninstall-restore:<rel> / pre-mv:uninstall:csgo/gameinfo.gi;
# migration -> pre-mv:migrate:<rel> / pre-mv:migrate:csgo/gameinfo.gi
RTAG="$TAG"; [ "$TAG" = uninstall ] && RTAG=uninstall-restore
# fault hook (TEST-ONLY): see fault() on the host side / faultpt in linux_install_capture
faultpt() { if [ -n "$FAULT" ] && [ "$FAULT" = "$1" ]; then echo "FAULT HOOK (test-only): simulating a kill at '$1'"; exit 99; fi; return 0; }
# our temps (<dest>.tmp.$$, same-dir) are registered and removed by the trap - a killed
# uninstall leaves none of them behind (r3 item 4; the gameinfo temp was NOT registered before)
TEMPS=()
cleanup_temps() { local t; for t in "${TEMPS[@]:-}"; do [ -n "$t" ] && rm -f "$t"; done; return 0; }
trap cleanup_temps EXIT
trap 'trap - EXIT; cleanup_temps; exit 130' INT
trap 'trap - EXIT; cleanup_temps; exit 143' TERM
KINDS=(); RESTORE=(); RELS=(); DIRS=(); slot=0
for a in "$@"; do
    if [ "$a" = "--" ]; then slot=$((slot+1)); continue; fi
    if   [ "$slot" = 0 ]; then KINDS+=("$a")
    elif [ "$slot" = 1 ]; then RESTORE+=("$a")
    elif [ "$slot" = 2 ]; then RELS+=("$a")
    else DIRS+=("$a"); fi
done
# r3 NIT: the SIGKILLed previous run's temps (<dest>.tmp.<digits>) of the files we touch are
# removed first - our exact temp naming only
ourtmp() { case "${1##*/}" in *.tmp.*) case "${1##*.}" in ''|*[!0-9]*) return 1 ;; *) return 0 ;; esac ;; *) return 1 ;; esac; }
sweep_dest() {
    local t
    for t in "$1".tmp.*; do
        [ -e "$t" ] || return 0
        ourtmp "$t" || continue
        rm -f "$t"; echo "removed a killed run's stray temp: ${t#"$G"/}"
    done
    return 0
}
sweep_dest "$G/csgo/gameinfo.gi"
for rel in "${RESTORE[@]:-}" "${RELS[@]:-}"; do
    [ -n "$rel" ] || continue
    sweep_dest "$G/${rel%% *}"
done
if [ ! -f "$BK/SHA256SUMS" ]; then echo "no SHA256SUMS in $BK"; exit 2; fi
# an EMPTY list is valid (nothing of ours pre-existed the first install); sha256sum -c refuses one
if [ -s "$BK/SHA256SUMS" ]; then
    (cd "$BK" && sha256sum -c --quiet SHA256SUMS) || { echo "backup SHA256SUMS mismatch in $BK"; exit 2; }
fi
if [ "$TAG" = uninstall ]; then echo "restoring from $BK (pre-first-install image)"
else echo "migrating the old layout (records of $BK)"; fi
# 1) restore what install OVERWROTE, per the WROTE_OVER union the caller built (r4 item 1):
#    entries are TRIPLETS (rel, sha-of-what-we-wrote, backup dir), grouped per path, that
#    path's entries NEWEST backup first. A path is restored from the NEWEST entry whose
#    recorded sha still equals the CURRENT sha (r5 item S1: install may have written the
#    same path more than once - a rebuild between installs makes ensure reinstall it, so the
#    OLDEST entry does not describe our LAST write); from there the chain steps DOWN while
#    the entry's backup copy is itself what the next-OLDER entry's install wrote - the last
#    copy reached is the bytes that existed before we FIRST wrote the path. No entry's sha
#    matches the current sha -> changed after our last write (Steam/user): LEFT ALONE with a
#    message. Gone paths / failed restores are warnings, never a stuck uninstall. The
#    gameinfo is not here (edited; reverted from the CURRENT file in step 3 below, AFTER the
#    file work - r5 item S2, as wine does).
n=${#RESTORE[@]}
i=0
while [ "$i" -lt "$n" ]; do
    rel="${RESTORE[$i]}"
    # this path's group: the consecutive triplets sharing rel (newest entry first)
    gs=$i
    ge=$((i+3))
    while [ "$ge" -lt "$n" ] && [ "${RESTORE[$ge]}" = "$rel" ]; do ge=$((ge+3)); done
    i=$ge
    [ -n "$rel" ] || continue
    sweep_dest "$G/$rel"
    if [ ! -d "$G/$(dirname "$rel")" ]; then
        echo "not restored: $rel (its directory is gone - removed by its owner after install; not recreated)"
        continue
    fi
    if [ ! -f "$G/$rel" ]; then
        echo "not restored: $rel (gone after install - removed by its owner?; not recreated)"
        continue
    fi
    cur="$(sha256sum "$G/$rel" | awk '{print $1}')"
    # the NEWEST group entry whose WROTE_OVER sha equals the current sha (the group is
    # newest-first, so the FIRST match is the newest; entry j's sha is RESTORE[j+1])
    mi=""
    j=$gs
    while [ "$j" -lt "$ge" ]; do
        if [ -z "$mi" ] && [ "${RESTORE[$((j+1))]}" = "$cur" ]; then mi=$j; fi
        j=$((j+3))
    done
    if [ -z "$mi" ]; then
        echo "not restored: $rel (changed after install - Steam/user?; left as it is now)"
        continue
    fi
    # step DOWN the chain: if this entry's backup copy is itself what the next-OLDER entry's
    # install wrote (its WROTE_OVER sha), the pre-first-install bytes live in the older
    # backup's copy - step to it, repeat
    while [ -f "${RESTORE[$((mi+2))]}/$rel" ]; do
        copy_sha="$(sha256sum "${RESTORE[$((mi+2))]}/$rel" | awk '{print $1}')"
        nxt=$((mi+3))
        [ "$nxt" -lt "$ge" ] || break
        [ "$copy_sha" = "${RESTORE[$((nxt+1))]}" ] || break
        mi=$nxt
    done
    srcbk="${RESTORE[$((mi+2))]}"
    if [ ! -f "$srcbk/$rel" ]; then
        echo "WARN: not restored: $rel (the backup copy in $srcbk is missing)"
        continue
    fi
    rt="$G/$rel.tmp.$$"
    cp -p "$srcbk/$rel" "$rt"
    TEMPS+=("$rt")
    faultpt "pre-mv:$RTAG:$rel"
    mv -f "$rt" "$G/$rel"
    cmp -s "$srcbk/$rel" "$G/$rel" || echo "WARN: restore of $rel differs from the backup copy"
    echo "restored (install overwrote it): $rel"
done
# 2) delete EXACTLY what install created, per the INSTALLED union (r3 item 3). Entries may
#    carry a sha256 (metaplugins.ini): deleted only when unchanged (item 6). The flat dests
#    appear here only when install created them (planned/actual INSTALLED) - never hardcoded.
for entry in "${RELS[@]:-}"; do
    [ -n "$entry" ] || continue
    rel="${entry%% *}"
    psha=""
    case "$entry" in *" "*) psha="${entry#"$rel" }" ;; esac
    if [ "$(basename "$rel")" = metaplugins.ini ] && [ -n "$psha" ] && [ -e "$G/$rel" ] \
       && [ "$(sha256sum "$G/$rel" | awk '{print $1}')" != "$psha" ]; then
        echo "kept: $rel (NOT ours - deleted only when we created it and it is unchanged)"
    elif [ -e "$G/$rel" ]; then
        rm -f "$G/$rel"
        echo "removed (added by install): $rel"
    else
        echo "was not installed: $rel"
    fi
done
for d in "${DIRS[@]:-}"; do
    [ -n "$d" ] || continue
    if [ -d "$G/$d" ]; then
        if rmdir "$G/$d" 2>/dev/null; then echo "removed empty dir: $d"; fi
    fi
done
# 3) revert our gameinfo edits in the CURRENT file LAST (r5 item S2, as wine does: the
#    gameinfo/manifest revert runs after the FILE work succeeded - a fault/kill at a restore
#    or delete leaves the gameinfo still carrying our lines, and a re-run completes): remove
#    exactly the recorded lines (a pre-existing line is not recorded and survives;
#    Steam-update changes survive too). The backup's gameinfo is evidence only - never
#    silently restore an old Valve file. Only gameinfo-era backups record any kind: a
#    gameinfo-free tree has nothing to revert (and needs no gameinfo.gi at all).
gi="$G/csgo/gameinfo.gi"
if [ "${#KINDS[@]}" -gt 0 ]; then
if [ -f "$gi" ]; then
    git="$gi.tmp.$$"
    if [ -e "$gi" ]; then cp -p "$gi" "$git"; else : > "$git"; fi
    TEMPS+=("$git")   # r3 item 4: registered - a killed uninstall leaves no temp behind
    if python3 - "$gi" "$git" "${KINDS[@]}" <<'PY'
import re, sys
p, outp = sys.argv[1], sys.argv[2]
kinds = [k for k in sys.argv[3:] if k]
t = open(p, encoding="utf-8", newline="").read()
orig = t
for kind in kinds:
    pat = {"gameinfo:Game csgo/addons/metamod": r"^[ \t]*Game[ \t]+csgo/addons/metamod[ \t]*\r?\n",
           "gameinfo:Write csgo":               r"^[ \t]*Write[ \t]+csgo[ \t]*\r?\n"}.get(kind)
    if pat:
        t = re.sub(pat, "", t, count=1, flags=re.M)
if t == orig:
    sys.exit(1)   # nothing to remove: our lines are already absent
open(outp, "w", encoding="utf-8", newline="").write(t)
PY
    then faultpt "pre-mv:$TAG:csgo/gameinfo.gi"
         mv -f "$git" "$gi"; echo "reverted: csgo/gameinfo.gi (our lines removed, your other changes kept)"
    else rm -f "$git"; echo "gameinfo: our lines already absent"
    fi
else
    echo "FATAL: cannot revert the gameinfo edits: $gi is GONE (game file damaged? refusing - verify the game files; nothing restored)"
    exit 2
fi
fi
EOF
    then
        :
    else
        local u_rc=$?
        [ "$u_rc" = 99 ] && exit 99   # the inner fault hook simulated a kill: propagate as such
        die "linux $tag failed inside the tree (see output above)"
    fi
}

cmd_install_linux() {
    linux_game_root
    guard_live_game
    linux_preflight
    migrate_linux   # refuses (before anything is touched) or migrates the old layout; --dry-run only reports
    if [ "$DRY" = 1 ]; then
        linux_backup "" 1
        say "dry-run: nothing below changes anything"
        say "DRY: mkdir csgo/addons/{metamod,cs2mcp/bin/linuxsteamrt64}; cp $MM_BUILD/metamod -> csgo/addons/metamod;"
        say "DRY: cp $RS_LIB $MMLOAD_SO -> csgo/addons/cs2mcp/bin/linuxsteamrt64/"
        say "DRY: write csgo/addons/metamod/cs2mcp.vdf (csgo/gameinfo.gi is not touched)"
        return 0
    fi
    local bk
    bk="$BACKUP_ROOT/$BK_PREFIX-$(date -u '+%Y%m%dT%H%M%S.%NZ')"   # ns: two cycles in one second must not collide
    linux_backup "$bk" 0
    local li_out li_rc=0
    li_out="$(linux_install_capture)" || li_rc=$?
    say "$li_out"
    if [ "$li_rc" = 99 ]; then
        exit 99   # the inner fault hook simulated a kill: propagate it as such
    fi
    [ "$li_rc" = 0 ] || die "linux install failed inside the tree (see output above)"
    # INSTALLED = the files install CREATED (wrote-created: lines); drives uninstall's deletion
    local rel t
    CREATED_LIST=()
    while IFS= read -r rel; do
        [ -n "$rel" ] && CREATED_LIST+=("$rel")
    done < <(printf '%s\n' "$li_out" | sed -n 's/^wrote-created: //p')
    # WROTE_OVER = the files install OVERWROTE (wrote-over: lines carry the sha of the bytes
    # WE wrote); drives uninstall's restore (r4 item 1)
    WROTE_OVER_LIST=()
    while IFS= read -r rel; do
        [ -n "$rel" ] && WROTE_OVER_LIST+=("$rel")
    done < <(printf '%s\n' "$li_out" | sed -n 's/^wrote-over: //p')
    write_installed "$bk"
    write_wrote_over "$bk"
    say "install done (linux: tool + loader + metamod, standard addons layout, gameinfo untouched)"
    say "linux client launches need -insecure (Metamod refuses without -dedicated/-insecure); -dedicated covers the dedicated"
}

cmd_uninstall_linux() {
    linux_game_root
    guard_live_game
    local bk; bk="$(oldest_backup)"
    [ -n "$bk" ] || die "no $BK_PREFIX-* directory under $BACKUP_ROOT - refusing to uninstall without a backup"
    # the records of EVERY backup of this tree (r3 item 3, NIT S4): the INSTALLED union - a
    # later ensure repair's created files are removed too. A backup without a list (old-format)
    # is warned about and skipped; the refusal (nothing removed silently) happens only when NO
    # backup of this tree carries a list. Validated before anything is touched; every
    # contributing backup is SHA256SUMS-verified first (r5 NIT).
    records_load verify
    if [ "${#REC_INST[@]}" -eq 0 ]; then
        die "no INSTALLED list in any backup of this tree under $BACKUP_ROOT (the installs that wrote them did not complete) - refusing to uninstall: nothing would be removed"
    fi
    if [ "$DRY" = 1 ]; then
        say "dry-run: would uninstall (linux) from $bk (nothing changes)"
        return 0
    fi
    verify_backup_sha "$bk"
    # r4 item 3: the INSTALLED union never deletes a PRE-INSTALL file - an entry the OLDEST
    # backup's SHA256SUMS lists pre-existed the first install (e.g. deleted by its owner, then
    # recreated by a later ensure repair) and is SKIPPED; the WROTE_OVER restore puts the
    # owner's pre-install bytes back if install overwrote it.
    local ln rel
    UW_KINDS=() UW_RESTORE=() UW_RELS=() UW_DIRS=()
    for ln in ${REC_INST[@]+"${REC_INST[@]}"}; do
        [ -n "$ln" ] || continue
        rel="${ln%% *}"
        if rec_pre_first "$rel"; then
            say "  (INSTALLED: $rel pre-existed the first install - NOT deleted; the WROTE_OVER restore puts the owner's bytes back if install overwrote it)"
        else
            UW_RELS+=("$ln")
        fi
    done
    # r4 item 1 + r5 item S1: the RESTORE list = the WROTE_OVER union over EVERY backup of
    # this tree, NOT "every file the backup's SHA256SUMS lists": files that existed but that
    # we did NOT write are never touched (a foreign file removed by its owner stays removed).
    # The union is emitted per PATH (first-appearance order), that path's entries NEWEST
    # backup first, as TRIPLETS (rel, sha-of-what-we-wrote, backup dir) - the inner script
    # walks that chain down from the newest entry matching the CURRENT sha (r5 item S1: the
    # oldest entry does not describe our LAST write when a rebuild made ensure reinstall).
    local -a wro_paths=()
    local handled=" " j=0
    while [ "$j" -lt "${#REC_WRO[@]}" ]; do
        rel="${REC_WRO[$j]}"
        case "$handled" in
            *" $rel "*) ;;
            *) handled="$handled$rel "; wro_paths+=("$rel") ;;
        esac
        j=$((j+3))
    done
    uw_restore_triplets ${wro_paths[@]+"${wro_paths[@]}"}
    # the gameinfo-era kinds the backups recorded (a linux tree installed by the old script)
    local kind
    for kind in ${REC_KINDS[@]+"${REC_KINDS[@]}"}; do
        case "$kind" in
            "gameinfo:Game csgo/addons/metamod"|"gameinfo:Write csgo") UW_KINDS+=("$kind") ;;
            *) say "  (unknown EDITS_WROTE line ignored: $kind)" ;;
        esac
    done
    UW_DIRS=("${LN_DIRS[@]}")
    linux_unwind uninstall "$bk"
    # retire this cycle's backups (same contract as wine: see the comment in cmd_uninstall_wine)
    mkdir -p "$BACKUP_ROOT/retired"
    local b
    while IFS= read -r b; do
        if [ -n "$b" ] && mv "$b" "$BACKUP_ROOT/retired/" 2>/dev/null; then
            say "  retired backup: ${b##*/}"
        else
            say "  WARN: could not retire backup ${b##*/} (mv failed) - it stays in place; uninstall of the NEXT cycle restores from the oldest backup that matches this tree"
        fi
    done < <(backups_for_app)
    say "uninstall complete (linux: restore from the oldest backup + remove what install added)"
}

cmd_status_linux() {
    linux_game_root
    say "linux game: $LINUX_G  [$([ -n "$LINUX_GAME" ] && echo 'CS2_LINUX_GAME override (test tree)' || echo "volume $LINUX_VNAME")]"
    # r3 NIT: the .so line also says which build dir the installed lib matches (this checkout's
    # build + CS2_MCP_DIR's)
    say_build_checkout
    local out rc=0
    out="$(lrun "$LINUX_G" "$RS_LIB" "$ROOT/target/bridge-rs/linuxsteamrt64/libcs2_bridge.so" "$MCP_DIR/target/bridge-rs/linuxsteamrt64/libcs2_bridge.so" \
                "$MMLOAD_SO" "$ROOT/target/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so" "$MCP_DIR/target/bridge-rs/linuxsteamrt64/libcs2mcp_mmload.so" <<'EOF'
set -euo pipefail
G="$1"
[ -d "$G" ] || { echo "(whole game dir $G absent)"; exit 0; }
if [ -d "$G/csgo/addons/metamod" ]; then
    echo "metamod dir csgo/addons/metamod  present ($(find "$G/csgo/addons/metamod" -type f | wc -l) files)"
else
    echo "metamod dir csgo/addons/metamod  absent"
fi
for rel in csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so csgo/addons/metamod/cs2mcp.vdf; do
    if [ -f "$G/$rel" ]; then
        sha="$(sha256sum "$G/$rel" | awk '{print $1}')"
        match=""
        matched_dirs=""
        cands=()
        case "$rel" in
            */libcs2_bridge.so)    cands=("$2" "$3" "$4") ;;
            */libcs2mcp_mmload.so) cands=("$5" "$6" "$7") ;;
        esac
        if [ "${#cands[@]}" -gt 0 ]; then
            # r3 NIT: which build dir the installed lib's bytes come from
            match="  matches no build here"
            for c in "${cands[@]}"; do
                [ -f "$c" ] || continue
                [ "$(sha256sum "$c" | awk '{print $1}')" = "$sha" ] || continue
                case " $matched_dirs " in *" $c "*) continue ;; esac
                matched_dirs="$matched_dirs $c"
                case "$match" in
                    "  matches no build here") match="  matches build $c" ;;
                    *) match="$match and $c" ;;
                esac
            done
        fi
        echo "$rel  present  sha256=$sha$match"
    else
        echo "$rel  absent"
    fi
done
EOF
)" || rc=$?
    printf '%s\n' "$out"
    [ "$rc" = 0 ] || die "linux status: reading the tree failed (rc $rc)"
    # gameinfo is not ours to look at any more, except for the OLD layout's leftovers (an old
    # install's loader vdfs + gameinfo lines)
    records_load
    linux_old_scan
    if [ "$(mig_count)" = 0 ]; then
        say "old layout none (gameinfo untouched, no csgo/addons/metamod.vdf / metamod_x64.vdf)"
    else
        local l
        while IFS= read -r l; do say "old layout $l"; done < <(mig_lines)
    fi
}

# --- ensure ------------------------------------------------------------------------------------
# One line per artefact, then: all ok -> exit 0 (nothing touched); else refuse while busy
# (exit 4) or repair by running install (which backs up first, as install always does).

ensure_report() {  # ensure_report <state ok|missing|stale:> <detail>   ("stale:" carries the why)
    say "  $1  $2"
}

# --- r3 item 6 + r4 item 2: "current" also requires the Valve-owned anchor ------------------------
# r4 item 2: the anchor comparison is NORMALISED - strip \r, collapse whitespace runs to one
# space, ignore blank lines and // comment lines. "damaged" is reported ONLY when the anchor
# install needs (the manifest's hammer entry) is missing - everything else reads "stale" and is
# repaired. (gameinfo.gi is no longer read at all: its anchors, closing brace and the
# `Game csgo`/`Game core` fingerprints went with the gameinfo edits.)
anchors_missing() {  # anchors_missing <file> <kind> <oldest-bk-or-empty>: prints the FIRST
                     # missing anchor's normalised text (rc 1), or nothing (rc 0) when every
                     # anchor of <kind> recorded in the backup's ANCHORS (the fingerprint taken
                     # at install time; defaults = what install's edit function anchors on) is
                     # still a normalised line of <file>. ANCHORS lines of other kinds (the
                     # gameinfo ones an old install recorded) are ignored.
    local f="$1" kind="$2" bks="$3"
    local -a wants=()
    if [ -n "$bks" ] && [ -f "$bks/ANCHORS" ]; then
        local a want
        while IFS=$'\t' read -r a want; do
            if [ "$a" = "$kind" ] && [ -n "$want" ]; then wants+=("$want"); fi
        done < "$bks/ANCHORS"
    fi
    if [ "${#wants[@]}" -eq 0 ]; then
        case "$kind" in
            manifest) wants=('m_Name = "hammer"') ;;
        esac
    fi
    python3 -I - "$f" ${wants[@]+"${wants[@]}"} <<'PY'
import re, sys
f, wants = sys.argv[1], sys.argv[2:]
def norm(s):
    return re.sub(r"[ \t]+", " ", s.replace("\r", "").strip())
lines = [norm(ln) for ln in open(f, encoding="utf-8", errors="replace")]
lines = [ln for ln in lines if ln and not ln.startswith("//")]
for w in map(norm, wants):
    if w not in lines:
        print(w)
        sys.exit(1)
sys.exit(0)
PY
}

ensure_repair() {  # ensure_repair <issues 0|1> <fatal 0|1> <repair-fn-name>; FATAL_MSG explains a fatal
    if [ "$2" = 1 ]; then   # fatal FIRST: a fatal with no other issues is not "all current"
        say "ensure: cannot repair - ${FATAL_MSG:-build the artefact first (bridge/build-rs.sh)}"
        exit 1
    fi
    if [ "$1" = 0 ]; then
        say "ensure: all artefacts current - nothing to do (exit 0, nothing touched)"
        return 0
    fi
    if [ "$DRY" = 1 ]; then
        say "ensure: dry-run - the artefacts above are NOT repaired (--dry-run touches nothing)"
        exit 3   # documented: --dry-run ensure exits 3 when stale (0 current, 1 fatal)
    fi
    if guard_needed; then
        local busy rc=0
        busy="$(busy_probe)" || rc=$?
        if [ "$rc" != 0 ]; then
            die "podman failed while probing running containers (rc $rc) - cannot check whether the tree is safe to repair (never guessing that nothing is running)"
        fi
        if [ -n "$busy" ]; then
            say "these running containers mount the tree:"
            while IFS= read -r n; do [ -n "$n" ] && say "  - $n"; done <<<"$busy"
            say "and the artefacts listed above are missing/stale - refusing to repair while they mount the tree"
            exit 4
        fi
    fi
    say "ensure: installing to repair (backup first, as install always does)"
    "$3"
}

ensure_artefact() {  # ensure_artefact <label> <app-relative-dest> <build-path>: a copied artefact
                     # is current when byte-equal to the build WHEN THE BUILD EXISTS; no build +
                     # an installed copy = current; no build + nothing installed = fatal.
                     # Updates the caller's `issues` / `fatal` / FATAL_MSG (dynamic scope).
    local label="$1" rel="$2" build="$3"
    if [ -f "$APP/$rel" ]; then
        if [ -f "$build" ]; then
            if cmp -s "$APP/$rel" "$build"; then
                ensure_report ok "$label $rel"
            else
                ensure_report "stale:" "$label $rel differs from the built $build"
                issues=1
            fi
        else
            ensure_report ok "$label $rel (no build to compare: current by definition)"
        fi
    else
        if [ -f "$build" ]; then
            ensure_report missing "$label $rel"
        else
            ensure_report missing "$label $rel ($build is not built either - BUILD IT FIRST: bridge/build-rs.sh)"
            fatal=1
            FATAL_MSG="build the artefact first (bridge/build-rs.sh)"
        fi
        issues=1
    fi
}

cmd_ensure_wine() {
    local issues=0 fatal=0 bks
    FATAL_MSG=""
    bks="$(oldest_backup)"   # the ANCHORS fingerprint source (may be empty - defaults then)
    say "ensure (wine): reading artefacts (read-only; no container guard needed)"
    ensure_artefact "tool DLL" game/bin/win64/tools/cs2_bridge.dll "$RS_LIB"
    ensure_artefact "launcher exe" game/bin/win64/cs2mcp_launch.exe "$LAUNCH_EXE"
    ensure_artefact "mmload DLL" game/bin/win64/cs2mcp_mmload.dll "$LOAD_DLL"
    # item 11: "current" = the file is what install's edit function would produce from it
    # (run the edit on a copy and cmp) - a truncated file with our line still in it reads stale.
    # A file with NO anchors is fatal with "game file damaged" - not "build it first".
    # (The compare copies live under target/ - registered in TEMPS - on BOTH platforms since
    # r3 item 1: ensure's READ-ONLY check must not need write access to the game tree at all.)
    if [ -f "$MAN" ]; then
        mktemp_under_target install-rs-man
        local mt="$TMP_PATH" mrc=0 mmiss="" mamrc=0
        TEMPS+=("$mt")
        mmiss="$(anchors_missing "$MAN" manifest "$bks")" || mamrc=$?
        apply_manifest_edit "$MAN" "$mt" || mrc=$?
        if [ "$mamrc" != 0 ]; then
            # r3 item 6 + r4 item 2: our entry may still be in the file - but the Valve-owned
            # anchor entry install relies on (the hammer entry, fingerprinted in the backup's
            # ANCHORS) is gone: damaged. The message names the anchor and is actionable.
            ensure_report "fatal:" "manifest game/bin/sdkenginetools.txt - missing anchor '$mmiss'"
            fatal=1
            FATAL_MSG="game file damaged: $MAN - missing anchor '$mmiss' (verify the game files; if Valve changed this line, update install-rs.sh's anchors (apply_manifest_edit))"
        elif [ "$mrc" = 3 ]; then
            ensure_report "fatal:" "manifest game/bin/sdkenginetools.txt - no 'hammer' anchor to insert before"
            fatal=1
            FATAL_MSG="game file damaged: $MAN - missing anchor 'm_Name = \"hammer\"' (verify the game files; if Valve changed this line, update install-rs.sh's anchors (apply_manifest_edit))"
        elif cmp -s "$mt" "$MAN"; then
            ensure_report ok "manifest game/bin/sdkenginetools.txt (cs2_bridge entry)"
        else
            ensure_report "stale:" "manifest game/bin/sdkenginetools.txt is not what install's edit would produce (entry missing or the file truncated?)"
            issues=1
        fi
        rm -f "$mt"
    else
        ensure_report "fatal:" "manifest game/bin/sdkenginetools.txt (whole file absent)"
        fatal=1
        FATAL_MSG="game file damaged: $MAN (verify the game files)"
        issues=1
    fi
    local b a
    for b in server.dll metamod.2.cs2.dll; do
        a=game/csgo/addons/metamod/bin/win64/$b
        if [ -f "$APP/$a" ]; then
            if [ -f "$MM_BUILD/metamod/bin/win64/$b" ] && cmp -s "$APP/$a" "$MM_BUILD/metamod/bin/win64/$b"; then
                ensure_report ok "metamod $a"
            else
                ensure_report "stale:" "metamod $a differs from the metamod build"
                issues=1
            fi
        else
            ensure_report missing "metamod $a"
            issues=1
        fi
    done
    a=game/csgo/addons/metamod/metaplugins.ini
    if [ -f "$APP/$a" ]; then
        ensure_report ok "metamod $a (present; NOT ours - content never compared)"
    else
        ensure_report missing "metamod $a"
        issues=1
    fi
    a=game/csgo/addons/metamod/cs2mcp.vdf
    if [ -f "$APP/$a" ]; then
        if cmp -s <(wine_vdf_text) "$APP/$a"; then
            ensure_report ok "metamod $a"
        else
            ensure_report "stale:" "metamod $a differs from the expected vdf ($(wine_vdf_text | sed -n 's/^\t"file".*/file ../p' | cut -c9-) - see the header)"
            issues=1
        fi
    else
        ensure_report missing "metamod $a"
        issues=1
    fi
    # the OLD (gameinfo-era) layout is "stale": the repair (install) migrates it away, or
    # refuses when it cannot attribute an artefact to an install of ours (migrate_wine)
    local l
    records_load
    wine_old_scan
    if [ "$(mig_count)" != 0 ]; then
        while IFS= read -r l; do ensure_report "stale:" "old layout: $l"; done < <(mig_lines)
        issues=1
    else
        ensure_report ok "no old (gameinfo-era) layout (gameinfo untouched, no game/metamod, no content/metamod)"
    fi
    ensure_repair "$issues" "$fatal" cmd_install_wine
}

cmd_ensure_linux() {
    linux_game_root
    say "ensure (linux): reading artefacts (read-only; no container guard needed)"
    # the tree-side check runs inside the tree's namespace; HAS_LIB / HAS_MM tell it whether the
    # built .so files exist (the "current" definition depends on it). Output lines are byte
    # messages, rc: 0 all ok / 9 problems, repairable / 2 fatal ("build it first").
    local has=0 hasmm=0; [ -f "$RS_LIB" ] && has=1; [ -f "$MMLOAD_SO" ] && hasmm=1
    local vdfsrc
    linux_vdf_file
    vdfsrc="$VDF_TMP"
    local out rc=0
    out="$(lrun "$LINUX_G" "$MM_BUILD" "$RS_LIB" "$has" "$MMLOAD_SO" "$hasmm" "$vdfsrc" <<'EOF'
set -euo pipefail
G="$1"; MM="$2"; SO="$3"; HAS_LIB="$4"; MMSO="$5"; HAS_MM="$6"; VDF="$7"
[ -d "$G" ] || { echo "missing  (whole game dir $G absent - install needs it; if a .so below is missing too, repair is fatal)"; exit 9; }
python3 - "$G" "$MM" "$SO" "$HAS_LIB" "$MMSO" "$HAS_MM" "$VDF" <<'PY'
import hashlib, os, sys

G, MM, SO, HAS_LIB, MMSO, HAS_MM, VDF = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5], int(sys.argv[6]), sys.argv[7]
issues = 0; fatal = 0
def report(state, what, why=""):
    print("%s  %s%s" % (state, what, (("  [" + why + "]") if why else "")))
def once(f):
    h = hashlib.sha256()
    with open(f, "rb") as fh:
        h.update(fh.read())
    return h.hexdigest()
def eq(a, b):
    return once(a) == once(b)

def lib(rel, build, has, what):
    global issues, fatal
    f = os.path.join(G, rel)
    if os.path.isfile(f):
        if has == 0:
            report("ok", what + " " + rel, "no built copy to compare: current by definition")
        elif eq(f, build):
            report("ok", what + " " + rel)
        else:
            report("stale:", what + " " + rel, "differs from the built " + build)
            issues += 1
    else:
        report("missing", what + " " + rel, "" if has else "no built copy either - BUILD IT FIRST (bridge/build-rs.sh)")
        issues += 1
        if has == 0:
            fatal += 1

lib("csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so", SO, HAS_LIB, ".so")
lib("csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so", MMSO, HAS_MM, "mmload .so")

METADIR = os.path.join(G, "csgo/addons/metamod")
SRC = os.path.join(MM, "metamod")
for root, dirs, files in os.walk(SRC):
    for fn in sorted(files):
        s = os.path.join(root, fn)
        g = os.path.join(METADIR, os.path.relpath(s, SRC))
        what = "metamod csgo/addons/metamod/%s" % os.path.relpath(s, SRC)
        if fn == "metaplugins.ini" and root == SRC:
            # metaplugins.ini is METAMOD's config, not ours (item 6): presence only - we load
            # through csgo/addons/metamod/cs2mcp.vdf, so its content is never compared
            if os.path.isfile(g):
                report("ok", what, "present; NOT ours - content never compared")
            else:
                report("missing", what)
                issues += 1
            continue
        if not os.path.isfile(g):
            report("missing", what)
            issues += 1
        elif eq(s, g):
            report("ok", what)
        else:
            report("stale:", what, "differs from the metamod build")
            issues += 1

f = os.path.join(G, "csgo/addons/metamod/cs2mcp.vdf")
if not os.path.isfile(f):
    report("missing", "csgo/addons/metamod/cs2mcp.vdf")
    issues += 1
# full-byte compare against the SAME file install copies (linux_vdf_file) - the trailing
# newline counts (round-2 blocker: a $(...)-stripped string made this never "current")
elif open(f, "rb").read() == open(VDF, "rb").read():
    report("ok", "csgo/addons/metamod/cs2mcp.vdf")
else:
    report("stale:", "csgo/addons/metamod/cs2mcp.vdf", "differs from the expected vdf")
    issues += 1
sys.exit(2 if fatal else (9 if issues else 0))
PY
EOF
)" || rc=$?
    local line
    while IFS= read -r line; do [ -n "$line" ] && say "$line"; done <<<"$out"
    # the OLD (gameinfo-era) layout is "stale": the repair (install) migrates it away, or
    # refuses when it cannot attribute an artefact to an install of ours (migrate_linux)
    records_load
    linux_old_scan
    if [ "$(mig_count)" != 0 ]; then
        while IFS= read -r line; do ensure_report "stale:" "old layout: $line"; done < <(mig_lines)
        [ "$rc" = 0 ] && rc=9
    else
        ensure_report ok "no old (gameinfo-era) layout (gameinfo untouched, no csgo/addons/metamod.vdf / metamod_x64.vdf)"
    fi
    case "$rc" in
        0) ensure_repair 0 0 "" ;;
        2)
            # a fatal: "build it first" (or any FATALMSG line the tree-side check printed)
            FATAL_MSG="$(printf '%s\n' "$out" | sed -n 's/^FATALMSG: //p' | head -1)"
            ensure_repair 1 1 "" ;;
        9) ensure_repair 1 0 cmd_install_linux ;;
        *) die "ensure: reading the tree failed (rc $rc)" ;;
    esac
}

# --- dispatch ------------------------------------------------------------------------------------

case "$CMD" in
    install)   if [ "$PLATFORM" = wine ]; then cmd_install_wine; else cmd_install_linux;   fi ;;
    uninstall) if [ "$PLATFORM" = wine ]; then cmd_uninstall_wine; else cmd_uninstall_linux; fi ;;
    status)    if [ "$PLATFORM" = wine ]; then cmd_status_wine;    else cmd_status_linux;    fi ;;
    ensure)    if [ "$PLATFORM" = wine ]; then cmd_ensure_wine;    else cmd_ensure_linux;    fi ;;
esac
