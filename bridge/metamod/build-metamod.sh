#!/usr/bin/env bash
#
# build-metamod.sh — build Metamod:Source 2.0-dev (cs2 SDK only, x86_64) in a container,
# from the same metamod-source commit our s2sdk headers pin against. Development only: releases
# and the containers take the official dev builds (fetch-metamod.sh linux|win64, README.md).
#
#   bridge/metamod/build-metamod.sh linux          -> target/metamod/linux/addons
#   bridge/metamod/build-metamod.sh win64          -> STOP: AMBuild cannot cross-compile with
#                                                     clang-cl; prints the blocker list (exit 3)
#   bridge/metamod/build-metamod.sh win64-probe    -> proves the win64 toolchain works (clang-cl +
#                                                     lld-link + the XWIN sysroot link a real DLL)
#   bridge/metamod/build-metamod.sh win64-attempt  -> drives that toolchain through AMBuild with
#                                                     the two build-script adaptations; fails at
#                                                     AMBuild's compiler probe (evidence)
#   bridge/metamod/build-metamod.sh image          -> build the AMBuild-carrying build image
#   bridge/metamod/build-metamod.sh all            -> image, then linux, then win64 (which stops)
#
# Everything is cached under target/metamod-deps/ (mounted at /deps in the container):
#   src/metamod-source  local clone of the reference clone, pinned to MM_COMMIT (+ submodules)
#   hl2sdk-cs2          the cs2 SDK (alliedmodders/s2sdk, branch cs2), cloned by checkout-deps.sh
#   build-<platform>    AMBuild build tree (so a rebuild is incremental)
#   stage-<platform>    the packaged addons, handed to target/metamod/ on success
# Outputs are written to target/metamod/<platform>/ only after the container exits 0.
#
# Environment overrides: MM_COMMIT, MM_REF, XWIN, MM_IMAGE, MM_BASE_IMAGE, MM_FORCE_IMAGE=1,
# MM_NETWORK (container network mode, default "host": rootless pasta is broken on this host).
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=../lib-root.sh
. "$ROOT/bridge/lib-root.sh"
MAIN=$(cs2mcp_main_checkout "$ROOT")   # a linked worktree's shared caches live in the main checkout

DEPS=${MM_DEPS:-$ROOT/target/metamod-deps}
OUTROOT=${MM_OUT:-$ROOT/target/metamod}
# The reference clone (never modified): source of the pinned tree and of the submodule objects.
REF=${MM_REF:-$MAIN/target/ext/metamod-source}
# Metamod:Source commit to build: 170058a "Bump khook with the latest changes" (2026-10-18).
MM_COMMIT=${MM_COMMIT:-170058a2f0dbd58106b7c76bfe88a94672295356}
# khook submodule that commit pins (checked, not fetched).
KH_COMMIT_EXPECTED=${KH_COMMIT_EXPECTED:-0ca2b01363aff9fa11ad73b1b08dfdc2a195fb3a}
# The MSVC sysroot for the win64 cross build (crt + sdk).
XWIN=${XWIN:-$MAIN/target/xwin}

BASE_IMAGE=${MM_BASE_IMAGE:-ghcr.io/alliedmodders/build-containers/debian11-clang22:latest}
IMAGE=${MM_IMAGE:-localhost/mmsource-build:debian11-clang22}
CONTAINERFILE=$SCRIPT_DIR/Containerfile

# podman's rootless runtime dir: the project root's (bridge/lib-root.sh), unless already set.
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-$(cs2mcp_project_root "$ROOT")/.podman/xdg}

log()  { printf '[metamod] %s\n' "$*" >&2; }
fail() { printf '[metamod] FAIL: %s\n' "$*" >&2; exit 1; }

usage() {
  sed -n '3,15p' "${BASH_SOURCE[0]}" >&2
  exit 2
}

# ---------------------------------------------------------------- source cache

# Local clone of the reference clone (no network: the reference is a synced checkout with
# submodules). Submodule content is cloned out of the reference's .git/modules/<path> (recursively
# for nested submodules such as khook -> safetyhook), so the reference stays read-only and the
# copy is self-contained.
sync_src() {
  local src=$DEPS/src/metamod-source
  if [ -d "$src/.git" ]; then
    log "src cache hit: $src ($(git -C "$src" rev-parse --short HEAD))"
    return 0
  fi
  [ -d "$REF/.git" ] || fail "reference clone missing: $REF (set MM_REF)"
  log "src cold: cloning $REF -> $src (local clone, no network)"
  mkdir -p "$DEPS/src"
  git clone --quiet --no-hardlinks --no-checkout "$REF" "$src"
  clone_trees "$src" "$REF/.git/modules"
  log "src cold: cloned $(find "$src" -name .git | wc -l) git trees"
}

