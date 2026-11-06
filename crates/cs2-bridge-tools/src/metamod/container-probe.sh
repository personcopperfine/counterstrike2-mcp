# The container half of `cargo bridge metamod win64-probe` (src/metamod/mod.rs): piped to
# `bash -s` inside the build image with the xwin sysroot mounted at /xwin (read-only). It proves
# the win64 toolchain alone - clang-cl + lld-link + the sysroot compile and link a real DLL and
# llvm-readobj shows its export - and fails loudly when any piece is missing; it never invents a
# result. /deps is not mounted here: the probe DLL stays in the (--rm) container.
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
