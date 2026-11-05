#!/bin/bash
# test_install_rs.sh - HOST test for bridge/install-rs.sh (KHOOK-C1). Self-contained: builds
# fake trees, a stub podman and tiny fake DLL/SO artefacts under target/fake/install-rs-test/
# (never the real game trees: <project root>/workshop/run/app and the cs2-home
# volume are shared with other sessions and are only ever touched by read-only `status` runs).
#
# The gameinfo-free layouts (mm-launch lane L2): install adds files (and, wine, the one
# sdkenginetools.txt entry) and NEVER touches gameinfo.gi - every case that installs asserts the
# fixture gameinfo is still byte-identical. An OLD (gameinfo-era) tree is built by running the
# PREVIOUS script (git show dc63443:bridge/install-rs.sh - the last gameinfo-era version, pinned)
# against the fake tree, so the migration is tested against REAL old backups and layouts.
#
# Cases (each prints PASS/FAIL; the script exits non-zero on any FAIL):
#   wine install on a fake tree: exact file set (standard game/csgo/addons/metamod layout, the
#     launcher exe + mmload dll, no game/metamod, no content/metamod, no addons/cs2mcp), vdf bytes,
#     gameinfo UNTOUCHED, manifest entry; re-install is a byte-identical no-op; uninstall restores
#     cmp-identical and removes only added files/dirs;
#   wine uninstall of a LEGACY install (old script's second copy + dirs): removed;
#   linux install/uninstall on a fake tree (stub `podman unshare` = run the args): exact file
#     set (bridge + mmload .so, no loader vdfs), vdf, gameinfo untouched, backup has APP_TREE +
#     SHA256SUMS + an empty EDITS_WROTE, uninstall cmp-identical;
#   ensure: fresh tree -> installs; all-current -> exit 0 and the tree's sha snapshot is
#     unchanged; an installed file gone stale -> repaired;
#     busy (stub podman reports a container mounting the tree, GUARD_FAKE=1) + current -> exit 0
#     untouched; busy + stale -> exit 4 with the container name, untouched;
#   MIGRATION (both platforms): a tree installed by the OLD script is ensure-stale ("old
#     layout"), install/ensure migrates it (our gameinfo lines, game/metamod, content/metamod,
#     the loader vdfs - exactly what the old backups record as ours), lays down the new set,
#     leaves the owner's files and unrecorded gameinfo lines alone, is idempotent, refuses (tree
#     untouched) an old artefact no backup attributes to us, survives a kill mid-migration, and
#     uninstall round-trips the tree byte-exact from the OLD backups;
#   NEGATIVE (mandatory): stub podman whose `ps` fails -> install / ensure-mutate refuse
#     (non-zero, tree untouched); a missing metamod build file -> preflight refuses before any
#     change (tree sha snapshot unchanged). Both platforms.
#
# Toolchain: the little fake DLL is linked with clang-cl + lld-link (/NODEFAULTLIB, single
# __declspec(dllexport) -> the REAL bridge/check-exports.sh accepts it), the fake .so with
# gcc -shared; the real export gate runs on every install. If those tools are missing, setup
# FAILS loudly naming them (no skip-if-missing).
#
# The stub podman contract (see make_stub): `ps -q` (optionally `--filter volume=cs2-home`),
# `inspect --format '{{.Name}}'`, `inspect --format '{{json .Mounts}}'`,
# `volume inspect <vol> --format '{{.Mountpoint}}'`, and `unshare bash -s ... <heredoc` ==
# plain `bash -s ...` (so the whole --platform linux flow runs on a plain directory).
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIX="$ROOT/bridge/testdata/install-rs"
W="$ROOT/target/fake/install-rs-test"
NAME="cs2mm-spike-cl"          # the busy container the stub fabricates
PASSED=0 FAILED=0

say()  { printf 'test_install_rs: %s\n' "$*"; }
pass() { PASSED=$((PASSED+1)); printf 'PASS %s\n' "$1"; }
fail() { FAILED=$((FAILED+1)); printf 'FAIL %s\n' "$1"; printf 'FAIL ... %s\n' "${2:-}"; }
note() { printf '  note: %s\n' "$1"; }
exit_if_setup_failed() {   # loud, never a skip
    if [ -n "$SETUP_ERROR" ]; then fail "setup: $SETUP_ERROR"; exit 1; fi
}

# ----------------------------------------------------------------- setup ----------------
# a previously SIGKILLed run can leave read-only dirs (case_ensure_readonly_tree): make the
# wipe immune before rm -rf, so one killed run never poisons the next
[ -d "$W" ] && find "$W" -type d -exec chmod u+w {} + 2>/dev/null
rm -rf "$W"
mkdir -p "$W/bin" "$W/builds" "$W/common" "$W/podman-xdg"
SETUP_ERROR=""

# 1. the tiny fake DLL/SO, gated by the REAL check-exports.sh
GLANG="$(command -v gcc || true)"
CLANG_CL="$(command -v clang-cl || true)"
LLD_LINK="$(command -v lld-link || true)"
[ -n "$GLANG" ] || SETUP_ERROR="gcc not found (needed to build the fake .so)"
[ -n "$CLANG_CL" ] || SETUP_ERROR="clang-cl not found (needed to build the fake .dll)"
[ -n "$LLD_LINK" ] || SETUP_ERROR="lld-link not found (needed to build the fake .dll)"
exit_if_setup_failed
mkdir -p "$W/src"
cat > "$W/src/fake.c" <<'EOF'
__declspec(dllexport) void *CreateInterface(const char *n, int *r) { (void)n; (void)r; return 0; }
EOF
cat > "$W/src/fake-so.c" <<'EOF'
__attribute__((visibility("default"))) void *CreateInterface(const char *n, int *r) { (void)n; (void)r; return 0; }
EOF
"$CLANG_CL" /c "$W/src/fake.c" /Fo:"$W/src/fake.obj" >"$W/src/dll.log" 2>&1 \
    || { SETUP_ERROR="clang-cl failed on the fake.c (see $W/src/dll.log)"; exit_if_setup_failed; }
"$LLD_LINK" /DLL /OUT:"$W/bin/fake.dll" /NOENTRY /NODEFAULTLIB "$W/src/fake.obj" >>"$W/src/dll.log" 2>&1 \
    || { SETUP_ERROR="lld-link failed on the fake.obj (see $W/src/dll.log)"; exit_if_setup_failed; }
"$GLANG" -shared -fPIC -o "$W/bin/fake.so" "$W/src/fake-so.c" >"$W/src/so.log" 2>&1 \
    || { SETUP_ERROR="gcc failed on the fake-so.c (see $W/src/so.log)"; exit_if_setup_failed; }
bash "$ROOT/bridge/check-exports.sh" "$W/bin/fake.dll" >"$W/src/gate-dll.log" 2>&1 \
    || { SETUP_ERROR="real check-exports.sh refused the fake .dll (see $W/src/gate-dll.log)"; exit_if_setup_failed; }
bash "$ROOT/bridge/check-exports.sh" "$W/bin/fake.so" >"$W/src/gate-so.log" 2>&1 \
    || { SETUP_ERROR="real check-exports.sh refused the fake .so (see $W/src/gate-so.log)"; exit_if_setup_failed; }
say "fakes built + gated: $W/bin/fake.dll $W/bin/fake.so (real check-exports.sh: OK)"
# the launcher artefacts install copies verbatim (NOT export-gated): distinct stub bytes each
printf 'stub cs2mcp_launch.exe\n' > "$W/bin/launch.exe"
printf 'stub cs2mcp_mmload.dll\n' > "$W/bin/mmload.dll"
printf 'stub libcs2mcp_mmload.so\n' > "$W/bin/mmload.so"
# the last gameinfo-era install-rs.sh, pinned: builds the OLD layouts the migration cases start from.
# It runs from its own mini-checkout (ROOT = $W/oldscript; its check-exports.sh next to it).
OLD_REF=dc63443
mkdir -p "$W/oldscript/bridge"
git -C "$ROOT" show "$OLD_REF:bridge/install-rs.sh" > "$W/oldscript/bridge/install-rs.sh" 2>"$W/src/oldscript.log" \
    || { SETUP_ERROR="git show $OLD_REF:bridge/install-rs.sh failed (see $W/src/oldscript.log): the migration cases need the gameinfo-era script"; exit_if_setup_failed; }
cp "$ROOT/bridge/check-exports.sh" "$W/oldscript/bridge/check-exports.sh"

# 2. mock metamod builds (install copies blindly; only RS_LIB is export-gated)
mkdir -p "$W/builds/mmwin/metamod/bin/win64"
cp "$W/bin/fake.dll" "$W/builds/mmwin/metamod/bin/win64/server.dll"
cp "$W/bin/fake.dll" "$W/builds/mmwin/metamod/bin/win64/metamod.2.cs2.dll"
printf 'mm Plugins Load\n' > "$W/builds/mmwin/metamod/metaplugins.ini"
mkdir -p "$W/builds/mmlinux/metamod/bin/linux64" "$W/builds/mmlinux/metamod/bin/linuxsteamrt64"
cp "$W/bin/fake.so" "$W/builds/mmlinux/metamod/bin/linux64/server.so"
cp "$W/bin/fake.so" "$W/builds/mmlinux/metamod/bin/linuxsteamrt64/libserver.so"
cp "$W/bin/fake.so" "$W/builds/mmlinux/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
printf 'mm Plugins Load\n' > "$W/builds/mmlinux/metamod/metaplugins.ini"
printf 'Metamod:Source 2.0-dev (linux build)\n' > "$W/builds/mmlinux/metamod/README.txt"
printf '"file" "../game/bin/win64/server.dll"\n' > "$W/builds/mmlinux/metamod.vdf"
printf '"file" "../game/bin/win64/server.dll"\n' > "$W/builds/mmlinux/metamod_x64.vdf"

# 3. per-case env
env_reset() {
    unset CS2_GAME CS2_LINUX_GAME CS2_MCP_DIR PODMAN PODMAN_XDG GUARD_FAKE \
          CS2_MM CS2_MM_LINUX CS2_BRIDGE_DLL CS2_BRIDGE_LINUX_SO CS2DOCKER_ROOT CS2_MCP_PROJECT_ROOT \
          CS2_MM_EXE CS2_MM_DLL CS2_MMLOAD_SO \
          INSTALL_RS_LOCK_WAIT INSTALL_RS_FAULT INSTALL_RS_REAL_APP || true
}
mk_env() {  # mk_env <tree-root> <wine|linux> [GUARD_FAKE=1]
    env_reset
    export CS2_MCP_DIR="$W" PODMAN="$W/bin/podman" PODMAN_XDG="$W/podman-xdg"
    # the install lock lives under the fake root in tests (CS2DOCKER_ROOT: the name the pinned
    # gameinfo-era script reads; CS2_MCP_PROJECT_ROOT: the current one, which wins over it)
    export CS2DOCKER_ROOT="$W/cs2docker" CS2_MCP_PROJECT_ROOT="$W/cs2docker"
    if [ "$2" = wine ]; then
        export CS2_GAME="$1/game" CS2_MM="$W/builds/mmwin" CS2_BRIDGE_DLL="$W/bin/fake.dll"
        export CS2_MM_EXE="$W/bin/launch.exe" CS2_MM_DLL="$W/bin/mmload.dll"
    else
        export CS2_LINUX_GAME="$1/game" CS2_MM_LINUX="$W/builds/mmlinux" CS2_BRIDGE_LINUX_SO="$W/bin/fake.so"
        export CS2_MMLOAD_SO="$W/bin/mmload.so"
    fi
    [ -n "${3:-}" ] && export GUARD_FAKE="$3"
    return 0
}
rs() {  # rs <args...> -> stdout+stderr captured in RS_OUT, rc in RS_RC
    RS_OUT="$(bash "$ROOT/bridge/install-rs.sh" "$@" 2>&1)"
    RS_RC=$?
}
old_rs() {  # old_rs <args...>: the PREVIOUS (gameinfo-era) script against the same env (it shares the
            # fake backup root, so the new script sees its backups)
    RS_OUT="$(bash "$W/oldscript/bridge/install-rs.sh" "$@" 2>&1)"
    RS_RC=$?
}