# clone_trees <dest repo> <module store dir>: for every gitlink in <dest>'s HEAD, clone the
# referenced tree out of <module store>/<path> into <dest>/<path> and recurse.
clone_trees() {
  local dest=$1 store=$2 path sha
  while read -r path sha; do
    [ -n "$path" ] || continue
    [ -d "$store/$path" ] || fail "reference clone has no objects for submodule $path ($store/$path)"
    log "src cold: cloning submodule $path @ $(git -C "$store/$path" rev-parse --short "$sha")"
    rm -rf "${dest:?}/$path"
    git clone --quiet --no-hardlinks --no-checkout "$store/$path" "$dest/$path"
    git -C "$dest/$path" checkout --quiet --detach --force "$sha"
    clone_trees "$dest/$path" "$store/$path/modules"
  done < <(git -C "$dest" ls-tree -r HEAD | awk '$1 == "160000" { print $4, $3 }')
}

# Verify every gitlink in <repo>'s HEAD is present at the pinned commit, recursively.
verify_trees() {
  local repo=$1 path sha want cur
  while read -r path sha; do
    [ -n "$path" ] || continue
    [ -d "$repo/$path/.git" ] || fail "submodule $path missing from $repo (remove $DEPS/src and rerun)"
    cur=$(git -C "$repo/$path" rev-parse HEAD)
    [ "$cur" = "$sha" ] || fail "submodule $path is at $cur, expected $sha (remove $DEPS/src and rerun)"
    verify_trees "$repo/$path"
  done < <(git -C "$repo" ls-tree -r HEAD | awk '$1 == "160000" { print $4, $3 }')
}

# Force every tree in the copy (and every nested clone) back to the revision it is checked out
# at: the copy is a cache, so a local adaptation from an earlier run (e.g. the win64 experiment)
# must never leak into a later build. Logs anything it had to discard.
reset_trees() {
  local repo=$1 dirty path sha
  dirty=$(git -C "$repo" status --porcelain --ignore-submodules=dirty | wc -l)
  git -C "$repo" checkout --quiet --force HEAD
  [ "$dirty" -eq 0 ] || log "reset $repo to HEAD ($dirty locally modified path(s) discarded)"
  while read -r path sha; do
    [ -n "$path" ] || continue
    [ -d "$repo/$path/.git" ] || continue
    reset_trees "$repo/$path"
  done < <(git -C "$repo" ls-tree -r HEAD | awk '$1 == "160000" { print $4, $3 }')
}

# Resolve and check out MM_COMMIT. Runs before anything under target/metamod is touched, so a
# wrong pin fails without disturbing previous outputs.
pin_src() {
  local src=$DEPS/src/metamod-source
  git -C "$REF" rev-parse --verify --quiet "${MM_COMMIT}^{commit}" >/dev/null \
    || fail "MM_COMMIT=$MM_COMMIT is not a commit in the reference clone $REF (refusing to build)"
  git -C "$src" rev-parse --verify --quiet "${MM_COMMIT}^{commit}" >/dev/null \
    || fail "MM_COMMIT=$MM_COMMIT is not in the source clone $src (refusing to build)"
  git -C "$src" checkout --quiet --detach --force "$MM_COMMIT"
  reset_trees "$src"
  verify_trees "$src"

  local kh
  kh=$(git -C "$src/third_party/khook" rev-parse HEAD)
  [ "$kh" = "$KH_COMMIT_EXPECTED" ] \
    || fail "khook is $kh, expected $KH_COMMIT_EXPECTED (the pin moved?)"
  log "pinned src: metamod $(git -C "$src" rev-parse --short HEAD), khook $(git -C "$src/third_party/khook" rev-parse --short HEAD)"
}

# ---------------------------------------------------------------- build image

ensure_image() {
  if [ "${MM_FORCE_IMAGE:-0}" != 1 ] && podman image exists "$IMAGE" 2>/dev/null; then
    log "image cache hit: $IMAGE"
  else
    log "building $IMAGE from $CONTAINERFILE (base $BASE_IMAGE)"
    podman build -t "$IMAGE" -f "$CONTAINERFILE" "$SCRIPT_DIR" || fail "podman build of $IMAGE failed"
  fi
}

