# The container half of `cargo bridge metamod linux|win64-attempt|all` (src/metamod/mod.rs):
# piped to `bash -s -- <platform>` inside the build image (crates/metamod/Containerfile) with the
# dependency cache target/metamod-deps mounted at /deps (and, for win64, the xwin sysroot at
# /xwin, read-only). The host has already cloned and pinned /deps/src/metamod-source; this side
# brings in the cs2 SDK, records the build info, runs configure.py + ambuild and leaves the
# package in /deps/stage-<platform>, which the host publishes only after this exits 0.
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