# 4. the stub podman
make_stub() {  # make_stub <mode none|busy|ps-fail> <bind-source-path>
    local mode="$1" src="${2:-}" vmp
    vmp="$W/common/cs2-home"
    {
        echo '#!/bin/bash'
        echo "MODE='$mode'"
        echo "NAME='$NAME'"
        echo "SRC='$src'"
        echo "VMP='$vmp'"
        cat <<'STUB'
cmd="${1:-}"; shift
case "$cmd" in
  ps)
    if [ "$MODE" = ps-fail ]; then echo "stub podman: ps fails" >&2; exit 1; fi
    if [ "$MODE" = busy ]; then echo "container_id_one"; fi
    ;;
  inspect)
    fmt="$2"; id="$3"   # (after the shift: --format fmt id)
    case "$fmt" in
      '{{.Name}}') echo "$NAME" ;;
      '{{json .Mounts}}')
          # wine: an unrelated named volume FIRST (the matcher must ignore it), then the tree bind
          if [ -n "$SRC" ]; then
              printf '[{"Type":"volume","Name":"cs2-home","Destination":"/home/steam"},{"Type":"bind","Source":"%s","Destination":"/app"}]\n' "$SRC"
          else
              echo '[]'
          fi ;;
      *) echo "stub: unsupported format: $fmt" >&2; exit 1 ;;
    esac ;;
  volume)
    case "$1/$2/$4" in   # (after the shift: inspect <vol> --format fmt)
      inspect/*/'{{.Mountpoint}}') echo "$VMP" ;;
      *) echo "stub: unsupported volume call: $*" >&2; exit 1 ;;
    esac ;;
  unshare)
    exec "$@"   # the test's stand-in for `podman unshare`: drop "unshare", run "$@" in place
    ;;
  *) echo "stub: unsupported: $cmd" >&2; exit 2 ;;
esac
STUB
    } > "$W/bin/podman"
    chmod +x "$W/bin/podman"
}

# 5. tree builders + helpers
fresh_wine() {  # fresh_wine <tree-root>: build/bin + csgo dirs from the CRLF fixture
    # content/csgo: Valve's content root exists in every real tools tree, and content/ is never
    # ours to remove (an empty dir, so the file-set checks below are unaffected)
    mkdir -p "$1/game/bin" "$1/game/csgo" "$1/content/csgo"
    cp "$FIX/sdkenginetools.txt" "$1/game/bin/sdkenginetools.txt"
    cp "$FIX/gameinfo.gi" "$1/game/csgo/gameinfo.gi"
}
fresh_linux() {  # fresh_linux <tree-root>: csgo/gameinfo.gi (+ one unrelated pre-existing file
                 # to prove uninstall keeps what it does not own)
    local lf="${2:-}"
    mkdir -p "$1/game/csgo/addons"
    if [ "$lf" = lf ]; then cp "$FIX/gameinfo-lf.gi" "$1/game/csgo/gameinfo.gi"; else cp "$FIX/gameinfo.gi" "$1/game/csgo/gameinfo.gi"; fi
    printf 'unrelated pre-existing content\n' > "$1/game/csgo/addons/unrelated-stays.txt"
}
snap() {  # snap <tree-root> -> "sha  path" lines, sorted (whole-tree byte snapshot)
    (cd "$1" && find . -type f -exec sha256sum {} + | sort)
}
expect_files() {  # expect_files <tree-root> <label> file1 file2 ... -> pass/fail on the exact set
    local tree="$1" label="$2" want have
    shift 2
    want="$(printf '%s\n' "$@" | sed 's|^|./|' | sort)"
    have="$(cd "$tree" && find . -type f | sort)"
    if [ "$have" = "$want" ]; then pass "$label"; else
        fail "$label" "file set differs"; diff <(printf '%s\n' "$want") <(printf '%s\n' "$have") | sed 's/^/FAIL     /'
    fi
}
absent_paths() {  # absent_paths <tree-root> <label> path1 ... : none of the paths may exist
    local tree="$1" label="$2" bad=0 p
    shift 2
    for p in "$@"; do
        if [ -e "$tree/$p" ]; then bad=1; note "unexpectedly present: $p"; fi
    done
    if [ "$bad" = 0 ]; then pass "$label"; else fail "$label" "expected all of the paths above to be absent"; fi
}
cmp_file() {  # cmp_file <a> <b> <label> <maybe-note>
    if cmp -s "$1" "$2"; then pass "$3"; else fail "$3" "differs"; cmp "$1" "$2" | head -2 | sed 's/^/FAIL     /' ; [ -n "${4:-}" ] && note "$4"; return 0; fi
}
newest_backup_dir() {  # newest_backup_dir <APP_TREE-value>: the matching backup dir or empty
    # (the installers stamp APP_TREE with the tree root; find the newest dir carrying it)
    local wanted="$1" d hit=""
    for d in $(find "$W/target/bridge" -maxdepth 1 -name 'backup-*' 2>/dev/null | sort); do
        [ -f "$d/APP_TREE" ] && [ "$(cat "$d/APP_TREE")" = "$wanted" ] && hit="$d"
    done
    [ -n "$hit" ] && printf '%s' "$hit"
    return 0
}
summary_exit() {
    if [ "$FAILED" -ne 0 ]; then
        printf 'RESULT: FAIL (%d passed, %d failed)\n' "$PASSED" "$FAILED"
        exit 1
    fi
    printf 'RESULT: ALL PASS (%d passed, %d failed)\n' "$PASSED" "$FAILED"
    exit 0
}

# =========================================================================== cases =========
case_wine_install_reinstall_uninstall() {
    local T="$W/trees/wine-main"
    fresh_wine "$T"
    rm -rf "$W/common/wine.pristine"
    cp -a "$T" "$W/common/wine.pristine"      # the PRE-install tree (uninstall's reference)
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "wine install rc" "rc=$RS_RC output:$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    pass "wine install rc=0"
    # NB: the tree root here is $T (the app dir) - content/ is a sibling of game/, like the real tree
    expect_files "$T" "wine install file set (standard addons layout + launcher; no addons/cs2mcp)" \
        game/bin/win64/tools/cs2_bridge.dll game/bin/sdkenginetools.txt game/csgo/gameinfo.gi \
        game/bin/win64/cs2mcp_launch.exe game/bin/win64/cs2mcp_mmload.dll \
        game/csgo/addons/metamod/bin/win64/server.dll game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll \
        game/csgo/addons/metamod/metaplugins.ini game/csgo/addons/metamod/cs2mcp.vdf
    absent_paths "$T" "wine install leaves NO second copy / no cs2mcp dirs / no old -tools layout" \
        game/csgo/addons/cs2mcp/bin/win64/cs2_bridge.dll game/csgo/addons/cs2mcp game/metamod content/metamod
    printf '"Metamod Plugin"\n{\n\t"alias"\t\t"cs2mcp"\n\t"file"\t\t"../bin/win64/tools/cs2_bridge.dll"\n}\n' \
        > "$W/common/wine.vdf.expect"
    cmp_file "$W/common/wine.vdf.expect" "$T/game/csgo/addons/metamod/cs2mcp.vdf" \
        "wine vdf bytes point at ../bin/win64/tools/cs2_bridge.dll"
    if [ "$(sha256sum "$T/game/bin/win64/tools/cs2_bridge.dll" | awk '{print $1}')" = "$(sha256sum "$W/bin/fake.dll" | awk '{print $1}')" ]; then
        pass "wine tool DLL is the gated artefact"
    else
        fail "wine tool DLL sha"
    fi
    if grep -q 'm_Name = "cs2_bridge"' "$T/game/bin/sdkenginetools.txt" \
       && grep -q 'm_Library = "tools/cs2_bridge.dll"' "$T/game/bin/sdkenginetools.txt"; then
        pass "wine manifest entry"
    else
        fail "wine manifest entry"
    fi
    cmp_file "$FIX/gameinfo.gi" "$T/game/csgo/gameinfo.gi" "wine gameinfo.gi is byte-identical to the pristine fixture (install never touches it)"
    cmp_file "$W/bin/launch.exe" "$T/game/bin/win64/cs2mcp_launch.exe" "wine launcher exe is the build artefact"
    cmp_file "$W/bin/mmload.dll" "$T/game/bin/win64/cs2mcp_mmload.dll" "wine mmload dll is the build artefact"
    cmp_file "$W/builds/mmwin/metamod/bin/win64/server.dll" "$T/game/csgo/addons/metamod/bin/win64/server.dll" \
        "wine Metamod loader lands in the standard game/csgo/addons/metamod/bin/win64/"

    # round-2 blocker regression, wine side: install must leave EVERY artefact ensure-current
    # (r3 NIT: with the same mtime + backup-count assertions the linux case has)
    local bkcount
    bkcount="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-rs-*' 2>/dev/null | wc -l)"
    (cd "$T" && find game -type f -exec stat -c '%n %Y %s' {} +) | sort > "$W/common/ensw.mtime.before"
    rs ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "wine install -> ensure reports all artefacts current"
    else
        fail "wine install -> ensure all-current" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    if [ "$(find "$W/target/bridge" -maxdepth 1 -name 'backup-rs-*' 2>/dev/null | wc -l)" = "$bkcount" ]; then
        pass "wine ensure all-current creates NO new backup"
    else
        fail "wine ensure all-current created a backup dir (it must touch nothing)"
    fi
    (cd "$T" && find game -type f -exec stat -c '%n %Y %s' {} +) | sort > "$W/common/ensw.mtime.after"
    cmp_file "$W/common/ensw.mtime.before" "$W/common/ensw.mtime.after" "wine ensure all-current: our files byte+mtime identical"

    # re-install: byte-identical no-op
    snap "$T/game" > "$W/common/snap.before"
    rs install
    [ "$RS_RC" = 0 ] || { fail "wine re-install rc"; return 0; }
    snap "$T/game" > "$W/common/snap.after"
    cmp_file "$W/common/snap.before" "$W/common/snap.after" "wine re-install is a byte-identical no-op"

    # uninstall: the reference is the pre-install copy taken just after fresh_wine
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "wine uninstall rc"; return 0; }
    if diff -r "$W/common/wine.pristine" "$T" >/dev/null 2>&1; then
        pass "wine uninstall restores the pre-install tree cmp-identically"
    else
        fail "wine uninstall restore" "diff -r pristine vs tree:"
        diff -r "$W/common/wine.pristine" "$T" | head -10 | sed 's/^/FAIL     /'
    fi
    absent_paths "$T" "wine uninstall removed install's dirs" \
        game/metamod content/metamod game/csgo/addons/metamod game/csgo/addons game/bin/win64/tools game/bin/win64
}

case_wine_uninstall_legacy() {
    local T="$W/trees/wine-legacy"
    fresh_wine "$T"
    mkdir -p "$T/game/csgo/addons/cs2mcp/bin/win64"
    printf 'legacy second copy from an OLD install\n' > "$T/game/csgo/addons/cs2mcp/bin/win64/cs2_bridge.dll"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "wine legacy install rc" "rc=$RS_RC"; return 0; }
    pass "wine legacy install rc=0 (legacy copy tolerated)"
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "wine legacy uninstall rc"; return 0; }
    absent_paths "$T/game" "wine uninstall removes the LEGACY second copy" \
        game/csgo/addons/cs2mcp/bin/win64/cs2_bridge.dll
    absent_paths "$T/game" "wine uninstall removed the LEGACY dirs" \
        game/csgo/addons/cs2mcp
}

case_linux_install_uninstall() {
    local T="$W/trees/linux-main"
    fresh_linux "$T" crlf
    rm -rf "$W/common/linux.pristine"
    cp -a "$T" "$W/common/linux.pristine"     # the PRE-install tree (uninstall's reference)
    make_stub none
    mk_env "$T" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "linux install rc" "rc=$RS_RC output:$(printf '%s' "$RS_OUT" | tail -5)"; return 0; }
    pass "linux install rc=0"
    expect_files "$T/game" "linux install file set (bridge + mmload .so, NO loader vdfs)" \
        csgo/gameinfo.gi csgo/addons/unrelated-stays.txt \
        csgo/addons/metamod/bin/linux64/server.so csgo/addons/metamod/bin/linuxsteamrt64/libserver.so \
        csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so csgo/addons/metamod/metaplugins.ini \
        csgo/addons/metamod/README.txt csgo/addons/metamod/cs2mcp.vdf \
        csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so
    printf '"Metamod Plugin"\n{\n\t"alias"\t"cs2mcp"\n\t"file"\t"addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so"\n}\n' \
        > "$W/common/linux.vdf.expect"
    cmp_file "$W/common/linux.vdf.expect" "$T/game/csgo/addons/metamod/cs2mcp.vdf" "linux cs2mcp.vdf bytes"
    if [ "$(sha256sum "$T/game/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so" | awk '{print $1}')" = "$(sha256sum "$W/bin/fake.so" | awk '{print $1}')" ]; then
        pass "linux .so is the gated artefact"
    else
        fail "linux .so sha"
    fi
    cmp_file "$FIX/gameinfo.gi" "$T/game/csgo/gameinfo.gi" "linux gameinfo.gi is byte-identical to the pristine fixture (install never touches it)"
    cmp_file "$W/bin/mmload.so" "$T/game/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so" "linux mmload .so is the build artefact"
    # round-2 blocker regression, linux side: install must leave EVERY artefact ensure-current
    rs --platform linux ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "linux install -> ensure reports all artefacts current"
    else
        fail "linux install -> ensure all-current" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    # backup contract
    local bk; bk="$(newest_backup_dir "$T/game")"
    if [ -n "$bk" ] && [ -f "$bk/APP_TREE" ] && [ -f "$bk/SHA256SUMS" ] && [ -f "$bk/EDITS_WROTE" ] && [ ! -s "$bk/EDITS_WROTE" ] \
       && [ -f "$bk/INSTALLED" ] && [ ! -e "$bk/csgo/gameinfo.gi" ]; then
        pass "linux backup has APP_TREE + SHA256SUMS + an EMPTY EDITS_WROTE (no edit; gameinfo not backed up)"
    else
        fail "linux backup contents" "bk='$bk'"
    fi

    # LF fixture tree: gameinfo still untouched (no CR introduced, nothing added)
    local T2="$W/trees/linux-lf"
    fresh_linux "$T2" lf
    mk_env "$T2" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "linux lf install rc"; return 0; }
    cmp_file "$FIX/gameinfo-lf.gi" "$T2/game/csgo/gameinfo.gi" "linux LF gameinfo.gi untouched"

    # uninstall -> cmp-identical to pristine (incl. the unrelated file)
    mk_env "$T" linux
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "linux uninstall rc" "rc=$RS_RC"; return 0; }
    if diff -r "$W/common/linux.pristine" "$T" >/dev/null 2>&1; then
        pass "linux uninstall restores cmp-identically (unrelated file kept)"
    else
        fail "linux uninstall restore"
        diff -r "$W/common/linux.pristine" "$T" | head -10 | sed 's/^/FAIL     /'
    fi
    absent_paths "$T/game" "linux uninstall removed install's dirs (the pre-existing csgo/addons stays: unrelated-stays.txt)" \
        csgo/addons/metamod csgo/addons/cs2mcp
}

case_ensure() {
    # fresh tree -> ensure installs
    local T="$W/trees/ensure-main"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "ensure fresh rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if printf '%s' "$RS_OUT" | grep -q 'installing to repair' && [ -f "$T/game/bin/win64/tools/cs2_bridge.dll" ]; then
        pass "ensure on a fresh tree runs install"
    else
        fail "ensure on a fresh tree"
    fi
    # all-current -> exit 0, nothing touched
    snap "$T/game" > "$W/common/ensure.snap.before"
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "ensure all-current rc"; return 0; }
    snap "$T/game" > "$W/common/ensure.snap.after"
    cmp_file "$W/common/ensure.snap.before" "$W/common/ensure.snap.after" "ensure all-current touches nothing"
    printf '%s' "$RS_OUT" | grep -q 'all artefacts current' || fail "ensure all-current message"

    # simulated Steam update: sdkenginetools.txt rewritten (the fixture without our entry) -
    # the one remaining game-file edit; gameinfo.gi is not involved any more
    cp "$FIX/sdkenginetools.txt" "$T/game/bin/sdkenginetools.txt"
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "ensure stale rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if grep -q 'm_Name = "cs2_bridge"' "$T/game/bin/sdkenginetools.txt"; then
        pass "ensure re-applies the manifest entry"
    else
        fail "ensure re-applies the manifest entry"
    fi
    printf '%s' "$RS_OUT" | grep -q "stale:" || fail "ensure reports stale lines"
    cmp_file "$FIX/gameinfo.gi" "$T/game/csgo/gameinfo.gi" "ensure never touches gameinfo.gi"

    # busy + current -> exit 0 untouched (GUARD_FAKE=1 + stub reports a container mounting the tree)
    make_stub busy "$T/app"
    mk_env "$T" wine 1
    snap "$T/game" > "$W/common/ensure.busy.snap"
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "ensure busy+current rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    snap "$T/game" > "$W/common/ensure.busy.snap.after"
    cmp_file "$W/common/ensure.busy.snap" "$W/common/ensure.busy.snap.after" "ensure busy+current leaves the tree untouched"

    # busy + stale -> exit 4, container named, untouched
    cp "$FIX/sdkenginetools.txt" "$T/game/bin/sdkenginetools.txt"
    snap "$T/game" > "$W/common/ensure.busy.stale.before"
    rs ensure
    if [ "$RS_RC" = 4 ]; then
        pass "ensure busy+stale exits 4"
    else
        fail "ensure busy+stale rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    if printf '%s' "$RS_OUT" | grep -q "$NAME"; then
        pass "ensure busy+stale names the container ($NAME)"
    else
        fail "ensure busy+stale names the container"
    fi
    snap "$T/game" > "$W/common/ensure.busy.stale.snap"
    cmp_file "$W/common/ensure.busy.stale.before" "$W/common/ensure.busy.stale.snap" "ensure busy+stale leaves the tree untouched"
}

case_negative() {
    # stub podman whose ps FAILS -> install refuses (wine); tree untouched
    local T="$W/trees/neg-wine"
    fresh_wine "$T"
    make_stub ps-fail
    mk_env "$T" wine 1
    snap "$T/game" > "$W/common/neg.snap"
    rs install
    if [ "$RS_RC" != 0 ]; then
        pass "wine install refuses when podman ps fails (rc=$RS_RC)"
    else
        fail "wine install refuses when podman ps fails" "rc=0"
    fi
    snap "$T/game" > "$W/common/neg.snap.after"
    cmp_file "$W/common/neg.snap" "$W/common/neg.snap.after" "wine ps-fail install touches nothing"
    # ... and ensure's MUTATE path refuses too (make an issue so the repair path is reached)
    rs ensure
    if [ "$RS_RC" != 0 ]; then
        pass "wine ensure refuses (mutate) when podman ps fails (rc=$RS_RC)"
    else
        fail "wine ensure refuses when podman ps fails" "rc=0"
    fi
    snap "$T/game" > "$W/common/neg2.snap.after"
    cmp_file "$W/common/neg.snap" "$W/common/neg2.snap.after" "wine ps-fail ensure touches nothing"

    # linux: ps fails + even a repair would be needed -> refused without touching the tree
    local TL="$W/trees/neg-linux"
    fresh_linux "$TL" crlf
    make_stub ps-fail
    mk_env "$TL" linux 1
    snap "$TL/game" > "$W/common/negl.snap"
    rs --platform linux install
    if [ "$RS_RC" != 0 ]; then
        pass "linux install refuses when podman ps fails (rc=$RS_RC)"
    else
        fail "linux install refuses when podman ps fails"
    fi
    snap "$TL/game" > "$W/common/negl.snap.after"
    cmp_file "$W/common/negl.snap" "$W/common/negl.snap.after" "linux ps-fail install touches nothing"

    # linux ensure with podman failing (round-2 review: the missing test): a stale tree needs
    # the repair path, whose probe must refuse (exit 1: podman failure, NOT busy 4); the fresh
    # tree has nothing installed, so ensure is stale by itself
    rs --platform linux ensure
    if [ "$RS_RC" = 1 ]; then
        pass "linux ensure with a stale tree refuses when podman ps fails (rc=1, names the failure)"
    else
        fail "linux ensure with a stale tree refuses when podman ps fails" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    snap "$TL/game" > "$W/common/negl2.snap.after"
    cmp_file "$W/common/negl.snap" "$W/common/negl2.snap.after" "linux ps-fail ensure touches nothing"

    # missing metamod build file -> preflight BEFORE any change (both platforms)
    make_stub none
    local TW="$W/trees/neg-wine-mm"
    fresh_wine "$TW"
    mv "$W/builds/mmwin/metamod/metaplugins.ini" "$W/common/mmwin.metaplugins.ini.bak"
    mk_env "$TW" wine
    snap "$TW/game" > "$W/common/negmm.snap"
    rs install
    [ "$RS_RC" != 0 ] && pass "wine preflight refuses on a missing metamod build file (rc=$RS_RC)" \
        || fail "wine preflight refuses on a missing metamod build file" "rc=$RS_RC"
    snap "$TW/game" > "$W/common/negmm.snap.after"
    cmp_file "$W/common/negmm.snap" "$W/common/negmm.snap.after" "wine mm-missing install touches nothing"
    mv "$W/common/mmwin.metaplugins.ini.bak" "$W/builds/mmwin/metamod/metaplugins.ini"

    local TLL="$W/trees/neg-linux-mm"
    fresh_linux "$TLL" crlf
    mv "$W/builds/mmlinux/metamod/bin/linuxsteamrt64/libserver.so" "$W/common/mmlinux.libserver.bak"
    mk_env "$TLL" linux
    snap "$TLL/game" > "$W/common/negml.snap"
    rs --platform linux install
    [ "$RS_RC" != 0 ] && pass "linux preflight refuses on a missing metamod build file (rc=$RS_RC)" \
        || fail "linux preflight refuses on a missing metamod build file" "rc=$RS_RC"
    snap "$TLL/game" > "$W/common/negml.snap.after"
    cmp_file "$W/common/negml.snap" "$W/common/negml.snap.after" "linux mm-missing install touches nothing"
    mv "$W/common/mmlinux.libserver.bak" "$W/builds/mmlinux/metamod/bin/linuxsteamrt64/libserver.so"
    # a missing LAUNCHER artefact (nothing installed) refuses before any change, both platforms,
    # and says to run build-rs.sh
    mv "$W/bin/mmload.dll" "$W/bin/mmload.dll.hidden"
    mk_env "$TW" wine
    snap "$TW/game" > "$W/common/neglc.snap"
    rs install
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'BUILD IT FIRST (run bridge/build-rs.sh)'; then
        pass "wine preflight refuses on a missing mmload dll (rc=$RS_RC, says to run build-rs.sh)"
    else
        fail "wine preflight refuses on a missing mmload dll" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    snap "$TW/game" > "$W/common/neglc.snap.after"
    cmp_file "$W/common/neglc.snap" "$W/common/neglc.snap.after" "wine mmload-missing install touches nothing"
    mv "$W/bin/mmload.dll.hidden" "$W/bin/mmload.dll"
    mv "$W/bin/mmload.so" "$W/bin/mmload.so.hidden"
    mk_env "$TLL" linux
    snap "$TLL/game" > "$W/common/neglc2.snap"
    rs --platform linux install
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'BUILD IT FIRST (run bridge/build-rs.sh'; then
        pass "linux preflight refuses on a missing mmload .so (rc=$RS_RC, says to run build-rs.sh)"
    else
        fail "linux preflight refuses on a missing mmload .so" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    snap "$TLL/game" > "$W/common/neglc2.snap.after"
    cmp_file "$W/common/neglc2.snap" "$W/common/neglc2.snap.after" "linux mmload-missing install touches nothing"
    mv "$W/bin/mmload.so.hidden" "$W/bin/mmload.so"
}

case_ensure_linux() {
    # linux ensure: fresh -> installs; all-current -> 0 untouched
    local T="$W/trees/ensure-linux"
    fresh_linux "$T" crlf
    make_stub none
    mk_env "$T" linux
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "linux ensure fresh rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"; return 0; }
    pass "linux ensure on a fresh tree runs install"
    # BLOCKER regression: install's vdf writer and ensure's producer shared no bytes before
    # (a $(...)-stripped trailing newline), so linux ensure NEVER read all-current. After a
    # plain install, ensure must report every artefact current WITHOUT touching anything:
    # no new backup dir, and our files' mtimes unchanged (not even a rewritten copy).
    local bkcount
    bkcount="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-linux-*' 2>/dev/null | wc -l)"
    # mtime+sha snapshot of every file install created
    (cd "$T/game" && find csgo -type f -exec stat -c '%n %Y %s' {} + ) | sort > "$W/common/ensl.mtime.before"
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "linux ensure all-current rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"; return 0; }
    printf '%s' "$RS_OUT" | grep -q 'all artefacts current' || fail "linux ensure all-current message"
    if [ "$(find "$W/target/bridge" -maxdepth 1 -name 'backup-linux-*' 2>/dev/null | wc -l)" = "$bkcount" ]; then
        pass "linux ensure all-current creates NO new backup"
    else
        fail "linux ensure all-current created a backup dir (it must touch nothing)"
    fi
    (cd "$T/game" && find csgo -type f -exec stat -c '%n %Y %s' {} + ) | sort > "$W/common/ensl.mtime.after"
    cmp_file "$W/common/ensl.mtime.before" "$W/common/ensl.mtime.after" "linux ensure all-current: our files byte+mtime identical"
    snap "$T/game" > "$W/common/ensl.snap"
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "linux ensure all-current rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"; return 0; }
    snap "$T/game" > "$W/common/ensl2.snap"
    cmp_file "$W/common/ensl.snap" "$W/common/ensl2.snap" "linux ensure all-current touches nothing"
    # busy + CURRENT -> exit 0 untouched (round-2 review: the missing linux busy+current case)
    make_stub busy "$W/common/cs2-home"
    mk_env "$T" linux 1
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "linux ensure busy+current rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"; return 0; }
    snap "$T/game" > "$W/common/ensl.busyc.after"
    cmp_file "$W/common/ensl.snap" "$W/common/ensl.busyc.after" "linux ensure busy+current touches nothing"
    printf '%s' "$RS_OUT" | grep -q 'all artefacts current' || fail "linux ensure busy+current message"
    # busy + stale (linux guard via the volume rule; GUARD_FAKE=1): the mmload .so went stale
    printf 'stale\n' >> "$T/game/csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so"
    snap "$T/game" > "$W/common/ensl.busy"
    rs --platform linux ensure
    if [ "$RS_RC" = 4 ]; then
        pass "linux ensure busy+stale exits 4"
    else
        fail "linux ensure busy+stale rc" "rc=$RS_RC"
    fi
    if printf '%s' "$RS_OUT" | grep -q "$NAME"; then
        pass "linux ensure busy+stale names the container ($NAME)"
    else
        fail "linux ensure busy+stale names the container"
    fi
    snap "$T/game" > "$W/common/ensl.busy2"
    cmp_file "$W/common/ensl.busy" "$W/common/ensl.busy2" "linux ensure busy+stale touches nothing"
}

case_fault_hook() {
    # ITEM 2 (NEG): a kill BETWEEN temp write and rename must leave the original file intact,
    # no temp behind (the trap), and the next ensure repairs. INSTALL_RS_FAULT=<point> is a
    # test-only hook in install-rs.sh that exits 99 at that point.
    local T="$W/trees/fault-wine"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    local out
    out="$(INSTALL_RS_FAULT=pre-mv:game/bin/sdkenginetools.txt bash "$ROOT/bridge/install-rs.sh" install 2>&1)"
    local rc=$?
    if [ "$rc" = 99 ]; then
        pass "wine install fault hook fires (rc 99)"
    else
        fail "wine install fault hook rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    fi
    cmp_file "$FIX/sdkenginetools.txt" "$T/game/bin/sdkenginetools.txt" "wine fault: original sdkenginetools.txt intact"
    local leftovers
    leftovers="$(find "$T" -name '*.tmp.*' -type f)"
    if [ -z "$leftovers" ]; then
        pass "wine fault: no temp file left behind"
    else
        fail "wine fault left temps" "$leftovers"
    fi
    rs ensure
    [ "$RS_RC" = 0 ] || fail "wine fault: next ensure repairs (rc=$RS_RC)"
    rs ensure   # and the run AFTER the repair must read every artefact current
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "wine fault: next ensure repairs (all current again)"
    else
        fail "wine fault: next ensure repairs" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi

    # linux: the same fault at the vdf rename inside the unshare'd script (a linux install edits
    # no existing file any more, so the dest must simply not exist yet)
    local TL="$W/trees/fault-linux"
    fresh_linux "$TL" crlf
    mk_env "$TL" linux
    out="$(INSTALL_RS_FAULT=pre-mv:csgo/addons/metamod/cs2mcp.vdf bash "$ROOT/bridge/install-rs.sh" --platform linux install 2>&1)"
    rc=$?
    if [ "$rc" = 99 ]; then
        pass "linux install fault hook fires (rc 99)"
    else
        fail "linux install fault hook rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    fi
    if [ ! -e "$TL/game/csgo/addons/metamod/cs2mcp.vdf" ]; then
        pass "linux fault: the killed rename left no half-written vdf"
    else
        fail "linux fault: the vdf exists although its rename was killed"
    fi
    cmp_file "$FIX/gameinfo.gi" "$TL/game/csgo/gameinfo.gi" "linux fault: gameinfo.gi intact"
    leftovers="$(find "$TL" -name '*.tmp.*' -type f)"
    if [ -z "$leftovers" ]; then
        pass "linux fault: no temp file left behind"
    else
        fail "linux fault left temps" "$leftovers"
    fi
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || fail "linux fault: next ensure repairs (rc=$RS_RC)"
    rs --platform linux ensure   # and the run AFTER the repair must read every artefact current
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "linux fault: next ensure repairs (all current again)"
    else
        fail "linux fault: next ensure repairs" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi

    # ITEM 3 (NEG): a kill MID-BACKUP leaves NO usable backup (the .partial is trap-removed,
    # never finalised) and the next install run works from scratch.
    local TB="$W/trees/fault-backup"
    fresh_wine "$TB"
    make_stub none
    mk_env "$TB" wine
    local bks_before bks_after
    bks_before="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-rs-*' | wc -l)"
    out="$(INSTALL_RS_FAULT=mid-backup bash "$ROOT/bridge/install-rs.sh" install 2>&1)"
    rc=$?
    if [ "$rc" = 99 ]; then
        pass "wine mid-backup fault hook fires (rc 99)"
    else
        fail "wine mid-backup fault hook rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    fi
    bks_after="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-rs-*' | wc -l)"
    if [ "$bks_after" = "$bks_before" ]; then
        pass "wine mid-backup kill leaves NO backup (final or .partial)"
    else
        fail "wine mid-backup kill left a backup dir" "before=$bks_before after=$bks_after"
    fi
    find "$W/target/bridge" -maxdepth 1 -name '*.partial' | grep -q . \
        && fail "wine mid-backup left a .partial dir" || pass "wine mid-backup left no .partial dir"
    rs install
    [ "$RS_RC" = 0 ] && pass "wine mid-backup: next install run works" \
        || fail "wine mid-backup: next install run works" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    rs uninstall
    [ "$RS_RC" = 0 ] && pass "wine mid-backup: uninstall on the recovered install works" \
        || fail "wine mid-backup: uninstall after recovery" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"

    # linux: the same mid-backup fault (the backup is built on the host side, out of the volume)
    local TBL="$W/trees/fault-backup-linux"
    fresh_linux "$TBL" crlf
    mk_env "$TBL" linux
    bks_before="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-linux-*' | wc -l)"
    out="$(INSTALL_RS_FAULT=mid-backup bash "$ROOT/bridge/install-rs.sh" --platform linux install 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "linux mid-backup fault hook fires (rc 99)" \
        || fail "linux mid-backup fault hook rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    bks_after="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-linux-*' | wc -l)"
    [ "$bks_after" = "$bks_before" ] && pass "linux mid-backup kill leaves NO backup" \
        || fail "linux mid-backup kill left a backup dir" "before=$bks_before after=$bks_after"
    rs --platform linux install
    [ "$RS_RC" = 0 ] && pass "linux mid-backup: next install run works" \
        || fail "linux mid-backup: next install run works" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"

    # INSTALLED/EDITS_WROTE are written: a fresh-tree install's backup records exactly what
    # install created (item 7's deletion list); a linux install edits no game file, so its
    # EDITS_WROTE is empty
    local bk; bk="$(newest_backup_dir "$TBL/game")"
    if [ -n "$bk" ] && [ -f "$bk/INSTALLED" ] && [ -f "$bk/EDITS_WROTE" ]; then
        if grep -q 'csgo/addons/metamod/cs2mcp.vdf' "$bk/INSTALLED" \
           && grep -q 'csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so' "$bk/INSTALLED" \
           && ! grep -q 'unrelated-stays' "$bk/INSTALLED" \
           && [ ! -s "$bk/EDITS_WROTE" ]; then
            pass "linux backup records INSTALLED (created only) + an empty EDITS_WROTE"
        else
            fail "linux backup list contents" "$(cat "$bk/INSTALLED" 2>/dev/null | tr '\n' ' ') / $(cat "$bk/EDITS_WROTE" 2>/dev/null | tr '\n' ' ')"
        fi
    else
        fail "linux backup has INSTALLED + EDITS_WROTE" "bk='$bk'"
    fi
}

case_uninstall_revert() {
    # ITEM 4: uninstall reverts EDITS in the CURRENT file instead of restoring stale backup
    # copies: a Steam update may rewrite sdkenginetools.txt after install - those changes survive.
    # (The gameinfo edits are gone with the gameinfo path; their revert lives in the migration.)
    local T="$W/trees/revert-wine"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "revert-wine install rc" "rc=$RS_RC"; return 0; }
    # simulated Steam update: a modification install did NOT make (a new entry before the hammer one)
    python3 - "$T/game/bin/sdkenginetools.txt" <<'PY'
import sys
p = sys.argv[1]
t = open(p, encoding="utf-8", newline="").read()
anchor = '\t\t{\n\t\t\tm_Name = "hammer"'
assert anchor in t
t = t.replace(anchor, '\t\t{\n\t\t\tm_Name = "steam_update_tool"\n\t\t},\n' + anchor, 1)
open(p, "w", encoding="utf-8", newline="").write(t)
PY
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "revert-wine uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if grep -q 'm_Name = "steam_update_tool"' "$T/game/bin/sdkenginetools.txt"; then
        pass "wine uninstall keeps the Steam-update modification"
    else
        fail "wine uninstall keeps the Steam-update modification"
    fi
    if ! grep -q 'm_Name = "cs2_bridge"' "$T/game/bin/sdkenginetools.txt"; then
        pass "wine uninstall removes exactly our manifest entry"
    else
        fail "wine uninstall removes exactly our manifest entry"
    fi
    cmp_file "$FIX/gameinfo.gi" "$T/game/csgo/gameinfo.gi" "wine uninstall leaves gameinfo.gi alone too"
    # NEG: a manifest that ALREADY carried our entry before the first install -> install records no
    # manifest edit, uninstall leaves the entry (we did not write it)
    local TP="$W/trees/revert-wine-preentry"
    mkdir -p "$TP/game/bin" "$TP/game/csgo"
    cp "$FIX/gameinfo.gi" "$TP/game/csgo/gameinfo.gi"
    python3 - "$FIX/sdkenginetools.txt" "$TP/game/bin/sdkenginetools.txt" <<'PY'
import sys
t = open(sys.argv[1], encoding="utf-8", newline="").read()
anchor = '\t\t{\n\t\t\tm_Name = "hammer"'
entry = ('\t\t{\n\t\t\tm_Name = "cs2_bridge"\n\t\t\tm_FriendlyName = "CS2 Bridge (Rust)"\n'
         '\t\t\tm_Library = "tools/cs2_bridge.dll"\n\t\t\tm_bShowInToolsMenu = false\n\t\t},\n')
open(sys.argv[2], "w", encoding="utf-8", newline="").write(t.replace(anchor, entry + anchor, 1))
PY
    cp "$TP/game/bin/sdkenginetools.txt" "$W/common/preentry.manifest"
    mk_env "$TP" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "preentry install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "preentry uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file "$W/common/preentry.manifest" "$TP/game/bin/sdkenginetools.txt" \
        "NEG: the pre-existing cs2_bridge manifest entry survives uninstall (not recorded as ours)"
}

case_uninstall_installed_list() {
    # ITEM 7: deletion is driven by the backup's recorded INSTALLED list - not by the metamod
    # build dir: remove the build, uninstall still removes every installed metamod file.
    local T="$W/trees/installed-list"
    fresh_linux "$T" crlf
    make_stub none
    mk_env "$T" linux
    # use a PRIVATE copy of the linux metamod build (the case deletes it; the shared build
    # under builds/ must stay intact for the later negative cases)
    rm -rf "$W/builds/mmlinux-copy"
    cp -a "$W/builds/mmlinux" "$W/builds/mmlinux-copy"
    export CS2_MM_LINUX="$W/builds/mmlinux-copy"
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "installed-list install rc" "rc=$RS_RC"; return 0; }
    rm -rf "$W/builds/mmlinux-copy"   # the metamod build disappears (a different checkout, etc.)
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "linux uninstall without the metamod build rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"; return 0; }
    if diff -r "$W/common/linux.pristine" "$T" >/dev/null 2>&1; then
        pass "linux uninstall (build dir gone) still removes every installed metamod file"
    else
        fail "linux uninstall (build dir gone)" "diff -r pristine vs tree:"
        diff -r "$W/common/linux.pristine" "$T" | head -8 | sed 's/^/FAIL     /'
    fi
    # NEG: a missing INSTALLED list refuses (never "uninstall complete" with nothing removed)
    local TN="$W/trees/installed-list-missing"
    fresh_wine "$TN"
    mk_env "$TN" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "installed-missing install rc" "rc=$RS_RC"; return 0; }
    local bk; bk="$(newest_backup_dir "$TN")"
    [ -n "$bk" ] || { fail "installed-missing: no backup found"; return 0; }
    mv "$bk/INSTALLED" "$W/common/INSTALLED.bak"
    snap "$TN/game" > "$W/common/instl.snap"
    rs uninstall
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'INSTALLED'; then
        pass "NEG: uninstall refuses without an INSTALLED list (rc=$RS_RC, message names it)"
    else
        fail "NEG: uninstall refuses without an INSTALLED list" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    snap "$TN/game" > "$W/common/instl.snap.after"
    cmp_file "$W/common/instl.snap" "$W/common/instl.snap.after" "NEG: missing-INSTALLED uninstall touches nothing"
    mv "$W/common/INSTALLED.bak" "$bk/INSTALLED"
}

case_ensure_no_build() {
    # ITEM 5: with the bridge installed and NO build artefact, ensure still repairs config
    # (the installed DLL/SO counts as current). NEG: nothing installed + no build -> exit 1
    # "build it first".
    local T="$W/trees/ensure-nobuild"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "ensure-nobuild install rc" "rc=$RS_RC"; return 0; }
    mv "$W/bin/fake.dll" "$W/bin/fake.dll.hidden"   # the build artefacts disappear
    mv "$W/bin/launch.exe" "$W/bin/launch.exe.hidden"
    mv "$W/bin/mmload.dll" "$W/bin/mmload.dll.hidden"
    cp "$FIX/sdkenginetools.txt" "$T/game/bin/sdkenginetools.txt"   # simulated Steam update: our entry gone
    snap "$T/game/bin/win64" > "$W/common/nobuild.bins.before"
    rs ensure
    snap "$T/game/bin/win64" > "$W/common/nobuild.bins.after"
    if [ "$RS_RC" = 0 ] && [ -f "$T/game/bin/win64/tools/cs2_bridge.dll" ] \
       && grep -q 'm_Name = "cs2_bridge"' "$T/game/bin/sdkenginetools.txt"; then
        pass "wine ensure repairs config with no build (installed DLL/launcher untouched, entry re-applied)"
    else
        fail "wine ensure repairs config with no build" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    cmp_file "$W/common/nobuild.bins.before" "$W/common/nobuild.bins.after" "wine ensure with no build keeps the installed binaries byte-identical"
    mv "$W/bin/fake.dll.hidden" "$W/bin/fake.dll"   # restore for the other cases
    mv "$W/bin/launch.exe.hidden" "$W/bin/launch.exe"
    mv "$W/bin/mmload.dll.hidden" "$W/bin/mmload.dll"
    # NEG (wine): nothing installed + no build -> exit 1 "build it first"
    local TN="$W/trees/ensure-nobuild-neg"
    fresh_wine "$TN"
    make_stub none
    mk_env "$TN" wine
    mv "$W/bin/fake.dll" "$W/bin/fake.dll.hidden"
    rs ensure
    if [ "$RS_RC" = 1 ] && printf '%s' "$RS_OUT" | grep -q 'BUILD IT FIRST'; then
        pass "NEG: wine ensure without install and without build exits 1 (build it first)"
    else
        fail "NEG: wine ensure without install and without build" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    mv "$W/bin/fake.dll.hidden" "$W/bin/fake.dll"
    # NEG (wine): the tool DLL is built, the LAUNCHER is not and nothing is installed -> exit 1
    local TNL="$W/trees/ensure-nobuild-launcher"
    fresh_wine "$TNL"
    mk_env "$TNL" wine
    mv "$W/bin/launch.exe" "$W/bin/launch.exe.hidden"
    rs ensure
    if [ "$RS_RC" = 1 ] && printf '%s' "$RS_OUT" | grep -q 'BUILD IT FIRST'; then
        pass "NEG: wine ensure with no launcher build and none installed exits 1 (build it first)"
    else
        fail "NEG: wine ensure without a launcher build" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    mv "$W/bin/launch.exe.hidden" "$W/bin/launch.exe"
    # NEG (linux): same contract
    local TL="$W/trees/ensure-nobuild-linux"
    fresh_linux "$TL" crlf
    make_stub none
    mk_env "$TL" linux
    mv "$W/bin/fake.so" "$W/bin/fake.so.hidden"
    rs --platform linux ensure
    if [ "$RS_RC" = 1 ] && printf '%s' "$RS_OUT" | grep -q 'BUILD IT FIRST'; then
        pass "NEG: linux ensure without install and without build exits 1 (build it first)"
    else
        fail "NEG: linux ensure without install and without build" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    mv "$W/bin/fake.so.hidden" "$W/bin/fake.so"
    mv "$W/bin/mmload.so" "$W/bin/mmload.so.hidden"
    rs --platform linux ensure
    if [ "$RS_RC" = 1 ] && printf '%s' "$RS_OUT" | grep -q 'BUILD IT FIRST'; then
        pass "NEG: linux ensure with no mmload .so build and none installed exits 1 (build it first)"
    else
        fail "NEG: linux ensure without an mmload build" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    mv "$W/bin/mmload.so.hidden" "$W/bin/mmload.so"
}

case_metaplugins_not_ours() {
    # ITEM 6: a pre-existing metaplugins.ini (another plugin's line) survives install, ensure
    # (reported current) and uninstall. We load through addons/metamod/cs2mcp.vdf, so that
    # config is never ours to overwrite.
    local T="$W/trees/metaplugins"
    fresh_wine "$T"
    mkdir -p "$T/game/csgo/addons/metamod"
    printf 'plugin_other  ../bin/win64/someotherplugin.dll\n' > "$T/game/csgo/addons/metamod/metaplugins.ini"
    cp "$T/game/csgo/addons/metamod/metaplugins.ini" "$W/common/mp.before"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "metaplugins install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file "$W/common/mp.before" "$T/game/csgo/addons/metamod/metaplugins.ini" "wine install keeps the pre-existing metaplugins.ini byte-for-byte"
    rs ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "wine ensure reports the foreign metaplugins.ini as current (presence only)"
    else
        fail "wine ensure foreign metaplugins.ini" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "metaplugins uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file "$W/common/mp.before" "$T/game/csgo/addons/metamod/metaplugins.ini" "wine uninstall keeps the foreign metaplugins.ini"
    # linux: same contract
    local TL="$W/trees/metaplugins-linux"
    fresh_linux "$TL" crlf
    mkdir -p "$TL/game/csgo/addons/metamod"
    printf 'plugin_other  ../bin/win64/someotherplugin.dll\n' > "$TL/game/csgo/addons/metamod/metaplugins.ini"
    cp "$TL/game/csgo/addons/metamod/metaplugins.ini" "$W/common/mpl.before"
    mk_env "$TL" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "metaplugins-linux install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file "$W/common/mpl.before" "$TL/game/csgo/addons/metamod/metaplugins.ini" "linux install keeps the pre-existing metaplugins.ini byte-for-byte"
    rs --platform linux ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "linux ensure reports the foreign metaplugins.ini as current (presence only)"
    else
        fail "linux ensure foreign metaplugins.ini" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "metaplugins-linux uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file "$W/common/mpl.before" "$TL/game/csgo/addons/metamod/metaplugins.ini" "linux uninstall keeps the foreign metaplugins.ini"
}

case_lock() {
    # ITEM 8: a second install-rs.sh waits (INSTALL_RS_LOCK_WAIT override), then exits 4 naming
    # the lock file; --dry-run takes the lock SHARED (coexists with another shared holder).
    local T="$W/trees/lock"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    mkdir -p "$W/cs2docker/.podman"
    rs install
    [ "$RS_RC" = 0 ] || { fail "lock: baseline install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    # hold the lock the way another install-rs.sh (another checkout, another session) would
    exec 9>>"$W/cs2docker/.podman/install-rs-wine.lock"
    flock -x -w 5 9 || { fail "lock: test cannot hold the lock"; return 0; }
    local out
    out="$(INSTALL_RS_LOCK_WAIT=1 bash "$ROOT/bridge/install-rs.sh" ensure 2>&1)"
    local rc=$?
    if [ "$rc" = 4 ] && printf '%s' "$out" | grep -q "another install-rs.sh holds .*install-rs-wine.lock"; then
        pass "lock: ensure exits 4 naming the lock file when another holder has it"
    else
        fail "lock: ensure against a held lock" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    fi
    # a SHARED holder does not block --dry-run ensure (shared): it reports on the current tree
    flock -s -w 5 9 || { fail "lock: test cannot downgrade to shared"; return 0; }
    out="$(INSTALL_RS_LOCK_WAIT=1 bash "$ROOT/bridge/install-rs.sh" --dry-run ensure 2>&1)"
    rc=$?
    if [ "$rc" = 0 ]; then
        pass "lock: --dry-run ensure takes the lock shared (rc 0 while another shared holder)"
    else
        fail "lock: --dry-run ensure shared" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    fi
    flock -u 9 2>/dev/null
    exec 9>&-
    rs ensure
    [ "$RS_RC" = 0 ] && pass "lock: ensure proceeds once the lock is free" \
        || fail "lock: ensure after release" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
}

case_realpath_guard() {
    # ITEM 10: the real-tree decision is made on realpath -m of both sides. The "real" root is
    # faked via INSTALL_RS_REAL_APP (test-only; the real trees are never touched) and the stub
    # podman reports a container mounting that root - so a matching spelling must REFUSE (rc 4)
    # and a genuinely different tree must SKIP the guard (install proceeds).
    local T="$W/trees/realpath"
    fresh_wine "$T"
    make_stub busy "$T/app"
    mk_env "$T" wine
    export INSTALL_RS_REAL_APP="$T/app"
    local spelling
    for spelling in "$T/app/game/" "$T/app/game/x/.." ; do   # trailing slash, ".." segments
        export CS2_GAME="$spelling"
        rs install
        if [ "$RS_RC" = 4 ] && printf '%s' "$RS_OUT" | grep -q "$NAME"; then
            pass "wine guard applies to the real tree spelled '$spelling' (rc 4, container named)"
        else
            fail "wine guard applies to the real tree spelled '$spelling'" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        fi
    done
    ln -sfn "$T/app" "$W/trees/link-app"
    export CS2_GAME="$W/trees/link-app/game"
    rs install
    if [ "$RS_RC" = 4 ]; then
        pass "wine guard applies to the real tree spelled via a symlink (rc 4)"
    else
        fail "wine guard applies to the real tree spelled via a symlink" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    # NEG: a DIFFERENT tree skips the guard -> install proceeds (rc 0)
    local TO="$W/trees/realpath-other"
    fresh_wine "$TO"
    export CS2_GAME="$TO/game"
    rs install
    if [ "$RS_RC" = 0 ]; then
        pass "NEG: wine install on a different tree skips the guard (rc 0)"
    else
        fail "NEG: wine install on a different tree skips the guard" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    # linux: the same decision against the stub volume's mountpoint
    local TL="$W/trees/realpath-linux"
    fresh_linux "$TL" crlf
    make_stub busy "$W/common/cs2-home"
    mk_env "$TL" linux
    export CS2_LINUX_GAME="$W/common/cs2-home/steamapps/common/Counter-Strike Global Offensive/game/"
    rs --platform linux install
    if [ "$RS_RC" = 4 ] && printf '%s' "$RS_OUT" | grep -q "$NAME"; then
        pass "linux guard applies to the volume game dir spelled with a trailing slash (rc 4)"
    else
        fail "linux guard applies to the volume game dir spelled with a trailing slash" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    ln -sfn "$W/common/cs2-home" "$W/trees/link-vol"
    export CS2_LINUX_GAME="$W/trees/link-vol/steamapps/common/Counter-Strike Global Offensive/game"
    rs --platform linux install
    if [ "$RS_RC" = 4 ]; then
        pass "linux guard applies to the volume game dir spelled via a symlink (rc 4)"
    else
        fail "linux guard applies to the volume game dir spelled via a symlink" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    # NEG: a different linux tree skips the guard
    local TLO="$W/trees/realpath-linux-other"
    fresh_linux "$TLO" crlf
    export CS2_LINUX_GAME="$TLO/game"
    rs --platform linux install
    if [ "$RS_RC" = 0 ]; then
        pass "NEG: linux install on a different tree skips the guard (rc 0)"
    else
        fail "NEG: linux install on a different tree skips the guard" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
}

case_content_compare() {
    # ITEM 11: "current" = the file is what install's edit would produce from it. Since the
    # gameinfo edits are gone the one edited file is sdkenginetools.txt: a manifest that lost
    # our entry (a Steam update restored it) reads STALE and is repaired; one with no 'hammer'
    # anchor at all is FATAL "game file damaged" (exit 1), not "build it first".
    # --dry-run ensure: exit 0 current, 3 stale, 1 fatal (documented).
    local T="$W/trees/truncated"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "truncated install rc" "rc=$RS_RC"; return 0; }
    cp "$FIX/sdkenginetools.txt" "$T/game/bin/sdkenginetools.txt"   # the Steam-restored file: no entry
    rs ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'is not what install'; then
        pass "wine manifest without our entry reads stale and is repaired"
    else
        fail "wine manifest without our entry reads stale" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    if grep -q 'm_Name = "cs2_bridge"' "$T/game/bin/sdkenginetools.txt"; then
        pass "wine stale manifest was repaired by the edit"
    else
        fail "wine stale manifest repaired"
    fi
    # NEG: no anchor at all (and no entry) -> fatal "game file damaged", not "build it first"
    local TN="$W/trees/damaged"
    fresh_wine "$TN"
    mk_env "$TN" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "damaged install rc" "rc=$RS_RC"; return 0; }
    printf '%s\n' '{' '}' > "$TN/game/bin/sdkenginetools.txt"
    rs ensure
    if [ "$RS_RC" = 1 ] && printf '%s' "$RS_OUT" | grep -q 'game file damaged'; then
        pass "NEG: wine manifest with no anchors -> exit 1 'game file damaged' (not 'build it first')"
    else
        fail "NEG: wine manifest with no anchors" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    # --dry-run ensure: exit 3 stale (and it must not repair)
    local T3="$W/trees/dryrun"
    fresh_wine "$T3"
    mk_env "$T3" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "dryrun install rc" "rc=$RS_RC"; return 0; }
    cp "$FIX/sdkenginetools.txt" "$T3/game/bin/sdkenginetools.txt"   # stale again
    snap "$T3/game" > "$W/common/dry.snap"
    rs --dry-run ensure
    local rc3=$RS_RC
    snap "$T3/game" > "$W/common/dry.snap.after"
    if [ "$rc3" = 3 ]; then
        pass "--dry-run ensure exits 3 when stale (documented; 3 was unused)"
    else
        fail "--dry-run ensure stale exit code" "rc=$rc3 out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    cmp_file "$W/common/dry.snap" "$W/common/dry.snap.after" "--dry-run ensure touches nothing"
    # linux: gameinfo.gi is not read at all - a MISSING or garbage gameinfo.gi is irrelevant
    local TL="$W/trees/nogameinfo-linux"
    fresh_linux "$TL" crlf
    mk_env "$TL" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "nogameinfo-linux install rc" "rc=$RS_RC"; return 0; }
    rm -f "$TL/game/csgo/gameinfo.gi"
    rs --platform linux ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "linux ensure ignores gameinfo.gi entirely (absent file still reads current)"
    else
        fail "linux ensure with no gameinfo.gi" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    # wine: the same
    local TW="$W/trees/nogameinfo-wine"
    fresh_wine "$TW"
    mk_env "$TW" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "nogameinfo-wine install rc" "rc=$RS_RC"; return 0; }
    printf 'garbage, not a gameinfo\n' > "$TW/game/csgo/gameinfo.gi"
    rs ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "wine ensure ignores gameinfo.gi entirely (garbage content still reads current)"
    else
        fail "wine ensure with a garbage gameinfo.gi" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
}

case_damaged_rare() {
    # r4 item 2: "damaged" is rare and actionable. The anchor comparison is NORMALISED (\r
    # stripped, whitespace runs collapsed, blank + // comment lines ignored): with our entry in
    # the file, a spaced `m_Name    =   "hammer"` line or a trailing blank/comment line is NOT
    # damaged (ensure reads current, rc 0). A REAL anchor missing IS damaged, and the message
    # names the anchor and is actionable.
    local T="$W/trees/dmg-rare"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "dmg-rare install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    python3 - "$T/game/bin/sdkenginetools.txt" <<'PY'
import re, sys
p = sys.argv[1]
t = open(p, encoding="utf-8", newline="").read()
t = t.replace('m_Name = "hammer"', 'm_Name     =    "hammer"', 1)
t = t + "\n// a trailing Valve comment\n\n"
open(p, "w", encoding="utf-8", newline="").write(t)
PY
    rs --dry-run ensure
    if [ "$RS_RC" = 0 ] && ! printf '%s' "$RS_OUT" | grep -q 'damaged'; then
        pass "wine: a spaced hammer line + trailing comment/blank do NOT read damaged (normalised anchors)"
    else
        fail "wine: spaced/commented manifest reads current" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | grep -E 'fatal|stale|damaged' | head -3)"
    fi
    # a REAL anchor missing -> damaged, and the message names the anchor + is actionable
    local T3="$W/trees/dmg-anchor-msg"
    fresh_wine "$T3"
    mk_env "$T3" wine
    rs install >/dev/null 2>&1
    python3 - "$T3/game/bin/sdkenginetools.txt" <<'PY'
import re, sys
p = sys.argv[1]
t = open(p, encoding="utf-8", newline="").read()
t = re.sub(r"\t\t\{\n\t\t\tm_Name = \"hammer\".*?\},\n", "", t, count=1, flags=re.S)
open(p, "w", encoding="utf-8", newline="").write(t)
PY
    rs --dry-run ensure
    if [ "$RS_RC" = 1 ] \
       && printf '%s' "$RS_OUT" | grep -q "missing anchor 'm_Name = \"hammer\"'" \
       && printf '%s' "$RS_OUT" | grep -q 'verify the game files; if Valve changed this line, update install-rs.sh.s anchors (apply_manifest_edit)'; then
        pass "wine: a real anchor missing reads damaged; the message names it + is actionable"
    else
        fail "wine: real anchor missing message" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | grep -E 'fatal|damaged' | head -3)"
    fi
}

case_ensure_readonly_tree() {
    # r3 item 1: ensure's read-only check must not write ANYTHING into the tree: its gameinfo
    # compare copy lives under the checkout's target/. Before the fix the linux check created
    # <gi>.ensure.tmp in the tree and died there with a PermissionError on a read-only csgo/
    # (reviewer repro S3). FAILS BEFORE (rc 1, PermissionError), PASSES AFTER.
    local T="$W/trees/ensure-readonly"
    [ -d "$T/game" ] && find "$T/game" -type d -exec chmod u+w {} +   # leftover of a killed run
    fresh_linux "$T" crlf
    make_stub none
    mk_env "$T" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "readonly: install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    find "$T/game" -type d -exec chmod a-w {} +
    local list_before rc1 rc2
    list_before="$(cd "$T/game" && find . | sort)"
    rs --platform linux --dry-run ensure
    rc1=$RS_RC
    rs --platform linux ensure
    rc2=$RS_RC
    find "$T/game" -type d -exec chmod u+w {} +   # undo, for the diff -r runs below
    local list_after
    list_after="$(cd "$T/game" && find . | sort)"
    if [ "$rc1" = 0 ] && [ "$rc2" = 0 ]; then
        pass "linux ensure (--dry-run and plain) is rc 0 with a read-only csgo/"
    else
        fail "linux ensure on a read-only tree" "dry=$rc1 plain=$rc2 out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    if [ "$list_before" = "$list_after" ]; then
        pass "linux ensure on a read-only tree creates NO file anywhere in the tree"
    else
        fail "linux ensure wrote into the tree" "diff:"
        diff <(printf '%s\n' "$list_before") <(printf '%s\n' "$list_after") | sed 's/^/FAIL     /'
    fi
    printf '%s' "$RS_OUT" | grep -q 'all artefacts current' \
        || fail "readonly ensure all-current message" "$(printf '%s' "$RS_OUT" | tail -3)"
}

case_metaplugins_sha_gate() {
    # r3 item 2: a CREATED metaplugins.ini must record the sha of the bytes WE wrote (the
    # temp/final file) - before, the linux put() hashed $dest BEFORE the mv (No such file),
    # the INSTALLED line carried an empty sha, and uninstall deleted another plugin's
    # registration. Test: install -> another plugin appends a line -> uninstall keeps the file
    # (message says why); with no foreign edit, uninstall removes it. Both platforms.
    # linux: FAILS BEFORE (file deleted despite the foreign edit); wine: PASSES BEFORE AND
    # AFTER (wine hashes the final file at write_installed) - regression guard.
    local T="$W/trees/mpgate-linux"
    fresh_linux "$T" crlf
    make_stub none
    mk_env "$T" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "mpgate-linux install rc" "rc=$RS_RC"; return 0; }
    printf 'plugin_other  ../bin/win64/someotherplugin.dll\n' >> "$T/game/csgo/addons/metamod/metaplugins.ini"
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "mpgate-linux uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if [ -f "$T/game/csgo/addons/metamod/metaplugins.ini" ] \
       && grep -q 'plugin_other' "$T/game/csgo/addons/metamod/metaplugins.ini"; then
        pass "linux uninstall keeps the foreign-edited metaplugins.ini"
    else
        fail "linux uninstall keeps the foreign-edited metaplugins.ini" "deleted or foreign line gone"
    fi
    printf '%s' "$RS_OUT" | grep -q 'kept: csgo/addons/metamod/metaplugins.ini' \
        || fail "linux uninstall says WHY it keeps the metaplugins.ini" "$(printf '%s' "$RS_OUT" | tail -3)"
    # and with NO foreign edit, uninstall removes it (the sha gate lets OUR file through)
    local T2="$W/trees/mpgate-linux-clean"
    fresh_linux "$T2" crlf
    mk_env "$T2" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "mpgate-linux-clean install rc" "rc=$RS_RC"; return 0; }
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "mpgate-linux-clean uninstall rc" "rc=$RS_RC"; return 0; }
    if [ ! -e "$T2/game/csgo/addons/metamod/metaplugins.ini" ] \
       && printf '%s' "$RS_OUT" | grep -q 'removed (added by install): csgo/addons/metamod/metaplugins.ini'; then
        pass "linux uninstall removes the unedited metaplugins.ini we created"
    else
        fail "linux uninstall removes the unedited metaplugins.ini" "$(printf '%s' "$RS_OUT" | grep metaplugins)"
    fi
    # wine: the same gate (hash of what we created)
    local TW="$W/trees/mpgate-wine"
    fresh_wine "$TW"
    mk_env "$TW" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "mpgate-wine install rc" "rc=$RS_RC"; return 0; }
    printf 'plugin_other  ../bin/win64/someotherplugin.dll\n' >> "$TW/game/csgo/addons/metamod/metaplugins.ini"
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "mpgate-wine uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if [ -f "$TW/game/csgo/addons/metamod/metaplugins.ini" ] \
       && grep -q 'plugin_other' "$TW/game/csgo/addons/metamod/metaplugins.ini"; then
        pass "wine uninstall keeps the foreign-edited metaplugins.ini"
    else
        fail "wine uninstall keeps the foreign-edited metaplugins.ini"
    fi
}

case_killed_install_uninstallable() {
    # r3 item 3 (reviewer repro S1): a killed install finalises the backup (with the PLANNED
    # created list) before INSTALLED is rewritten as the actual list - the tree must stay
    # uninstallable: fault install -> ensure -> uninstall succeeds and the tree equals the
    # pre-install tree. Before the fix the backup carried no INSTALLED list and uninstall
    # refused ("no INSTALLED list ... nothing would be removed"). FAILS BEFORE, PASSES AFTER.
    local T="$W/trees/s1-wine"
    fresh_wine "$T"
    rm -rf "$W/common/s1.pristine"; cp -a "$T" "$W/common/s1.pristine"
    make_stub none
    mk_env "$T" wine
    local out rc
    out="$(INSTALL_RS_FAULT=pre-mv:game/bin/sdkenginetools.txt bash "$ROOT/bridge/install-rs.sh" install 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "S1 wine: killed install (fault rc 99)" \
        || fail "S1 wine killed install rc" "rc=$rc"
    local bk1; bk1="$(newest_backup_dir "$T")"
    if [ -n "$bk1" ] && [ -f "$bk1/INSTALLED" ] && [ -s "$bk1/INSTALLED" ]; then
        pass "S1 wine: the finalised backup already carries the PLANNED INSTALLED list"
    else
        fail "S1 wine: killed install's backup has no (planned) INSTALLED" "bk='$bk1'"
    fi
    rs ensure
    [ "$RS_RC" = 0 ] || fail "S1 wine: ensure repairs" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    rs uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/s1.pristine" "$T" >/dev/null 2>&1; then
        pass "S1 wine: uninstall succeeds after the killed install; tree == pre-install"
    else
        fail "S1 wine: uninstall after a killed install" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        diff -r "$W/common/s1.pristine" "$T" 2>&1 | head -6 | sed 's/^/FAIL     /'
    fi
    # linux: the same killed flow
    local TL="$W/trees/s1-linux"
    fresh_linux "$TL" crlf
    rm -rf "$W/common/s1l.pristine"; cp -a "$TL" "$W/common/s1l.pristine"
    mk_env "$TL" linux
    out="$(INSTALL_RS_FAULT=pre-mv:csgo/addons/metamod/cs2mcp.vdf bash "$ROOT/bridge/install-rs.sh" --platform linux install 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "S1 linux: killed install (fault rc 99)" \
        || fail "S1 linux killed install rc" "rc=$rc"
    bk1="$(newest_backup_dir "$TL/game")"
    if [ -n "$bk1" ] && [ -f "$bk1/INSTALLED" ] && [ -s "$bk1/INSTALLED" ]; then
        pass "S1 linux: the finalised backup already carries the PLANNED INSTALLED list"
    else
        fail "S1 linux: killed install's backup has no (planned) INSTALLED" "bk='$bk1'"
    fi
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || fail "S1 linux: ensure repairs" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    rs --platform linux uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/s1l.pristine" "$TL" >/dev/null 2>&1; then
        pass "S1 linux: uninstall succeeds after the killed install; tree == pre-install"
    else
        fail "S1 linux: uninstall after a killed install" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        diff -r "$W/common/s1l.pristine" "$TL" 2>&1 | head -6 | sed 's/^/FAIL     /'
    fi
}

case_installed_union() {
    # r3 item 3 / NIT S4: uninstall reads INSTALLED from EVERY backup of this platform+tree,
    # not only the oldest - files a later ensure repair created are removed too. The oldest
    # backup's list is removed here (an old-format backup, nothing known about what IT
    # created - warned and skipped), and the later repair's list must still drive the
    # uninstall. FAILS BEFORE (uninstall refuses outright: "no INSTALLED list"), PASSES AFTER
    # (rc 0, the repair-created file removed, the loss warned about).
    local T="$W/trees/installed-union"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "union install rc" "rc=$RS_RC"; return 0; }
    local bk1; bk1="$(newest_backup_dir "$T")"
    [ -n "$bk1" ] || { fail "union: no backup found"; return 0; }
    mv "$bk1/INSTALLED" "$W/common/union.INSTALLED.bak"   # simulate an old-format backup
    rm -f "$T/game/csgo/addons/metamod/cs2mcp.vdf"        # lost again (Steam update, say)
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "union ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    rs uninstall
    if [ "$RS_RC" = 0 ]; then
        pass "union: uninstall proceeds from the union of the other backups' lists"
    else
        fail "union: uninstall from the INSTALLED union" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    if [ ! -e "$T/game/csgo/addons/metamod/cs2mcp.vdf" ] \
       && printf '%s' "$RS_OUT" | grep -q 'removed (created by install): game/csgo/addons/metamod/cs2mcp.vdf'; then
        pass "union: the file the LATER ensure repair created is removed too"
    else
        fail "union: the later repair's created file is removed"
    fi
    if printf '%s' "$RS_OUT" | grep -q 'WARN: no INSTALLED list'; then
        pass "union: the old-format backup's missing list is warned about (not silent)"
    else
        fail "union: missing-list WARN"
    fi
    mv "$W/common/union.INSTALLED.bak" "$bk1/INSTALLED"
}

case_uninstall_atomic() {
    # r3 item 4: uninstall's writes are atomic too - the manifest revert and the restores go
    # through a same-dir temp + mv -f (fault-hooked), and the linux gameinfo temp is
    # registered with the trap. A killed uninstall must leave the original files intact, no
    # temp behind, and the next uninstall completes to the pre-install tree.
    # FAILS BEFORE: the hooks did not exist (uninstall completed, rc 0).
    local T="$W/trees/uninst-atomic-wine"
    fresh_wine "$T"
    mkdir -p "$T/game/csgo/addons/metamod"
    printf 'foreign metaplugins (pre-existing)\n' > "$T/game/csgo/addons/metamod/metaplugins.ini"
    rm -rf "$W/common/ua.pristine"; cp -a "$T" "$W/common/ua.pristine"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "uninst-atomic install rc" "rc=$RS_RC"; return 0; }
    local out rc leftovers
    out="$(INSTALL_RS_FAULT=pre-mv:uninstall:game/bin/sdkenginetools.txt bash "$ROOT/bridge/install-rs.sh" uninstall 2>&1)"
    rc=$?
    if [ "$rc" = 99 ]; then
        pass "wine uninstall fault hook at the manifest revert fires (rc 99)"
    else
        fail "wine uninstall manifest-revert fault rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    fi
    if grep -q 'm_Name = "cs2_bridge"' "$T/game/bin/sdkenginetools.txt"; then
        pass "wine killed uninstall: the manifest file is intact (our entry still there)"
    else
        fail "wine killed uninstall left a half-reverted manifest"
    fi
    leftovers="$(find "$T" -name '*.tmp.*' -type f)"
    [ -z "$leftovers" ] && pass "wine killed uninstall: no temp left behind" \
        || fail "wine killed uninstall left temps" "$leftovers"
    rs uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/ua.pristine" "$T" >/dev/null 2>&1; then
        pass "wine: the next uninstall completes to the pre-install tree"
    else
        fail "wine: uninstall after the killed one" "rc=$RS_RC"
        diff -r "$W/common/ua.pristine" "$T" 2>&1 | head -6 | sed 's/^/FAIL     /'
    fi
    # the restore path (r4 item 1: ONLY files install overwrote - WROTE_OVER - are restored,
    # so the fault point must be one of those): kill at a pre-seeded server.dll's restore
    # rename (pre-existing -> install overwrites it -> it IS in WROTE_OVER)
    local T2="$W/trees/uninst-atomic-wine2"
    fresh_wine "$T2"
    mkdir -p "$T2/game/csgo/addons/metamod" "$T2/game/csgo/addons/metamod/bin/win64"
    printf 'foreign metaplugins (pre-existing)\n' > "$T2/game/csgo/addons/metamod/metaplugins.ini"
    printf 'OLD MM CORE\n' > "$T2/game/csgo/addons/metamod/bin/win64/server.dll"
    rm -rf "$W/common/ua2.pristine"; cp -a "$T2" "$W/common/ua2.pristine"
    mk_env "$T2" wine
    rs install >/dev/null 2>&1
    if cmp -s "$T2/game/csgo/addons/metamod/bin/win64/server.dll" "$W/bin/fake.dll"; then
        pass "wine restore-kill: install really overwrote the pre-seeded server.dll (test sound)"
    else
        fail "wine restore-kill: install did not overwrite the pre-seeded server.dll (test broken)"
    fi
    out="$(INSTALL_RS_FAULT=pre-mv:uninstall-restore:game/csgo/addons/metamod/bin/win64/server.dll bash "$ROOT/bridge/install-rs.sh" uninstall 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "wine uninstall fault hook at the restore's rename fires (rc 99)" \
        || fail "wine uninstall restore fault rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    leftovers="$(find "$T2" -name '*.tmp.*' -type f)"
    [ -z "$leftovers" ] && pass "wine restore-kill: no temp left behind" \
        || fail "wine restore-kill left temps" "$leftovers"
    rs uninstall
    [ "$RS_RC" = 0 ] && diff -r "$W/common/ua2.pristine" "$T2" >/dev/null 2>&1 \
        && pass "wine: uninstall after the restore-kill completes to pre-install" \
        || { fail "wine: uninstall after the restore-kill" "rc=$RS_RC"; diff -r "$W/common/ua2.pristine" "$T2" 2>&1 | head -6 | sed 's/^/FAIL     /'; }
    # linux: a kill at the restore's rename (the temp is trap-registered) leaves no temp, our bytes
    # intact (not a half-written file), and the next uninstall works
    local TL="$W/trees/uninst-atomic-linux"
    fresh_linux "$TL" crlf
    mkdir -p "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64"
    printf 'FOREIGN-MM\n' > "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
    rm -rf "$W/common/ual.pristine"; cp -a "$TL" "$W/common/ual.pristine"
    mk_env "$TL" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "uninst-atomic-linux install rc" "rc=$RS_RC"; return 0; }
    out="$(INSTALL_RS_FAULT=pre-mv:uninstall-restore:csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so bash "$ROOT/bridge/install-rs.sh" --platform linux uninstall 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "linux uninstall fault hook at the restore's rename fires (rc 99)" \
        || fail "linux uninstall restore fault rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    if cmp -s "$W/bin/fake.so" "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"; then
        pass "linux killed uninstall: the file is intact (still our bytes, not half-restored)"
    else
        fail "linux killed uninstall left a half-restored file"
    fi
    leftovers="$(find "$TL" -name '*.tmp.*' -type f)"
    [ -z "$leftovers" ] && pass "linux killed uninstall: no temp left behind (the temp is trap-registered)" \
        || fail "linux killed uninstall left temps" "$leftovers"
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] && diff -r "$W/common/ual.pristine" "$TL" >/dev/null 2>&1 \
        && pass "linux: uninstall after the killed one completes to pre-install" \
        || { fail "linux: uninstall after the killed one" "rc=$RS_RC"; diff -r "$W/common/ual.pristine" "$TL" 2>&1 | head -6 | sed 's/^/FAIL     /'; }
}

case_uninstall_restores_overwritten() {
    # r3 item 5 (reviewer repro S2): EVERY file the backup's SHA256SUMS lists that install
    # overwrote must be restored by uninstall - not only INSTALLED + the flat dests. A
    # pre-existing metamod.2.cs2.so (another plugin set): install overwrites it, uninstall puts
    # the original bytes back.
    # FAILS BEFORE (our ELF bytes stayed in metamod.2.cs2.so), PASSES AFTER.
    local T="$W/trees/s2-linux"
    fresh_linux "$T" crlf
    mkdir -p "$T/game/csgo/addons/metamod/bin/linuxsteamrt64"
    printf 'OLD MM CORE\n' > "$T/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
    rm -rf "$W/common/s2.pristine"; cp -a "$T" "$W/common/s2.pristine"
    make_stub none
    mk_env "$T" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "S2 install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if cmp -s "$T/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so" "$W/bin/fake.so"; then
        pass "S2: install really overwrote the pre-existing core"
    else
        fail "S2: install did not overwrite the pre-existing core (test broken)"
    fi
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "S2 uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file "$W/common/s2.pristine/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so" \
             "$T/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so" \
             "S2: the overwritten pre-existing core is restored byte-for-byte"
    if diff -r "$W/common/s2.pristine" "$T" >/dev/null 2>&1; then
        pass "S2: the tree equals the pre-install tree"
    else
        fail "S2: tree equals pre-install"
        diff -r "$W/common/s2.pristine" "$T" 2>&1 | head -6 | sed 's/^/FAIL     /'
    fi
}

case_wrote_over_restore() {
    # r4 item 1 (the reviewer's repros): uninstall restores ONLY what install overwrote
    # (WROTE_OVER: pre-existing + our bytes written + the sha of what we wrote), never a file
    # it merely kept. The gameinfo/manifest revert happens LAST; a failed restore is a
    # warning, not a stuck uninstall.
    # (i) pre-existing metaplugins.ini edited by another plugin after install -> edit kept.
    #     FAILS BEFORE (uninstall restored the stale backup copy over the edit).
    local T="$W/trees/wro-metaplugins"
    fresh_wine "$T"
    mkdir -p "$T/game/csgo/addons/metamod"
    printf 'plugin_other line\n' > "$T/game/csgo/addons/metamod/metaplugins.ini"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "wro (i) install rc" "rc=$RS_RC"; return 0; }
    printf 'plugin_other EDITED after install\n' >> "$T/game/csgo/addons/metamod/metaplugins.ini"
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "wro (i) uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if grep -q 'EDITED after install' "$T/game/csgo/addons/metamod/metaplugins.ini"; then
        pass "wro (i): the foreign metaplugins.ini edit survives uninstall (never restored)"
    else
        fail "wro (i): foreign metaplugins edit kept" "the backup's stale copy was restored"
    fi
    # (ii) linux: a foreign addons/metamod/x.vdf REMOVED by its owner after install -> stays
    #      removed. FAILS BEFORE (uninstall resurrected it from the backup's SHA256SUMS).
    local TL="$W/trees/wro-foreign-removed"
    fresh_linux "$TL" crlf
    mkdir -p "$TL/game/csgo/addons/metamod"
    printf 'a foreign plugin vdf\n' > "$TL/game/csgo/addons/metamod/x.vdf"
    mk_env "$TL" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "wro (ii) install rc" "rc=$RS_RC"; return 0; }
    rm -f "$TL/game/csgo/addons/metamod/x.vdf"
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "wro (ii) uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if [ ! -e "$TL/game/csgo/addons/metamod/x.vdf" ]; then
        pass "wro (ii): the foreign file the owner removed stays removed"
    else
        fail "wro (ii): removed foreign file stays removed" "uninstall resurrected it"
    fi
    # (iii) linux: a foreign SUBDIR removed after install -> uninstall completes.
    #       FAILS BEFORE (the restore-everything loop died on the missing dir; uninstall rc 1).
    local T3="$W/trees/wro-foreign-subdir"
    fresh_linux "$T3" crlf
    mkdir -p "$T3/game/csgo/addons/metamod/sub"
    printf 'foreign sub file\n' > "$T3/game/csgo/addons/metamod/sub/foo.vdf"
    mk_env "$T3" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "wro (iii) install rc" "rc=$RS_RC"; return 0; }
    rm -rf "$T3/game/csgo/addons/metamod/sub"
    rs --platform linux uninstall
    if [ "$RS_RC" = 0 ]; then
        pass "wro (iii): uninstall completes when a foreign subdir is gone"
    else
        fail "wro (iii): uninstall completes with the foreign subdir gone" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    # (v) a WROTE_OVER file changed after install (Steam/user) -> NOT restored, message.
    #     FAILS BEFORE (uninstall overwrote the change with the backup copy, silently).
    local T4="$W/trees/wro-changed"
    fresh_wine "$T4"
    mkdir -p "$T4/game/csgo/addons/metamod/bin/win64"
    printf 'OLD MM CORE\n' > "$T4/game/csgo/addons/metamod/bin/win64/server.dll"
    mk_env "$T4" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "wro (v) install rc" "rc=$RS_RC"; return 0; }
    printf 'CHANGED AFTER INSTALL\n' > "$T4/game/csgo/addons/metamod/bin/win64/server.dll"
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "wro (v) uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if printf '%s' "$RS_OUT" | grep -q 'not restored: game/csgo/addons/metamod/bin/win64/server.dll' \
       && cmp -s <(printf 'CHANGED AFTER INSTALL\n') "$T4/game/csgo/addons/metamod/bin/win64/server.dll"; then
        pass "wro (v): the changed-after-install file is left alone, with the message"
    else
        fail "wro (v): changed-after-install file not restored + message" "$(printf '%s' "$RS_OUT" | grep -E 'restored' | tail -2)"
    fi
    # and the UNCHANGED overwritten file IS restored (the S2 control, wine side)
    local T5="$W/trees/wro-restored"
    fresh_wine "$T5"
    mkdir -p "$T5/game/csgo/addons/metamod/bin/win64"
    printf 'OLD MM CORE\n' > "$T5/game/csgo/addons/metamod/bin/win64/server.dll"
    mk_env "$T5" wine
    rs install >/dev/null 2>&1
    rs uninstall >/dev/null 2>&1
    if cmp -s <(printf 'OLD MM CORE\n') "$T5/game/csgo/addons/metamod/bin/win64/server.dll"; then
        pass "wro control: the overwritten pre-existing file IS restored byte-for-byte"
    else
        fail "wro control: overwritten pre-existing file restored"
    fi
}

case_installed_no_predelete() {
    # r4 item 3 (the reviewer's repro): the INSTALLED union must not delete PRE-INSTALL files.
    # A pre-existing cs2mcp.vdf (wine) / metamod.2.cs2.so (linux), deleted by its owner, then
    # recreated by ensure (a later backup records it as CREATED) -> uninstall must put the
    # ORIGINAL bytes back, not delete the file and not keep ensure's copy.
    # FAILS BEFORE (uninstall deleted the recreated file: the tree lost a pre-install file).
    local T="$W/trees/nopredelete-wine"
    fresh_wine "$T"
    mkdir -p "$T/game/csgo/addons/metamod"
    printf 'PRE-INSTALL vdf bytes\n' > "$T/game/csgo/addons/metamod/cs2mcp.vdf"
    rm -rf "$W/common/npd.pristine"; cp -a "$T" "$W/common/npd.pristine"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "nopredelete install rc" "rc=$RS_RC"; return 0; }
    rm -f "$T/game/csgo/addons/metamod/cs2mcp.vdf"   # the owner deletes it
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "nopredelete ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if [ -f "$T/game/csgo/addons/metamod/cs2mcp.vdf" ] \
       && ! cmp -s "$W/common/npd.pristine/game/csgo/addons/metamod/cs2mcp.vdf" "$T/game/csgo/addons/metamod/cs2mcp.vdf"; then
        pass "nopredelete: ensure recreated it with OUR bytes (test sound)"
    else
        fail "nopredelete: ensure did not recreate the deleted file (test broken)"
    fi
    rs uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/npd.pristine" "$T" >/dev/null 2>&1; then
        pass "wine: uninstall restores the pre-install file (the INSTALLED union did not delete it)"
    else
        fail "wine: pre-install file survives uninstall" "rc=$RS_RC"
        diff -r "$W/common/npd.pristine" "$T" 2>&1 | head -6 | sed 's/^/FAIL     /'
    fi
    if printf '%s' "$RS_OUT" | grep -q 'pre-existed the first install - NOT deleted'; then
        pass "wine: the delete skip says why (pre-existed the first install)"
    else
        fail "wine: the delete-skip message"
    fi
    # linux: the same repro with a pre-existing metamod.2.cs2.so
    local TL="$W/trees/nopredelete-linux"
    fresh_linux "$TL" crlf
    mkdir -p "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64"
    printf 'PRE-INSTALL MM CORE\n' > "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
    rm -rf "$W/common/npdl.pristine"; cp -a "$TL" "$W/common/npdl.pristine"
    mk_env "$TL" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "nopredelete-linux install rc" "rc=$RS_RC"; return 0; }
    rm -f "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "nopredelete-linux ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    rs --platform linux uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/npdl.pristine" "$TL" >/dev/null 2>&1; then
        pass "linux: uninstall restores the pre-install file (the INSTALLED union did not delete it)"
    else
        fail "linux: pre-install file survives uninstall" "rc=$RS_RC"
        diff -r "$W/common/npdl.pristine" "$TL" 2>&1 | head -6 | sed 's/^/FAIL     /'
    fi
}

case_anchors() {
    # r3 item 6 (the reviewer's repro): "current" also requires the Valve-owned anchor install
    # relies on - the manifest's pre-install hammer entry, fingerprinted as a stripped line in the
    # backup's ANCHORS at install time (defaults when no backup). A file truncated AFTER our
    # entry (our entry present, the anchor gone) must be FATAL "game file damaged" rc 1 - before
    # the fix the edit-compare read it ok/current.
    local T="$W/trees/anchors-manifest"
    fresh_wine "$T"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "anchors-manifest install rc" "rc=$RS_RC"; return 0; }
    # the new backup's ANCHORS records the manifest anchor ONLY (no gameinfo fingerprints)
    local bka; bka="$(newest_backup_dir "$T")"
    if [ -n "$bka" ] && grep -q $'^manifest\tm_Name = "hammer"$' "$bka/ANCHORS" && ! grep -q '^gameinfo' "$bka/ANCHORS"; then
        pass "wine backup ANCHORS fingerprints the manifest anchor only (no gameinfo)"
    else
        fail "wine backup ANCHORS content" "$(cat "$bka/ANCHORS" 2>/dev/null)"
    fi
    # the reviewer's repro: truncate from the hammer entry on - OUR entry survives
    python3 - "$T/game/bin/sdkenginetools.txt" <<'PY'
import sys
p = sys.argv[1]
t = open(p, encoding="utf-8", newline="").read()
i = t.index('m_Name = "hammer"')
open(p, "w", encoding="utf-8", newline="").write(t[:i])
PY
    snap "$T/game" > "$W/common/anchm.snap"
    rs --dry-run ensure
    if [ "$RS_RC" = 1 ] && printf '%s' "$RS_OUT" | grep -q 'game file damaged'; then
        pass "wine: manifest truncated after our entry (hammer anchor gone) -> damaged rc 1"
    else
        fail "wine: truncated manifest reads damaged" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    snap "$T/game" > "$W/common/anchm.snap.after"
    cmp_file "$W/common/anchm.snap" "$W/common/anchm.snap.after" "wine: the damaged manifest is NOT touched"
}

case_nits() {
    # r3 NITs:
    # - stray temps: a SIGKILLed run's <dest>.tmp.<digits> files are removed by the next
    #   install (our exact temp naming only; a foreign 'x.tmp.notdigits' stays); the linux
    #   backup's find skips them (they are never backed up / never restored)
    # - INSTALL_RS_LOCK_WAIT non-numeric -> exit 2 with a message
    # - a status label decided on realpath; the status greps anchored (`Write csgo_core` must
    #   not count as `Write csgo`); status shows which build the installed lib matches
    # - mktemp_under_target failure -> our own message naming the dir
    local T="$W/trees/nit-stray-wine"
    fresh_wine "$T"
    printf 'stray\n' > "$T/game/bin/sdkenginetools.txt.tmp.424242"   # a killed run's temp
    printf 'foreign\n' > "$T/game/csgo/other.tmp.notdigits"     # NOT our naming: stays
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "nit stray install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if [ ! -e "$T/game/bin/sdkenginetools.txt.tmp.424242" ] && [ -f "$T/game/csgo/other.tmp.notdigits" ]; then
        pass "wine install removes the killed run's stray temps (ours only)"
    else
        fail "wine stray-temp sweep" "stray=$([ -e "$T/game/bin/sdkenginetools.txt.tmp.424242" ] && echo stayed) foreign=$([ -e "$T/game/csgo/other.tmp.notdigits" ] && echo stayed-or-gone)"
    fi
    if printf '%s' "$RS_OUT" | grep -q "removed a killed run's stray temp"; then
        pass "wine install says it removed the stray temp"
    else
        fail "wine stray-temp message"
    fi
    # linux: the same sweep + the backup find skips temps
    local TL="$W/trees/nit-stray-linux"
    fresh_linux "$TL" crlf
    mkdir -p "$TL/game/csgo/addons/metamod"
    printf 'stray2\n' > "$TL/game/csgo/addons/metamod/README.txt.tmp.888888"   # a dest's temp
    mk_env "$TL" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "nit stray linux install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if [ ! -e "$TL/game/csgo/addons/metamod/README.txt.tmp.888888" ]; then
        pass "linux install removes the killed run's stray temps (ours only)"
    else
        fail "linux stray-temp sweep" "readme-tmp=stayed"
    fi
    # the stray temp must not appear in the backup (the find -type f filter skips it: never
    # backed up, never restored later)
    local bk; bk="$(newest_backup_dir "$TL/game")"
    if [ -n "$bk" ] && ! ls "$bk"/csgo/addons/metamod/*.tmp.[0-9]* >/dev/null 2>&1; then
        pass "linux backup skipped the stray temps (find -type f filter)"
    else
        fail "linux backup contains a stray temp" "bk='$bk'"
    fi
    # INSTALL_RS_LOCK_WAIT non-numeric -> exit 2 + message
    local out rc
    out="$(INSTALL_RS_LOCK_WAIT=abc bash "$ROOT/bridge/install-rs.sh" status 2>&1)"
    rc=$?
    if [ "$rc" = 2 ] && printf '%s' "$out" | grep -q 'INSTALL_RS_LOCK_WAIT'; then
        pass "non-numeric INSTALL_RS_LOCK_WAIT -> exit 2 with a message"
    else
        fail "non-numeric INSTALL_RS_LOCK_WAIT" "rc=$rc out=$(printf '%s' "$out" | tail -2)"
    fi
    # status: the fixture has `Write csgo_core` - an anchored grep must NOT see an old `Write csgo`
    # line; a fresh tree reports "old layout none"
    local TS="$W/trees/nit-status"
    fresh_wine "$TS"
    mk_env "$TS" wine
    rs status
    printf '%s\n' "$RS_OUT" > "$W/common/nit-status-fresh.out"
    if grep -q 'old layout none' "$W/common/nit-status-fresh.out"; then
        pass "status: a pristine tree (with 'Write csgo_core') reports no old layout"
    else
        fail "status: pristine tree old-layout line" "$(grep -i 'old layout' "$W/common/nit-status-fresh.out")"
    fi
    # status: the label is decided on realpath - a trailing-slash spelling of the "real" tree
    export INSTALL_RS_REAL_APP="$TS/app"
    export CS2_GAME="$TS/app/game/"
    rs status
    printf '%s\n' "$RS_OUT" > "$W/common/nit-status-real.out"
    if grep -q 'REAL tools install' "$W/common/nit-status-real.out"; then
        pass "status: the REAL label survives a trailing-slash spelling (realpath'd)"
    else
        fail "status: REAL label not realpath'd" "$(head -1 "$W/common/nit-status-real.out")"
    fi
    # status: the installed lib's sha + which build it matches
    mk_env "$TS" wine
    rs install >/dev/null 2>&1
    rs status
    printf '%s\n' "$RS_OUT" > "$W/common/nit-status-inst.out"
    if grep -q 'tool dll.*matches build ' "$W/common/nit-status-inst.out" \
       && grep -q 'launcher .*cs2mcp_launch.exe .*matches build ' "$W/common/nit-status-inst.out"; then
        pass "status: the installed DLL and launcher lines say which build they match"
    else
        fail "status: build match missing" "$(grep -E 'tool dll|launcher' "$W/common/nit-status-inst.out")"
    fi
    unset INSTALL_RS_REAL_APP
    # linux status: a pristine tree (a commented-out anchor line and `Write legacy csgo` in the
    # gameinfo) reports no old layout - the checks are anchored per-line; after install it lists the new set
    local TLL="$W/trees/nit-status-linux"
    fresh_linux "$TLL" crlf
    printf '\t\t// Game csgo/addons/metamod (disabled by the owner)\n' >> "$TLL/game/csgo/gameinfo.gi"
    printf '\t\tWrite\tlegacy csgo\n' >> "$TLL/game/csgo/gameinfo.gi"
    mk_env "$TLL" linux
    rs --platform linux status
    printf '%s\n' "$RS_OUT" > "$W/common/nit-status-linux.out"
    if grep -q 'old layout none' "$W/common/nit-status-linux.out"; then
        pass "linux status: anchored per-line (a commented anchor line and 'Write legacy csgo' are no old layout)"
    else
        fail "linux status: unanchored old-layout checks" "$(grep -i 'old layout' "$W/common/nit-status-linux.out")"
    fi
    rs --platform linux install >/dev/null 2>&1
    rs --platform linux status
    printf '%s\n' "$RS_OUT" > "$W/common/nit-status-linux2.out"
    if grep -q 'libcs2mcp_mmload.so  present  sha256=.*matches build ' "$W/common/nit-status-linux2.out" \
       && grep -q 'libcs2_bridge.so  present  sha256=.*matches build ' "$W/common/nit-status-linux2.out"; then
        pass "linux status: both installed libs say which build they match"
    else
        fail "linux status: build match lines" "$(cat "$W/common/nit-status-linux2.out")"
    fi
    # mktemp_under_target failure -> our own message naming the dir (ensure is the wine path's
    # first mktemp_under_target caller), on the installed fake wine tree: never the real one
    chmod a-w "$ROOT/target"
    out="$(CS2_GAME="$TS/game" bash "$ROOT/bridge/install-rs.sh" --dry-run ensure 2>&1)"
    rc=$?
    chmod u+w "$ROOT/target"
    if [ "$rc" = 1 ] && printf '%s' "$out" | grep -q "cannot create a temp file under $ROOT/target"; then
        pass "mktemp_under_target failure -> our message naming the dir"
    else
        fail "mktemp_under_target failure message" "rc=$rc out=$(printf '%s' "$out" | tail -2)"
    fi
}

case_wrote_over_chain() {
    # r5 item S1 (the reviewer's repro target/review4/u1.sh + u1l.sh): when a rebuild between
    # installs made ensure reinstall a path, uninstall compared the current file with the
    # OLDEST backup's "what we wrote" sha, said "changed after install - Steam/user?" and
    # left OUR binary. Fix: the WROTE_OVER chain walk - start from the NEWEST backup whose
    # WROTE_OVER sha equals the current sha, take its copy, and while that copy is itself
    # what an OLDER backup recorded as written, step to the older backup's copy; restore the
    # last copy reached (the bytes that existed before we FIRST wrote the path).
    # (a) wine: pre-existing foreign file -> install -> rebuild (new artefact sha) -> ensure
    #     -> uninstall => pre-install bytes. FAILS BEFORE (our rebuilt bytes stayed).
    local T="$W/trees/chain-rebuild-wine"
    fresh_wine "$T"
    mkdir -p "$T/game/csgo/addons/metamod/bin/win64"
    printf 'FOREIGN-SERVER\n' > "$T/game/csgo/addons/metamod/bin/win64/server.dll"
    rm -rf "$W/common/chain-w.pristine"; cp -a "$T" "$W/common/chain-w.pristine"
    make_stub none
    mk_env "$T" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "chain (a) install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cp -a "$W/builds/mmwin" "$W/builds/mmwin-chain"; printf 'rebuilt\n' >> "$W/builds/mmwin-chain/metamod/bin/win64/server.dll"
    export CS2_MM="$W/builds/mmwin-chain"   # (after mk_env: a metamod rebuild between installs)
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "chain (a) ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if cmp -s "$T/game/csgo/addons/metamod/bin/win64/server.dll" "$W/builds/mmwin-chain/metamod/bin/win64/server.dll"; then
        pass "chain (a) wine: ensure really installed the rebuilt bytes (test broken otherwise)"
    else
        fail "chain (a) wine: ensure did not install the rebuilt bytes (test broken)"
    fi
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "chain (a) uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file <(printf 'FOREIGN-SERVER\n') "$T/game/csgo/addons/metamod/bin/win64/server.dll" \
        "chain (a) wine: uninstall recovers the PRE-INSTALL bytes (not our rebuild)"
    if diff -r "$W/common/chain-w.pristine" "$T" >/dev/null 2>&1; then
        pass "chain (a) wine: the tree equals the pre-install tree"
    else
        fail "chain (a) wine: tree equals pre-install"
        diff -r "$W/common/chain-w.pristine" "$T" 2>&1 | head -6 | sed 's/^/FAIL     /'
    fi
    # (b) linux: the same with metamod.2.cs2.so (the reviewer's u1l.sh).
    local TL="$W/trees/chain-rebuild-linux"
    fresh_linux "$TL" crlf
    mkdir -p "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64"
    printf 'FOREIGN-MM\n' > "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
    rm -rf "$W/common/chain-l.pristine"; cp -a "$TL" "$W/common/chain-l.pristine"
    mk_env "$TL" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "chain (b) install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cp -a "$W/builds/mmlinux" "$W/builds/mmlinux-chain"; printf 'rebuilt\n' >> "$W/builds/mmlinux-chain/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
    export CS2_MM_LINUX="$W/builds/mmlinux-chain"
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "chain (b) ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    if cmp -s "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so" "$W/builds/mmlinux-chain/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"; then
        pass "chain (b) linux: ensure really installed the rebuilt bytes (test broken otherwise)"
    else
        fail "chain (b) linux: ensure did not install the rebuilt bytes (test broken)"
    fi
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "chain (b) uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file <(printf 'FOREIGN-MM\n') "$TL/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so" \
        "chain (b) linux: uninstall recovers the PRE-INSTALL bytes (not our rebuild)"
    if diff -r "$W/common/chain-l.pristine" "$TL" >/dev/null 2>&1; then
        pass "chain (b) linux: the tree equals the pre-install tree"
    else
        fail "chain (b) linux: tree equals pre-install"
        diff -r "$W/common/chain-l.pristine" "$TL" 2>&1 | head -6 | sed 's/^/FAIL     /'
    fi
    # (c) wine: pre-existing P -> the user replaces it with Q -> ensure overwrites Q ->
    #     uninstall => Q (not P). FAILS BEFORE (uninstall restored P: "first entry wins").
    local T2="$W/trees/chain-userq-wine"
    fresh_wine "$T2"
    mkdir -p "$T2/game/csgo/addons/metamod/bin/win64"
    printf 'P-ORIGINAL\n' > "$T2/game/csgo/addons/metamod/bin/win64/server.dll"
    mk_env "$T2" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "chain (c) install rc" "rc=$RS_RC"; return 0; }
    printf 'Q-USER-REPLACED\n' > "$T2/game/csgo/addons/metamod/bin/win64/server.dll"
    rs ensure   # stale (differs from the build) -> reinstalls, overwrites Q with our bytes
    [ "$RS_RC" = 0 ] || { fail "chain (c) ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    rs uninstall
    [ "$RS_RC" = 0 ] || { fail "chain (c) uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file <(printf 'Q-USER-REPLACED\n') "$T2/game/csgo/addons/metamod/bin/win64/server.dll" \
        "chain (c) wine: uninstall puts the user's Q back (not the pre-install P, not ours)"
    # (d) linux: the same.
    local T3="$W/trees/chain-userq-linux"
    fresh_linux "$T3" crlf
    mkdir -p "$T3/game/csgo/addons/metamod/bin/linuxsteamrt64"
    printf 'P-ORIGINAL\n' > "$T3/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
    mk_env "$T3" linux
    rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "chain (d) install rc" "rc=$RS_RC"; return 0; }
    printf 'Q-USER-REPLACED\n' > "$T3/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so"
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "chain (d) ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    rs --platform linux uninstall
    [ "$RS_RC" = 0 ] || { fail "chain (d) uninstall rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp_file <(printf 'Q-USER-REPLACED\n') "$T3/game/csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so" \
        "chain (d) linux: uninstall puts the user's Q back (not the pre-install P, not ours)"
    # (e) r5 NIT: wine refuses when a backup OTHER than the oldest that contributes entries
    #     fails its SHA256SUMS check. FAILS BEFORE (wine verified only the oldest backup).
    local T4="$W/trees/chain-shaverify" bk2
    fresh_wine "$T4"
    mk_env "$T4" wine
    rs install
    [ "$RS_RC" = 0 ] || { fail "chain (e) install rc" "rc=$RS_RC"; return 0; }
    cp "$FIX/sdkenginetools.txt" "$T4/game/bin/sdkenginetools.txt"   # a Steam update restores the manifest (our entry gone)
    rs ensure   # repairs the manifest -> a SECOND backup (of sdkenginetools.txt) exists now
    [ "$RS_RC" = 0 ] || { fail "chain (e) ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    bk2="$(newest_backup_dir "$T4")"   # wine: APP is the game dir's parent (= the APP_TREE stamp)
    [ -n "$bk2" ] || { fail "chain (e): no second backup after the repair (test broken)"; return 0; }
    printf 'TAMPERED\n' >> "$bk2/game/bin/sdkenginetools.txt"
    rs uninstall
    if [ "$RS_RC" -ne 0 ] && printf '%s' "$RS_OUT" | grep -q 'backup SHA256SUMS mismatch'; then
        pass "chain (e) wine: a tampered NEWER backup is caught (uninstall refuses, rc=$RS_RC)"
    else
        fail "chain (e) wine: tampered newer backup caught" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
}

# =========================================================================== migration ======
# The OLD (gameinfo-era) layouts are built by the pinned previous script (old_rs), so the
# migration is tested against REAL old backups (APP_TREE, SHA256SUMS, EDITS_WROTE, INSTALLED,
# WROTE_OVER, ANCHORS in the old formats).
has_gi_line() {  # has_gi_line <gameinfo> <ERE-body e.g. 'Game[[:space:]]+metamod'>: an anchored line
    grep -Eq $'^[[:space:]]*'"$2"$'[[:space:]]*\r?$' "$1"
}
insert_owner_write_csgo() {  # insert_owner_write_csgo <in> <out>: the owner's OWN `Write csgo` line after `Game core`
    python3 -I - "$1" "$2" <<'PY'
import re, sys
t = open(sys.argv[1], encoding="utf-8", newline="").read()
m = re.search(r"^([ \t]*)Game[ \t]+core[ \t]*\r?\n", t, re.M)
eol = "\r\n" if m.group(0).endswith("\r\n") else "\n"
t = t[:m.end()] + m.group(1) + "Write\tcsgo" + eol + t[m.end():]
open(sys.argv[2], "w", encoding="utf-8", newline="").write(t)
PY
}
new_wine_set=(game/bin/win64/tools/cs2_bridge.dll game/bin/sdkenginetools.txt game/csgo/gameinfo.gi
              game/bin/win64/cs2mcp_launch.exe game/bin/win64/cs2mcp_mmload.dll
              game/csgo/addons/metamod/bin/win64/server.dll game/csgo/addons/metamod/bin/win64/metamod.2.cs2.dll
              game/csgo/addons/metamod/metaplugins.ini game/csgo/addons/metamod/cs2mcp.vdf)
new_linux_set=(csgo/gameinfo.gi csgo/addons/unrelated-stays.txt
               csgo/addons/metamod/bin/linux64/server.so csgo/addons/metamod/bin/linuxsteamrt64/libserver.so
               csgo/addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so csgo/addons/metamod/metaplugins.ini
               csgo/addons/metamod/README.txt csgo/addons/metamod/cs2mcp.vdf
               csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2_bridge.so csgo/addons/cs2mcp/bin/linuxsteamrt64/libcs2mcp_mmload.so)

case_migration_wine() {
    # (A) a tree the OLD script installed: ensure sees it stale ("old layout"), --dry-run touches
    # nothing, ensure migrates it (our gameinfo lines, game/metamod, content/metamod), the new set
    # is laid down, gameinfo is pristine again, the next ensure is current, uninstall round-trips
    # byte-exact from the OLD backups.
    local T="$W/trees/mig-wine"
    fresh_wine "$T"
    rm -rf "$W/common/mig-wine.pristine"; cp -a "$T" "$W/common/mig-wine.pristine"
    make_stub none
    mk_env "$T" wine
    old_rs install
    [ "$RS_RC" = 0 ] || { fail "mig-wine: old script install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    # the engine leaves an empty _iconcache under every search-path root; the real tree has
    # game/metamod/_iconcache from the old `Game metamod` line
    mkdir -p "$T/game/metamod/_iconcache"
    if [ -f "$T/game/metamod/bin/win64/server.dll" ] && [ -f "$T/content/metamod/.keep" ] \
       && has_gi_line "$T/game/csgo/gameinfo.gi" 'Game[[:space:]]+metamod' \
       && has_gi_line "$T/game/csgo/gameinfo.gi" 'Write[[:space:]]+csgo'; then
        pass "mig-wine: the old script really built the gameinfo-era layout (test sound)"
    else
        fail "mig-wine: old layout not built (test broken)"; return 0
    fi
    rs status
    if printf '%s' "$RS_OUT" | grep -q 'old layout game/metamod/bin/win64/server.dll - created by an old install' \
       && printf '%s' "$RS_OUT" | grep -q 'old layout content/metamod/.keep' \
       && printf '%s' "$RS_OUT" | grep -q 'old layout game/csgo/gameinfo.gi: `Game metamod` line written by an old install'; then
        pass "mig-wine: status lists the old-layout artefacts (files + our gameinfo lines)"
    else
        fail "mig-wine: status old layout" "$(printf '%s' "$RS_OUT" | grep -i 'old layout')"
    fi
    snap "$T" > "$W/common/mig-wine.snap"
    rs --dry-run ensure
    if [ "$RS_RC" = 3 ] && printf '%s' "$RS_OUT" | grep -q 'stale:  old layout'; then
        pass "mig-wine: --dry-run ensure on an old-layout tree is stale (rc 3, names the old layout)"
    else
        fail "mig-wine: --dry-run ensure stale" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -6)"
    fi
    rs --dry-run install
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'DRY: migrate: remove game/metamod/bin/win64/server.dll' \
       && printf '%s' "$RS_OUT" | grep -q 'DRY: migrate: revert game/csgo/gameinfo.gi line Game metamod'; then
        pass "mig-wine: --dry-run install reports the migration"
    else
        fail "mig-wine: --dry-run install" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -6)"
    fi
    snap "$T" > "$W/common/mig-wine.snap.after"
    cmp_file "$W/common/mig-wine.snap" "$W/common/mig-wine.snap.after" "mig-wine: the dry-runs touch nothing"
    # busy + old layout -> refuse like any stale repair (exit 4), untouched
    make_stub busy "$T/app"
    mk_env "$T" wine 1
    rs ensure
    [ "$RS_RC" = 4 ] && pass "mig-wine: busy + old layout -> ensure exits 4" || fail "mig-wine: busy ensure" "rc=$RS_RC"
    snap "$T" > "$W/common/mig-wine.snap.after"
    cmp_file "$W/common/mig-wine.snap" "$W/common/mig-wine.snap.after" "mig-wine: busy ensure touches nothing"
    make_stub none
    mk_env "$T" wine
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "mig-wine: ensure (migrates) rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -8)"; return 0; }
    if printf '%s' "$RS_OUT" | grep -q 'migration done'; then pass "mig-wine: ensure migrated"; else fail "mig-wine: migration message" "$(printf '%s' "$RS_OUT" | tail -8)"; fi
    expect_files "$T" "mig-wine: the tree is the NEW set only (no game/metamod, no content/)" "${new_wine_set[@]}"
    cmp_file "$FIX/gameinfo.gi" "$T/game/csgo/gameinfo.gi" "mig-wine: gameinfo.gi is byte-identical to the pristine fixture again"
    absent_paths "$T" "mig-wine: old dirs are gone" game/metamod content/metamod
    local bkcount; bkcount="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-rs-*' | wc -l)"
    snap "$T/game" > "$W/common/mig-wine.cur"
    rs ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current' \
       && [ "$(find "$W/target/bridge" -maxdepth 1 -name 'backup-rs-*' | wc -l)" = "$bkcount" ]; then
        pass "mig-wine: idempotent - the next ensure is all-current and takes no backup"
    else
        fail "mig-wine: second ensure" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    rs install
    snap "$T/game" > "$W/common/mig-wine.cur2"
    [ "$RS_RC" = 0 ] && cmp_file "$W/common/mig-wine.cur" "$W/common/mig-wine.cur2" "mig-wine: a re-install after the migration is a byte-identical no-op" \
        || fail "mig-wine: re-install rc" "rc=$RS_RC"
    rs status
    printf '%s' "$RS_OUT" | grep -q 'old layout none' && pass "mig-wine: status reports no old layout afterwards" || fail "mig-wine: status afterwards"
    rs uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/mig-wine.pristine" "$T" >/dev/null 2>&1; then
        pass "mig-wine: uninstall (old + new backups) restores the PRE-OLD-INSTALL tree byte-exact"
    else
        fail "mig-wine: uninstall round-trip" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        diff -r "$W/common/mig-wine.pristine" "$T" 2>&1 | head -8 | sed 's/^/FAIL     /'
    fi

    # (B) the owner's files survive: a pre-existing game/metamod/bin/win64/server.dll (the old
    # install overwrote it - WROTE_OVER) is RESTORED, not deleted; the owner's own `Write csgo`
    # line (pre-existing, so never recorded) stays while our `Game metamod` line goes; the
    # restored owner's file is not an "old layout" marker afterwards (ensure reads current).
    local TB="$W/trees/mig-wine-owner"
    fresh_wine "$TB"
    mkdir -p "$TB/game/metamod/bin/win64"
    printf 'OWNER-MM-LOADER\n' > "$TB/game/metamod/bin/win64/server.dll"
    insert_owner_write_csgo "$TB/game/csgo/gameinfo.gi" "$W/common/mig-wine-b.gi" && cp "$W/common/mig-wine-b.gi" "$TB/game/csgo/gameinfo.gi"
    rm -rf "$W/common/mig-wine-b.pristine"; cp -a "$TB" "$W/common/mig-wine-b.pristine"
    mk_env "$TB" wine
    old_rs install
    [ "$RS_RC" = 0 ] || { fail "mig-wine-owner: old install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    cmp -s "$W/builds/mmwin/metamod/bin/win64/server.dll" "$TB/game/metamod/bin/win64/server.dll" \
        && pass "mig-wine-owner: the old install overwrote the owner's server.dll (test sound)" \
        || { fail "mig-wine-owner: old install did not overwrite server.dll (test broken)"; return 0; }
    rs install
    [ "$RS_RC" = 0 ] || { fail "mig-wine-owner: migrating install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -8)"; return 0; }
    cmp_file <(printf 'OWNER-MM-LOADER\n') "$TB/game/metamod/bin/win64/server.dll" "mig-wine-owner: the owner's server.dll bytes are restored at the old path"
    absent_paths "$TB" "mig-wine-owner: the files only the old install created are gone" \
        game/metamod/bin/win64/metamod.2.cs2.dll content/metamod
    if ! has_gi_line "$TB/game/csgo/gameinfo.gi" 'Game[[:space:]]+metamod' && has_gi_line "$TB/game/csgo/gameinfo.gi" 'Write[[:space:]]+csgo'; then
        pass "mig-wine-owner: our 'Game metamod' line is reverted, the owner's pre-existing 'Write csgo' stays"
    else
        fail "mig-wine-owner: gameinfo lines after migration" "$(grep -n 'Game\|Write' "$TB/game/csgo/gameinfo.gi" | tr -d '\r')"
    fi
    cmp_file "$W/common/mig-wine-b.pristine/game/csgo/gameinfo.gi" "$TB/game/csgo/gameinfo.gi" "mig-wine-owner: gameinfo.gi equals its pre-old-install bytes"
    rs ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current'; then
        pass "mig-wine-owner: the restored owner's file is not an old-layout marker (ensure current)"
    else
        fail "mig-wine-owner: ensure after migration" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    rs uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/mig-wine-b.pristine" "$TB" >/dev/null 2>&1; then
        pass "mig-wine-owner: uninstall round-trips to the pre-old-install tree (owner's file + 'Write csgo' intact)"
    else
        fail "mig-wine-owner: uninstall round-trip" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        diff -r "$W/common/mig-wine-b.pristine" "$TB" 2>&1 | head -8 | sed 's/^/FAIL     /'
    fi

    # (C) a Steam update wiped our gameinfo lines before the migration: the files still go,
    # nothing is "reverted", the result is the same
    local TC="$W/trees/mig-wine-steam"
    fresh_wine "$TC"
    rm -rf "$W/common/mig-wine-c.pristine"; cp -a "$TC" "$W/common/mig-wine-c.pristine"
    mk_env "$TC" wine
    old_rs install >/dev/null 2>&1
    cp "$FIX/gameinfo.gi" "$TC/game/csgo/gameinfo.gi"
    rs install
    if [ "$RS_RC" = 0 ]; then
        expect_files "$TC" "mig-wine-steam: new set only" "${new_wine_set[@]}"
    else
        fail "mig-wine-steam: install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    rs uninstall
    diff -r "$W/common/mig-wine-c.pristine" "$TC" >/dev/null 2>&1 && pass "mig-wine-steam: uninstall round-trips" \
        || fail "mig-wine-steam: uninstall round-trip"
}

case_migration_linux() {
    local T="$W/trees/mig-linux"
    fresh_linux "$T" crlf
    # the owner had their own loader vdf: the old install overwrote it (WROTE_OVER), the migration
    # must give it back
    printf 'OWNER-VDF\n' > "$T/game/csgo/addons/metamod.vdf"
    rm -rf "$W/common/mig-linux.pristine"; cp -a "$T" "$W/common/mig-linux.pristine"
    make_stub none
    mk_env "$T" linux
    old_rs --platform linux install
    [ "$RS_RC" = 0 ] || { fail "mig-linux: old script install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"; return 0; }
    # as above: the real volume has csgo/addons/metamod/_iconcache from the old search path
    mkdir -p "$T/game/csgo/addons/metamod/_iconcache"
    if [ -f "$T/game/csgo/addons/metamod_x64.vdf" ] && has_gi_line "$T/game/csgo/gameinfo.gi" 'Game[[:space:]]+csgo/addons/metamod' \
       && has_gi_line "$T/game/csgo/gameinfo.gi" 'Write[[:space:]]+csgo' && ! cmp -s "$T/game/csgo/addons/metamod.vdf" <(printf 'OWNER-VDF\n'); then
        pass "mig-linux: the old script really built the gameinfo-era layout (test sound)"
    else
        fail "mig-linux: old layout not built (test broken)"; return 0
    fi
    rs --platform linux status
    if printf '%s' "$RS_OUT" | grep -q 'old layout csgo/addons/metamod_x64.vdf - created by an old install' \
       && printf '%s' "$RS_OUT" | grep -q 'old layout csgo/addons/metamod.vdf - an old install of ours overwrote the owner' \
       && printf '%s' "$RS_OUT" | grep -q 'old layout csgo/gameinfo.gi: `Game csgo/addons/metamod` line written by an old install'; then
        pass "mig-linux: status lists the old-layout artefacts"
    else
        fail "mig-linux: status old layout" "$(printf '%s' "$RS_OUT" | grep -i 'old layout')"
    fi
    snap "$T/game" > "$W/common/mig-linux.snap"
    rs --platform linux --dry-run ensure
    if [ "$RS_RC" = 3 ] && printf '%s' "$RS_OUT" | grep -q 'stale:  old layout'; then
        pass "mig-linux: --dry-run ensure on an old-layout tree is stale (rc 3)"
    else
        fail "mig-linux: --dry-run ensure stale" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -6)"
    fi
    rs --platform linux --dry-run install
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'DRY: migrate: remove csgo/addons/metamod_x64.vdf'; then
        pass "mig-linux: --dry-run install reports the migration"
    else
        fail "mig-linux: --dry-run install" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -6)"
    fi
    snap "$T/game" > "$W/common/mig-linux.snap.after"
    cmp_file "$W/common/mig-linux.snap" "$W/common/mig-linux.snap.after" "mig-linux: the dry-runs touch nothing"
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "mig-linux: ensure (migrates) rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -8)"; return 0; }
    printf '%s' "$RS_OUT" | grep -q 'migration done' && pass "mig-linux: ensure migrated" || fail "mig-linux: migration message"
    cmp_file "$FIX/gameinfo.gi" "$T/game/csgo/gameinfo.gi" "mig-linux: gameinfo.gi is byte-identical to the pristine fixture again"
    cmp_file <(printf 'OWNER-VDF\n') "$T/game/csgo/addons/metamod.vdf" "mig-linux: the owner's metamod.vdf is restored"
    expect_files "$T/game" "mig-linux: the new set + the owner's metamod.vdf (no metamod_x64.vdf)" \
        "${new_linux_set[@]}" csgo/addons/metamod.vdf
    local bkcount; bkcount="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-linux-*' | wc -l)"
    rs --platform linux ensure
    if [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current' \
       && [ "$(find "$W/target/bridge" -maxdepth 1 -name 'backup-linux-*' | wc -l)" = "$bkcount" ]; then
        pass "mig-linux: idempotent - the next ensure is all-current (the owner's vdf is no marker) and takes no backup"
    else
        fail "mig-linux: second ensure" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    rs --platform linux uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/mig-linux.pristine" "$T" >/dev/null 2>&1; then
        pass "mig-linux: uninstall (old + new backups) restores the PRE-OLD-INSTALL tree byte-exact"
    else
        fail "mig-linux: uninstall round-trip" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        diff -r "$W/common/mig-linux.pristine" "$T" 2>&1 | head -8 | sed 's/^/FAIL     /'
    fi
    # LF gameinfo + a Steam update before the migration (lines wiped): files go, result clean
    local T2="$W/trees/mig-linux-steam"
    fresh_linux "$T2" lf
    rm -rf "$W/common/mig-linux2.pristine"; cp -a "$T2" "$W/common/mig-linux2.pristine"
    mk_env "$T2" linux
    old_rs --platform linux install >/dev/null 2>&1
    cp "$FIX/gameinfo-lf.gi" "$T2/game/csgo/gameinfo.gi"
    rs --platform linux install
    [ "$RS_RC" = 0 ] && expect_files "$T2/game" "mig-linux-steam: new set only" "${new_linux_set[@]}" \
        || fail "mig-linux-steam: install rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    rs --platform linux uninstall
    diff -r "$W/common/mig-linux2.pristine" "$T2" >/dev/null 2>&1 && pass "mig-linux-steam: uninstall round-trips" \
        || fail "mig-linux-steam: uninstall round-trip"
}

case_migration_refuse() {
    # An old-layout artefact NO backup of this tree attributes to us is never guessed at:
    # install/ensure refuse (non-zero, a message naming it), the tree is byte-untouched.
    make_stub none
    # (1) wine: a hand-made game/metamod/ with no backups at all
    local T="$W/trees/refuse-wine-file"
    fresh_wine "$T"
    mkdir -p "$T/game/metamod/bin/win64"
    printf 'SOMEBODY-ELSES-LOADER\n' > "$T/game/metamod/bin/win64/server.dll"
    mk_env "$T" wine
    snap "$T" > "$W/common/refuse-w1.snap"
    local bkn; bkn="$(find "$W/target/bridge" -maxdepth 1 -name 'backup-rs-*' | wc -l)"
    rs install
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'game/metamod/bin/win64/server.dll' \
       && printf '%s' "$RS_OUT" | grep -q 'cannot attribute'; then
        pass "refuse-wine: an unattributable game/metamod file -> install refuses, names it (rc=$RS_RC)"
    else
        fail "refuse-wine: unattributable file" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    snap "$T" > "$W/common/refuse-w1.snap.after"
    cmp_file "$W/common/refuse-w1.snap" "$W/common/refuse-w1.snap.after" "refuse-wine: the refused install touched nothing (no backup, no copy)"
    [ "$(find "$W/target/bridge" -maxdepth 1 -name 'backup-rs-*' | wc -l)" = "$bkn" ] \
        && pass "refuse-wine: the refusal created no backup dir" || fail "refuse-wine: a backup dir was created by the refusal"
    rs ensure
    if [ "$RS_RC" = 1 ] && printf '%s' "$RS_OUT" | grep -q 'stale:  old layout: game/metamod/bin/win64/server.dll - NOT attributable'; then
        pass "refuse-wine: ensure reports it stale (NOT attributable) and its repair refuses (rc 1)"
    else
        fail "refuse-wine: ensure" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    rs --dry-run install
    [ "$RS_RC" != 0 ] && pass "refuse-wine: --dry-run install refuses too" || fail "refuse-wine: --dry-run install"
    snap "$T" > "$W/common/refuse-w1.snap.after"
    cmp_file "$W/common/refuse-w1.snap" "$W/common/refuse-w1.snap.after" "refuse-wine: ensure + dry-run touched nothing"
    # (2) wine: a `Game metamod` line no install of ours wrote
    local T2="$W/trees/refuse-wine-gi"
    fresh_wine "$T2"
    python3 -I - "$T2/game/csgo/gameinfo.gi" <<'PY'
import re, sys
p = sys.argv[1]
t = open(p, encoding="utf-8", newline="").read()
m = re.search(r"^([ \t]*)Game[ \t]+csgo[ \t]*\r?\n", t, re.M)
t = t[:m.start()] + m.group(1) + "Game\tmetamod\r\n" + t[m.start():]
open(p, "w", encoding="utf-8", newline="").write(t)
PY
    mk_env "$T2" wine
    snap "$T2" > "$W/common/refuse-w2.snap"
    rs install
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'a `Game metamod` line that no install of ours wrote'; then
        pass "refuse-wine: a foreign 'Game metamod' gameinfo line -> install refuses (we never remove a line we did not write)"
    else
        fail "refuse-wine: foreign gameinfo line" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    snap "$T2" > "$W/common/refuse-w2.snap.after"
    cmp_file "$W/common/refuse-w2.snap" "$W/common/refuse-w2.snap.after" "refuse-wine: the foreign-line refusal touched nothing"
    # (3) wine: an unrecorded `Write csgo` ALONE is the owner's/Valve's: install proceeds, line stays
    local T3="$W/trees/refuse-wine-write"
    fresh_wine "$T3"
    insert_owner_write_csgo "$T3/game/csgo/gameinfo.gi" "$W/common/refuse-w3.gi" && cp "$W/common/refuse-w3.gi" "$T3/game/csgo/gameinfo.gi"
    mk_env "$T3" wine
    rs install
    if [ "$RS_RC" = 0 ] && cmp -s "$W/common/refuse-w3.gi" "$T3/game/csgo/gameinfo.gi"; then
        pass "refuse-wine: an unrecorded 'Write csgo' line is left alone (install proceeds, gameinfo byte-identical)"
    else
        fail "refuse-wine: unrecorded Write csgo" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    rs ensure
    [ "$RS_RC" = 0 ] && printf '%s' "$RS_OUT" | grep -q 'all artefacts current' \
        && pass "refuse-wine: ...and ensure reads current (no repair loop)" || fail "refuse-wine: ensure after unrecorded Write csgo" "rc=$RS_RC"
    # (4) wine: the old install's BACKUPS are gone -> its files cannot be attributed
    local T4="$W/trees/refuse-wine-nobackup"
    fresh_wine "$T4"
    mk_env "$T4" wine
    old_rs install >/dev/null 2>&1
    local bk4; bk4="$(newest_backup_dir "$T4")"
    mkdir -p "$W/common/parked"
    mv "$bk4" "$W/common/parked/"
    snap "$T4" > "$W/common/refuse-w4.snap"
    rs install
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'game/metamod/bin/win64/server.dll'; then
        pass "refuse-wine: old files whose backups are gone -> refused (no guessing)"
    else
        fail "refuse-wine: backups gone" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    snap "$T4" > "$W/common/refuse-w4.snap.after"
    cmp_file "$W/common/refuse-w4.snap" "$W/common/refuse-w4.snap.after" "refuse-wine: nothing touched when the backups are gone"
    mv "$W/common/parked/${bk4##*/}" "$bk4"
    # (5) wine: a tampered OLD backup -> the migration refuses before touching anything
    printf 'TAMPERED\n' >> "$bk4/game/csgo/gameinfo.gi"
    snap "$T4" > "$W/common/refuse-w5.snap"
    rs install
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'backup SHA256SUMS mismatch'; then
        pass "refuse-wine: a tampered old backup is caught before the migration touches anything"
    else
        fail "refuse-wine: tampered old backup" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    snap "$T4" > "$W/common/refuse-w5.snap.after"
    cmp_file "$W/common/refuse-w5.snap" "$W/common/refuse-w5.snap.after" "refuse-wine: the tamper refusal touched nothing"

    # (6) linux: a loader vdf nobody recorded
    local TL="$W/trees/refuse-linux-file"
    fresh_linux "$TL" crlf
    printf 'SOMEBODY-ELSES-VDF\n' > "$TL/game/csgo/addons/metamod_x64.vdf"
    mk_env "$TL" linux
    snap "$TL/game" > "$W/common/refuse-l1.snap"
    rs --platform linux install
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'csgo/addons/metamod_x64.vdf' && printf '%s' "$RS_OUT" | grep -q 'cannot attribute'; then
        pass "refuse-linux: an unattributable loader vdf -> install refuses, names it (rc=$RS_RC)"
    else
        fail "refuse-linux: unattributable vdf" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    snap "$TL/game" > "$W/common/refuse-l1.snap.after"
    cmp_file "$W/common/refuse-l1.snap" "$W/common/refuse-l1.snap.after" "refuse-linux: nothing touched"
    rs --platform linux ensure
    [ "$RS_RC" = 1 ] && pass "refuse-linux: ensure's repair refuses too (rc 1)" || fail "refuse-linux: ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    # (7) linux: a `Game csgo/addons/metamod` line nobody wrote (a manual standard Metamod install)
    local TL2="$W/trees/refuse-linux-gi"
    fresh_linux "$TL2" crlf
    python3 -I - "$TL2/game/csgo/gameinfo.gi" <<'PY'