# ---------------------------------------------------------------- container side

# Runs in the container as $1=$platform. The dependency cache and build tree live in /deps.
container_build() {
  local platform=$1 mounts=(-v "$DEPS:/deps")
  if [ "$platform" = win64 ]; then
    [ -d "$XWIN/crt" ] && [ -d "$XWIN/sdk" ] \
      || fail "win64 needs the MSVC sysroot at $XWIN/{crt,sdk} (set XWIN)"
    mounts+=(-v "$XWIN:/xwin:ro")
  fi
  podman run --rm -i \
    --name "mmsrc-$platform-$$" \
    --label "cs2mcp.metamod=$platform" \
    --network "${MM_NETWORK:-host}" \
    "${mounts[@]}" \
    "$IMAGE" bash -s -- "$platform" <<'CONTAINER_EOF'
set -euo pipefail
platform=$1
log() { printf '[container] %s\n' "$*" >&2; }
fail() { printf '[container] FAIL: %s\n' "$*" >&2; exit 1; }

src=/deps/src/metamod-source
[ -d "$src" ] || fail "no source at $src (host did not prepare it)"

# --- cs2 SDK (alliedmodders/s2sdk, branch cs2) -------------------------------------------
# Upstream CI runs this step in a folder that contains a "metamod-source" dir; the clone lands
# next to it. We keep the result in the mounted cache so later runs reuse it.
if [ -d /deps/hl2sdk-cs2 ]; then
  log "deps cache hit: /deps/hl2sdk-cs2 at $(git -C /deps/hl2sdk-cs2 rev-parse HEAD), not cloning"
else
  log "deps cold: checkout-deps.sh -s cs2 (clones s2sdk branch cs2)"
  cd /deps
  mkdir -p metamod-source
  "$src/support/checkout-deps.sh" -s cs2
fi
s2sdk_commit=$(git -C /deps/hl2sdk-cs2 rev-parse HEAD)

# --- build -------------------------------------------------------------------------------
build=/deps/build-$platform
mkdir -p "$build"
rm -rf /deps/stage-$platform
mkdir -p /deps/stage-$platform

{
  echo "metamod_commit=$(git -C "$src" rev-parse HEAD)"
  echo "metamod_describe=$(git -C "$src" describe --tags --always 2>/dev/null || git -C "$src" rev-parse --short HEAD)"
  echo "khook_commit=$(git -C "$src/third_party/khook" rev-parse HEAD)"
  echo "safetyhook_commit=$(git -C "$src/third_party/khook/third_party/safetyhook" rev-parse HEAD 2>/dev/null || echo none)"
  echo "hl2sdk_manifests_commit=$(git -C "$src/hl2sdk-manifests" rev-parse HEAD)"
  echo "amtl_commit=$(git -C "$src/third_party/amtl" rev-parse HEAD)"
  echo "s2sdk_commit=$s2sdk_commit"
  echo "s2sdk_branch=cs2"
  echo "ambuild_commit=$(cat /opt/ambuild-commit.txt 2>/dev/null || echo unknown)"
  echo "ambuild_api=$(python3 -c 'import ambuild2.run as r; print(getattr(r, "CURRENT_API", "unknown"))')"
  echo "python=$(python3 -V 2>&1)"
  echo "clang=$( { clang++ --version || clang++-22 --version || echo none; } 2>/dev/null | head -1)"
} > /deps/buildinfo-$platform.txt

if [ "$platform" = linux ]; then
  cd "$build"
  python3 "$src/configure.py" \
    --enable-optimize \
    --sdks=cs2 \
    --targets=x86_64 \
    --hl2sdk-root=/deps
  ambuild
  [ -d "$build/package/addons" ] || fail "build finished but $build/package/addons is missing"
  cp -a "$build/package/addons/." /deps/stage-$platform/addons/
elif [ "$platform" = win64 ]; then
  # ---------------------------------------------------------------------------------------
  # Windows x64 cross build: clang-cl + lld-link + the mounted xwin MSVC sysroot.
  #
  # AMBuild has no cross-compile story of its own (there is no --target-os; the only knob is
  # the `target=` phrase on DetectCxx, and MM:S passes only target_arch). On top of that, the
  # vendor of clang-cl is detected as "clang" from the probe program's macros
  # (ambuild2/frontend/cpp/verify.py:190), so it can never reach AMBuildScript's `cxx.like('msvc')`
  # branch and it inherits AMBuild's GCC conventions. Both are fixed below by rewriting the two
  # build scripts in this *copy* (the checked-out C++ sources are never touched); the diff is
  # captured in /deps/win64-buildsys.patch and recorded in BUILDINFO.txt.
  # ---------------------------------------------------------------------------------------
  ln -sf /usr/bin/clang-cl-22 /usr/local/bin/clang-cl
  ln -sf /usr/bin/lld-link-22 /usr/local/bin/lld-link
  ln -sf /usr/bin/lld-link-22 /usr/local/bin/link.exe
  ln -sf /usr/bin/llvm-lib-22 /usr/local/bin/llvm-lib
  ln -sf /usr/bin/llvm-ar-22 /usr/local/bin/llvm-ar
  ln -sf /usr/bin/llvm-rc-22 /usr/local/bin/llvm-rc
  ln -sf /usr/bin/llvm-windres-22 /usr/local/bin/llvm-windres
  ln -sf /usr/bin/llvm-readobj-22 /usr/local/bin/llvm-readobj
  for t in clang-cl lld-link llvm-lib llvm-ar llvm-rc llvm-windres llvm-readobj; do
    command -v "$t" >/dev/null || fail "win64: $t is missing from the build image"
  done
  [ -d /xwin/crt/include ] && [ -d /xwin/sdk/include/ucrt ] \
    || fail "win64: no MSVC sysroot at /xwin (mount the xwin crt/ and sdk/ trees)"
  export INCLUDE='/xwin/crt/include;/xwin/sdk/include/ucrt;/xwin/sdk/include/um;/xwin/sdk/include/shared;/xwin/sdk/include/winrt'
  export LIB='/xwin/crt/lib/x86_64;/xwin/sdk/lib/ucrt/x86_64;/xwin/sdk/lib/um/x86_64'
  export CC=clang-cl CXX=clang-cl RC=llvm-windres AR=llvm-ar
  export MMS_TARGET_PHRASE=windows-x86_64
  {
    echo "win64_cc=$(clang-cl --version | head -1)"
    echo "win64_linker=$(lld-link --version | head -1)"
    echo "win64_sysroot=$INCLUDE | $LIB"
  } >> /deps/buildinfo-win64.txt

  # Reset the two files to the pinned revision, then patch them (idempotent across reruns).
  git -C "$src" checkout -- AMBuildScript
  git -C "$src/hl2sdk-manifests" checkout -- SdkHelpers.ambuild
  cd "$src"
  python3 - <<'PATCH_PY' || fail "win64: build-script patch no longer matches the source (upstream moved?)"
import sys
edits = [
    # 1. Tell AMBuild which platform we are building for (MM:S only passes target_arch).
    ('AMBuildScript',
     '            cxx = builder.DetectCxx(target_arch = arch)',
     "            mms_target_phrase = os.environ.get('MMS_TARGET_PHRASE')\n"
     "            if mms_target_phrase:\n"
     "                cxx = builder.DetectCxx(target = mms_target_phrase)\n"
     "            else:\n"
     "                cxx = builder.DetectCxx(target_arch = arch)"),
    # 2/3. A windows target built by clang-cl takes the MSVC flag/link set (clang-cl speaks it),
    #      not the gcc one it would get from its detected vendor family.
    ('AMBuildScript',
     "    if cxx.behavior == 'gcc':\n      cxx.defines += [\n        'stricmp=strcasecmp',",
     "    if cxx.behavior == 'gcc' and cxx.target.platform != 'windows':\n      cxx.defines += [\n        'stricmp=strcasecmp',"),
    ('AMBuildScript',
     "    elif cxx.like('msvc'):\n",
     "    elif cxx.like('msvc') or cxx.target.platform == 'windows':\n"),
    # 4. Same for the SDK glue: the non-msvc branch would define COMPILER_GCC/_LINUX/POSIX.
    ('hl2sdk-manifests/SdkHelpers.ambuild',
     "        if cxx.family == 'msvc':\n",
     "        if cxx.family == 'msvc' or cxx.target.platform == 'windows':\n"),
    ('hl2sdk-manifests/SdkHelpers.ambuild',
     "            if cxx.version >= 1900:\n                cxx.linkflags += ['legacy_stdio_definitions.lib']\n",
     "            if cxx.family != 'msvc' or cxx.version >= 1900:\n                cxx.linkflags += ['legacy_stdio_definitions.lib']\n"),
]
for path, old, new in edits:
    with open(path, 'rt') as fp:
        text = fp.read()
    if text.count(old) != 1:
        sys.stderr.write('win64 patch anchor not unique/found in %s: %r\n' % (path, old[:60]))
        sys.exit(1)
    with open(path, 'wt') as fp:
        fp.write(text.replace(old, new))
print('patched: ' + ', '.join(sorted({p for p, _, _ in edits})))
PATCH_PY
  git -C "$src" diff --ignore-submodules=dirty > /deps/win64-buildsys.patch
  git -C "$src/hl2sdk-manifests" diff | sed 's#^\(--- \|+++ \)a/#\1hl2sdk-manifests/#' >> /deps/win64-buildsys.patch
  log "win64 build-script patch: $(sha256sum /deps/win64-buildsys.patch | cut -d' ' -f1) ($(grep -c '^[+-][^+-]' /deps/win64-buildsys.patch) changed lines)"

  # A cross build tree is not worth caching across patch revisions; configure from scratch.
  rm -rf "$build"
  mkdir -p "$build"
  cd "$build"
  python3 "$src/configure.py" \
    --enable-optimize \
    --sdks=cs2 \
    --targets=x86_64 \
    --hl2sdk-root=/deps || fail "win64: configure.py failed"
  set +e
  ambuild > /deps/win64-ambuild.log 2>&1
  rc=$?
  set -e
  if [ $rc -ne 0 ]; then
    log "win64: ambuild failed (rc=$rc); first errors:"
    grep -n -m 12 -E "error:|fatal error|FAILED|Error" /deps/win64-ambuild.log >&2 || true
    log "win64: tail of /deps/win64-ambuild.log:"
    tail -25 /deps/win64-ambuild.log >&2
    fail "win64 cross build failed (full log: target/metamod-deps/win64-ambuild.log)"
  fi
  [ -d "$build/package/addons" ] || fail "win64: build finished but $build/package/addons is missing"
  cp -a "$build/package/addons/." /deps/stage-$platform/addons/
else
  fail "unknown platform $platform"
fi

find /deps/stage-$platform -type f | sort > /deps/stage-$platform.files
log "staged $(wc -l < /deps/stage-$platform.files) files"
CONTAINER_EOF
}

# ---------------------------------------------------------------- win64: blocked in AMBuild

# AMBuild cannot cross-compile with clang-cl. Everything below is measured, not guessed; the
# probe subcommand reproduces the toolchain half and `win64-attempt` reproduces the AMBuild half.
win64_blocked() {
  cat >&2 <<'BLOCKED'
[metamod] STOP: no win64 binaries. The blocker is AMBuild (alliedmodders/ambuild), not this
[metamod] script, the image or the sysroot:
[metamod]   1. no cross-compile target support. AMBuild's only knob is `target=` on DetectCxx
[metamod]      (ambuild2/frontend/v2_2/cpp/detect.py:95-126); MM:S's AMBuildScript passes only
[metamod]      target_arch, and there is no --target-os/--target_platform in `ambuild --help`.
[metamod]      AMBuild's own windows cross path is mingw-only (`find_default_compiler`,
[metamod]      detect.py:384-391 -> x86_64-w64-mingw32-*), whose C++ ABI is not the game's.
[metamod]   2. clang-cl is always vendor "clang". The vendor comes from the probe program's macros
[metamod]      (ambuild2/frontend/cpp/verify.py:190-212), and clang-cl defines __clang__, so
[metamod]      AMBuildScript's `cxx.like('msvc')` branch (AMBuildScript:174) is unreachable and the
[metamod]      gcc conventions apply: objectArgs/dependencyArgv (`-MD -MF dep`, gcc.py:66-69) which
[metamod]      clang-cl ignores (`-MD` means /MD, `-MF` is "unknown argument ignored"), and
[metamod]      `-m64` from the arch rules (cpp_rules.py:28), which is a gcc spelling.
[metamod]   3. the SDK glue keys off the compiler family, not the target
[metamod]      (hl2sdk-manifests/SdkHelpers.ambuild:229: non-msvc -> COMPILER_GCC, _LINUX, LINUX,
[metamod]      POSIX), and picks protoc by target platform (SdkTarget.__init__ ->
[metamod]      devtools/bin/protoc.exe, a PE binary that cannot run here without wine).
[metamod] Making this work means patching three pinned upstream repositories (ambuild, MM:S,
[metamod] hl2sdk-manifests) into a hand-rolled cross toolchain - i.e. a new upstream feature, well
[metamod] past this task's budget, and it would no longer be the recipe upstream CI runs. The
[metamod] win64 plugin should come from the GitHub Actions release artifact of the same commit
[metamod] (built with MSVC on a Windows runner); that decision is the orchestrator's.
[metamod] Evidence (reproducible):
[metamod]   bridge/metamod/build-metamod.sh win64-probe    toolchain is fine: clang-cl + lld-link +
[metamod]                                                the xwin sysroot link a real DLL and
[metamod]                                                llvm-readobj shows its export.
[metamod]   bridge/metamod/build-metamod.sh win64-attempt  the same build driven through AMBuild:
[metamod]                                                fails at AMBuild's compiler probe /
[metamod]                                                (with the two build scripts patched) log:
[metamod]                                                target/metamod-deps/win64-ambuild.log
BLOCKED
  exit 3
}