import re, sys
p = sys.argv[1]
t = open(p, encoding="utf-8", newline="").read()
m = re.search(r"^([ \t]*)Game[ \t]+csgo[ \t]*\r?\n", t, re.M)
t = t[:m.start()] + m.group(1) + "Game\tcsgo/addons/metamod\r\n" + t[m.start():]
open(p, "w", encoding="utf-8", newline="").write(t)
PY
    mk_env "$TL2" linux
    snap "$TL2/game" > "$W/common/refuse-l2.snap"
    rs --platform linux install
    if [ "$RS_RC" != 0 ] && printf '%s' "$RS_OUT" | grep -q 'a `Game csgo/addons/metamod` line that no install of ours wrote'; then
        pass "refuse-linux: a foreign 'Game csgo/addons/metamod' line -> install refuses"
    else
        fail "refuse-linux: foreign gameinfo line" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    snap "$TL2/game" > "$W/common/refuse-l2.snap.after"
    cmp_file "$W/common/refuse-l2.snap" "$W/common/refuse-l2.snap.after" "refuse-linux: the foreign-line refusal touched nothing"
}

case_migration_fault() {
    # A kill mid-migration: no temp left, the tree is consistent, the NEXT run completes it.
    make_stub none
    # wine: kill at the gameinfo revert (the LAST step: the files are already migrated)
    local T="$W/trees/migfault-wine"
    fresh_wine "$T"
    rm -rf "$W/common/migfault-w.pristine"; cp -a "$T" "$W/common/migfault-w.pristine"
    mk_env "$T" wine
    old_rs install >/dev/null 2>&1
    local out rc
    out="$(INSTALL_RS_FAULT=pre-mv:migrate:game/csgo/gameinfo.gi bash "$ROOT/bridge/install-rs.sh" install 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "migfault-wine: the fault hook at the gameinfo revert fires (rc 99)" \
        || fail "migfault-wine: fault rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    if has_gi_line "$T/game/csgo/gameinfo.gi" 'Game[[:space:]]+metamod' && [ ! -e "$T/game/metamod" ]; then
        pass "migfault-wine: the gameinfo is intact (old lines still there), the files were migrated first"
    else
        fail "migfault-wine: state after the kill"
    fi
    [ -z "$(find "$T" -name '*.tmp.*' -type f)" ] && pass "migfault-wine: no temp left behind" || fail "migfault-wine: temps left" "$(find "$T" -name '*.tmp.*')"
    rs ensure
    if [ "$RS_RC" = 0 ]; then
        expect_files "$T" "migfault-wine: the next ensure completes the migration (new set only)" "${new_wine_set[@]}"
    else
        fail "migfault-wine: next ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -5)"
    fi
    cmp_file "$FIX/gameinfo.gi" "$T/game/csgo/gameinfo.gi" "migfault-wine: gameinfo pristine after the completed migration"
    rs uninstall
    diff -r "$W/common/migfault-w.pristine" "$T" >/dev/null 2>&1 && pass "migfault-wine: uninstall round-trips after the killed migration" \
        || { fail "migfault-wine: uninstall round-trip"; diff -r "$W/common/migfault-w.pristine" "$T" 2>&1 | head -6 | sed 's/^/FAIL     /'; }
    # wine: kill at an owner-file RESTORE rename
    local T2="$W/trees/migfault-wine2"
    fresh_wine "$T2"
    mkdir -p "$T2/game/metamod/bin/win64"
    printf 'OWNER-MM-LOADER\n' > "$T2/game/metamod/bin/win64/server.dll"
    rm -rf "$W/common/migfault-w2.pristine"; cp -a "$T2" "$W/common/migfault-w2.pristine"
    mk_env "$T2" wine
    old_rs install >/dev/null 2>&1
    out="$(INSTALL_RS_FAULT=pre-mv:migrate:game/metamod/bin/win64/server.dll bash "$ROOT/bridge/install-rs.sh" install 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "migfault-wine: the fault hook at the owner-file restore fires (rc 99)" \
        || fail "migfault-wine: restore fault rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    cmp -s "$W/builds/mmwin/metamod/bin/win64/server.dll" "$T2/game/metamod/bin/win64/server.dll" \
        && [ -z "$(find "$T2" -name '*.tmp.*' -type f)" ] \
        && pass "migfault-wine: the killed restore left the file intact (not half-written) and no temp" \
        || fail "migfault-wine: killed restore state"
    rs ensure
    [ "$RS_RC" = 0 ] || fail "migfault-wine: ensure after the killed restore" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    cmp_file <(printf 'OWNER-MM-LOADER\n') "$T2/game/metamod/bin/win64/server.dll" "migfault-wine: the next ensure restores the owner's bytes"
    rs uninstall
    diff -r "$W/common/migfault-w2.pristine" "$T2" >/dev/null 2>&1 && pass "migfault-wine: uninstall round-trips after the killed restore" \
        || fail "migfault-wine: restore-kill uninstall round-trip"
    # linux: kill at the gameinfo revert, then at an owner-vdf restore
    local TL="$W/trees/migfault-linux"
    fresh_linux "$TL" crlf
    printf 'OWNER-VDF\n' > "$TL/game/csgo/addons/metamod.vdf"
    rm -rf "$W/common/migfault-l.pristine"; cp -a "$TL" "$W/common/migfault-l.pristine"
    mk_env "$TL" linux
    old_rs --platform linux install >/dev/null 2>&1
    out="$(INSTALL_RS_FAULT=pre-mv:migrate:csgo/gameinfo.gi bash "$ROOT/bridge/install-rs.sh" --platform linux install 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "migfault-linux: the fault hook at the gameinfo revert fires (rc 99)" \
        || fail "migfault-linux: fault rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    if has_gi_line "$TL/game/csgo/gameinfo.gi" 'Game[[:space:]]+csgo/addons/metamod' && [ ! -e "$TL/game/csgo/addons/metamod_x64.vdf" ]; then
        pass "migfault-linux: gameinfo intact (old lines still there), the files were migrated first"
    else
        fail "migfault-linux: state after the kill"
    fi
    [ -z "$(find "$TL" -name '*.tmp.*' -type f)" ] && pass "migfault-linux: no temp left behind" || fail "migfault-linux: temps left"
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || fail "migfault-linux: next ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    cmp_file "$FIX/gameinfo.gi" "$TL/game/csgo/gameinfo.gi" "migfault-linux: gameinfo pristine after the completed migration"
    rs --platform linux uninstall
    diff -r "$W/common/migfault-l.pristine" "$TL" >/dev/null 2>&1 && pass "migfault-linux: uninstall round-trips after the killed migration" \
        || { fail "migfault-linux: uninstall round-trip"; diff -r "$W/common/migfault-l.pristine" "$TL" 2>&1 | head -6 | sed 's/^/FAIL     /'; }
    local TL2="$W/trees/migfault-linux2"
    fresh_linux "$TL2" crlf
    printf 'OWNER-VDF\n' > "$TL2/game/csgo/addons/metamod.vdf"
    rm -rf "$W/common/migfault-l2.pristine"; cp -a "$TL2" "$W/common/migfault-l2.pristine"
    mk_env "$TL2" linux
    old_rs --platform linux install >/dev/null 2>&1
    out="$(INSTALL_RS_FAULT=pre-mv:migrate:csgo/addons/metamod.vdf bash "$ROOT/bridge/install-rs.sh" --platform linux install 2>&1)"
    rc=$?
    [ "$rc" = 99 ] && pass "migfault-linux: the fault hook at the owner-vdf restore fires (rc 99)" \
        || fail "migfault-linux: restore fault rc" "rc=$rc out=$(printf '%s' "$out" | tail -3)"
    [ -z "$(find "$TL2" -name '*.tmp.*' -type f)" ] && pass "migfault-linux: the killed restore left no temp" || fail "migfault-linux: restore temps left"
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || fail "migfault-linux: ensure after the killed restore" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    cmp_file <(printf 'OWNER-VDF\n') "$TL2/game/csgo/addons/metamod.vdf" "migfault-linux: the next ensure restores the owner's vdf"
    rs --platform linux uninstall
    diff -r "$W/common/migfault-l2.pristine" "$TL2" >/dev/null 2>&1 && pass "migfault-linux: uninstall round-trips after the killed restore" \
        || fail "migfault-linux: restore-kill uninstall round-trip"
}