# Toolchain probe for win64: compile + link a DLL with clang-cl/lld-link/xwin and dump its exports.
# Fails loudly (non-zero) if any piece is missing; it never invents a result.
container_probe() {
  [ -d "$XWIN/crt" ] && [ -d "$XWIN/sdk" ] || fail "win64 needs the MSVC sysroot at $XWIN/{crt,sdk} (set XWIN)"
  podman run --rm -i \
    --name "mmsrc-probe-$$" \
    --label "cs2mcp.metamod=probe" \
    --network "${MM_NETWORK:-host}" \
    -v "$XWIN:/xwin:ro" \
    "$IMAGE" bash -s <<'PROBE_EOF'
set -euo pipefail
log() { printf '[probe] %s\n' "$*" >&2; }
fail() { printf '[probe] FAIL: %s\n' "$*" >&2; exit 1; }

for t in clang-cl-22 lld-link-22 llvm-lib-22 llvm-rc-22 llvm-windres-22 llvm-readobj-22; do
  [ -x "/usr/bin/$t" ] || fail "/usr/bin/$t missing from the build image (LLVM 22 toolchain)"
done
mkdir -p /usr/local/bin
ln -sf /usr/bin/clang-cl-22 /usr/local/bin/clang-cl
ln -sf /usr/bin/lld-link-22 /usr/local/bin/lld-link
ln -sf /usr/bin/llvm-lib-22 /usr/local/bin/llvm-lib
ln -sf /usr/bin/llvm-rc-22 /usr/local/bin/llvm-rc
ln -sf /usr/bin/llvm-windres-22 /usr/local/bin/llvm-windres
ln -sf /usr/bin/llvm-readobj-22 /usr/local/bin/llvm-readobj
[ -f /xwin/crt/include/vcruntime.h ] || fail "no MSVC CRT headers at /xwin/crt/include"
[ -f /xwin/crt/include/vector ] || fail "no MSVC STL headers at /xwin/crt/include (vector)"
[ -f /xwin/sdk/include/ucrt/stdio.h ] || fail "no Windows SDK headers at /xwin/sdk/include/ucrt"
[ -f /xwin/sdk/lib/um/x86_64/kernel32.lib ] || fail "no Windows SDK libs at /xwin/sdk/lib/um/x86_64"
export INCLUDE='/xwin/crt/include;/xwin/sdk/include/ucrt;/xwin/sdk/include/um;/xwin/sdk/include/shared;/xwin/sdk/include/winrt'
export LIB='/xwin/crt/lib/x86_64;/xwin/sdk/lib/ucrt/x86_64;/xwin/sdk/lib/um/x86_64'

work=/deps/probe-win64
rm -rf "$work"; mkdir -p "$work"; cd "$work"
cat > p.cpp <<'SRC'
#include <string>
#include <vector>
extern "C" __declspec(dllexport) void *CreateInterface(const char *name, int *rc) {
  std::vector<std::string> v{name ? name : ""};
  if (rc) *rc = 0;
  return (void *)v.size();
}
SRC
set -x
clang-cl --version | head -1
clang-cl -std:c++17 -W3 -c p.cpp -o p.obj
lld-link /DLL /OUT:probe.dll /SUBSYSTEM:WINDOWS p.obj kernel32.lib user32.lib msvcprt.lib libcmt.lib
llvm-readobj --coff-exports probe.dll
{ set +x; } 2>/dev/null
file probe.dll
log "toolchain OK: $work/probe.dll $(llvm-readobj --coff-exports probe.dll | grep -c 'Name:') exports"
PROBE_EOF
}

# ---------------------------------------------------------------- host side output

publish() {
  local platform=$1
  [ -d "$DEPS/stage-$platform/addons" ] || fail "container left no package at $DEPS/stage-$platform/addons"
  mkdir -p "$OUTROOT/$platform"
  rm -rf "${OUTROOT:?}/$platform/addons"
  cp -a "$DEPS/stage-$platform/addons" "$OUTROOT/$platform/addons"
  log "published $OUTROOT/$platform/addons"
  write_buildinfo
}

write_buildinfo() {
  local platform part=$DEPS/buildinfo-$platform.txt
  local out=$OUTROOT/BUILDINFO.txt
  local tmp=$OUTROOT/.BUILDINFO.txt.tmp
  [ -f "$part" ] || fail "missing $part (the container did not run to completion)"
  {
    echo "Metamod:Source build info — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo
    echo "## source"
    cat "$part"
    echo "s2sdk_pinned_by_upstream_lock=$(awk -F'"' '/^commit/ { print $2 }' "$ROOT/crates/s2sdk/s2sdk-sys/UPSTREAM.lock")"
    echo "source_clone=$DEPS/src/metamod-source"
    echo
    echo "## image"
    echo "base_image=$BASE_IMAGE"
    echo "base_image_digest=$(podman image inspect --format '{{.RepoDigests}}' "$BASE_IMAGE" 2>/dev/null | head -1)"
    echo "build_image=$IMAGE"
    echo "build_image_id=$(podman image inspect --format '{{.Id}}' "$IMAGE" 2>/dev/null)"
    echo "containerfile_sha256=$(sha256sum "$CONTAINERFILE" | cut -d' ' -f1)"
    if [ -f "$DEPS/win64-buildsys.patch" ]; then
      echo "win64_buildsystem_patch_sha256=$(sha256sum "$DEPS/win64-buildsys.patch" | cut -d' ' -f1)"
      echo "win64_buildsystem_patch_file=$DEPS/win64-buildsys.patch (applied to the copy in target/metamod-deps/src; C++ sources untouched)"
    fi
    echo
    for p in linux win64; do
      echo "## outputs: $p"
      if [ -d "$OUTROOT/$p/addons" ]; then
        ( cd "$OUTROOT/$p" && find addons -type f | sort | while read -r f; do
            printf 'sha256 %s  %s\nfile %s: %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$f" "$f" "$(file -b "$f")"
          done )
      elif [ "$p" = win64 ]; then
        echo "NOT BUILT — see the win64 STOP in bridge/metamod/README.md: AMBuild has no"
        echo "clang-cl/windows cross-compile support, so the plugin must come from the upstream"
        echo "GitHub Actions win64 release artifact of the same commit (orchestrator's call)."
        echo "Reproduce: bridge/metamod/build-metamod.sh win64 (blocker list), win64-probe"
        echo "(toolchain OK), win64-attempt (AMBuild failure log: target/metamod-deps/win64-ambuild.log)."
      else
        echo "(not built)"
      fi
      echo
    done
  } > "$tmp"
  mv "$tmp" "$out"
  log "wrote $out"
}

# ---------------------------------------------------------------- main

cmd=${1:-}
case "$cmd" in
  linux)
    sync_src
    pin_src
    ensure_image
    container_build "$cmd" || fail "$cmd build failed (container exited non-zero)"
    publish "$cmd"
    ;;
  win64)
    sync_src
    pin_src
    win64_blocked
    ;;
  win64-attempt)
    sync_src
    pin_src
    ensure_image
    container_build win64 || fail "win64 cross build failed (see target/metamod-deps/win64-ambuild.log)"
    publish win64
    ;;
  win64-probe)
    ensure_image
    container_probe
    ;;
  image)
    ensure_image
    podman image inspect --format '{{.Id}} {{.RepoDigests}}' "$IMAGE"
    ;;
  all)
    sync_src
    pin_src
    ensure_image
    container_build linux || fail "linux build failed"
    publish linux
    win64_blocked
    ;;
  *)
    usage
    ;;
esac