case_migration_cancel() {
    # The old backups still say "an install of ours wrote `Game metamod` / `Write csgo`". Once the
    # migration removed those lines, a line with the same text that appears LATER is somebody
    # else's: the new backups record `reverted:<kind>` and uninstall must leave such lines alone
    # (we never remove a gameinfo line we did not write).
    make_stub none
    local T="$W/trees/cancel-wine"
    fresh_wine "$T"
    mk_env "$T" wine
    old_rs install >/dev/null 2>&1
    rs ensure
    [ "$RS_RC" = 0 ] || { fail "cancel-wine: migrating ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"; return 0; }
    local bkn; bkn="$(newest_backup_dir "$T")"
    if grep -q '^reverted:gameinfo:Game metamod$' "$bkn/EDITS_WROTE" && grep -q '^reverted:gameinfo:Write csgo$' "$bkn/EDITS_WROTE"; then
        pass "cancel-wine: the post-migration backup records reverted:<kind> for both gameinfo kinds"
    else
        fail "cancel-wine: reverted: records" "$(cat "$bkn/EDITS_WROTE")"
    fi
    python3 -I - "$T/game/csgo/gameinfo.gi" <<'PY'
import re, sys
p = sys.argv[1]
t = open(p, encoding="utf-8", newline="").read()
m = re.search(r"^([ \t]*)Game[ \t]+csgo[ \t]*\r?\n", t, re.M)
t = t[:m.start()] + m.group(1) + "Game\tmetamod\r\n" + t[m.start():]
m = re.search(r"^([ \t]*)Game[ \t]+core[ \t]*\r?\n", t, re.M)
t = t[:m.end()] + m.group(1) + "Write\tcsgo\r\n" + t[m.end():]
open(p, "w", encoding="utf-8", newline="").write(t)
PY
    cp "$T/game/csgo/gameinfo.gi" "$W/common/cancel-w.gi"
    rs ensure
    if [ "$RS_RC" = 1 ] && printf '%s' "$RS_OUT" | grep -q 'a `Game metamod` line that no install of ours wrote'; then
        pass "cancel-wine: a same-text 'Game metamod' line added AFTER the migration is foreign (ensure's repair refuses)"
    else
        fail "cancel-wine: later foreign line" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"
    fi
    rs uninstall
    if [ "$RS_RC" = 0 ] && cmp -s "$W/common/cancel-w.gi" "$T/game/csgo/gameinfo.gi"; then
        pass "cancel-wine: uninstall leaves the later 'Game metamod' + 'Write csgo' lines alone"
    else
        fail "cancel-wine: uninstall removed a line we did not write" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
    local TL="$W/trees/cancel-linux"
    fresh_linux "$TL" crlf
    mk_env "$TL" linux
    old_rs --platform linux install >/dev/null 2>&1
    rs --platform linux ensure
    [ "$RS_RC" = 0 ] || { fail "cancel-linux: migrating ensure rc" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -4)"; return 0; }
    python3 -I - "$TL/game/csgo/gameinfo.gi" <<'PY'
import re, sys
p = sys.argv[1]
t = open(p, encoding="utf-8", newline="").read()
m = re.search(r"^([ \t]*)Game[ \t]+csgo[ \t]*\r?\n", t, re.M)
t = t[:m.start()] + m.group(1) + "Game\tcsgo/addons/metamod\r\n" + t[m.start():]
m = re.search(r"^([ \t]*)Game[ \t]+core[ \t]*\r?\n", t, re.M)
t = t[:m.end()] + m.group(1) + "Write\tcsgo\r\n" + t[m.end():]
open(p, "w", encoding="utf-8", newline="").write(t)
PY
    cp "$TL/game/csgo/gameinfo.gi" "$W/common/cancel-l.gi"
    rs --platform linux uninstall
    if [ "$RS_RC" = 0 ] && cmp -s "$W/common/cancel-l.gi" "$TL/game/csgo/gameinfo.gi"; then
        pass "cancel-linux: uninstall leaves the later 'Game csgo/addons/metamod' + 'Write csgo' lines alone"
    else
        fail "cancel-linux: uninstall removed a line we did not write" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
    fi
}

case_old_uninstall() {
    # "Old backups remain usable by uninstall": the NEW script's uninstall, run straight on a tree
    # the OLD script installed (no migration in between), removes the old-layout artefacts it owns
    # and reverts our gameinfo lines - the tree is the pre-old-install tree again, byte-exact.
    make_stub none
    local T="$W/trees/olduninst-wine"
    fresh_wine "$T"
    rm -rf "$W/common/olduninst-w.pristine"; cp -a "$T" "$W/common/olduninst-w.pristine"
    mk_env "$T" wine
    old_rs install >/dev/null 2>&1
    rs uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/olduninst-w.pristine" "$T" >/dev/null 2>&1; then
        pass "olduninst-wine: the new uninstall restores an old-script install byte-exact"
    else
        fail "olduninst-wine: uninstall of an old install" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        diff -r "$W/common/olduninst-w.pristine" "$T" 2>&1 | head -8 | sed 's/^/FAIL     /'
    fi
    local TL="$W/trees/olduninst-linux"
    fresh_linux "$TL" crlf
    printf 'OWNER-VDF\n' > "$TL/game/csgo/addons/metamod.vdf"
    rm -rf "$W/common/olduninst-l.pristine"; cp -a "$TL" "$W/common/olduninst-l.pristine"
    mk_env "$TL" linux
    old_rs --platform linux install >/dev/null 2>&1
    rs --platform linux uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/olduninst-l.pristine" "$TL" >/dev/null 2>&1; then
        pass "olduninst-linux: the new uninstall restores an old-script install byte-exact (owner's vdf back)"
    else
        fail "olduninst-linux: uninstall of an old install" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        diff -r "$W/common/olduninst-l.pristine" "$TL" 2>&1 | head -8 | sed 's/^/FAIL     /'
    fi
    # mixed history: old install -> migration -> (a Steam update restores the manifest) ensure repair
    # -> uninstall: still byte-exact from three backups
    local TM="$W/trees/olduninst-mixed"
    fresh_wine "$TM"
    rm -rf "$W/common/olduninst-m.pristine"; cp -a "$TM" "$W/common/olduninst-m.pristine"
    mk_env "$TM" wine
    old_rs install >/dev/null 2>&1
    rs ensure >/dev/null 2>&1
    cp "$FIX/sdkenginetools.txt" "$TM/game/bin/sdkenginetools.txt"
    rs ensure >/dev/null 2>&1
    rs uninstall
    if [ "$RS_RC" = 0 ] && diff -r "$W/common/olduninst-m.pristine" "$TM" >/dev/null 2>&1; then
        pass "olduninst-mixed: old install + migration + a later repair -> uninstall is byte-exact"
    else
        fail "olduninst-mixed" "rc=$RS_RC out=$(printf '%s' "$RS_OUT" | tail -3)"
        diff -r "$W/common/olduninst-m.pristine" "$TM" 2>&1 | head -8 | sed 's/^/FAIL     /'
    fi
}


# ----------------------------------------------------------------- run -------------------
say "running (fake trees under $W - the real game trees are never touched)"
case_wine_install_reinstall_uninstall
case_wine_uninstall_legacy
case_linux_install_uninstall
case_ensure
case_ensure_linux
case_fault_hook
case_uninstall_revert
case_uninstall_installed_list
case_ensure_no_build
case_metaplugins_not_ours
case_realpath_guard
case_content_compare
case_anchors
case_damaged_rare
case_ensure_readonly_tree
case_metaplugins_sha_gate
case_killed_install_uninstallable
case_installed_union
case_uninstall_atomic
case_uninstall_restores_overwritten
case_wrote_over_restore
case_installed_no_predelete
case_wrote_over_chain
case_nits
case_migration_wine
case_migration_linux
case_migration_refuse
case_migration_fault
case_migration_cancel
case_old_uninstall
case_lock
case_negative
exit_if_setup_failed
summary_exit
