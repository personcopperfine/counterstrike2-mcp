#!/bin/sh
# Fetch the Windows sysroot gen.py's `--abi msvc` pass needs.
#
# The MSVC target cannot use the host's glibc/libstdc++ headers (they fail with
# __float128/__TC__ errors), and there are no MSVC headers on a Linux box, so the
# MSVC CRT + STL and the Windows SDK headers come from xwin's `splat`: it
# downloads the Microsoft build and drops them as a plain include tree under
# target/xwin/ (gitignored, like everything else under target/).
#
# This is the generator's only download, and it is not silent: it installs xwin
# with cargo and passes `--accept-license` to it, which accepts Microsoft's
# license for the SDK bits it downloads (~1.5 GB). Run it yourself, or point
# $S2SDK_XWIN at a sysroot you already have, if you would rather do that
# knowingly. gen.py fails with this script's path when target/xwin is missing.
#
# The versions are not pinned yet (clang's MS compatibility version either), so
# a re-run can move the STL under a layout; the MSVC pass records the clang
# version and the sysroot it used for that reason. XWIN_VERSION=<x.y.z> pins the
# xwin tool itself (CI does, to key its sysroot cache on it).
#
#   ./s2sdk/s2sdk-sys/gen/fetch-xwin.sh        # from the repository root
#   python3 -I crates/s2sdk/s2sdk-sys/gen/gen.py --abi msvc --ir-only
#
# Any working directory is fine: the script cd's to the repository root.
set -eu

cd "$(dirname "$0")/../../../.."

if [ -d target/xwin/crt/include ] && [ -d target/xwin/sdk/include/ucrt ]; then
	echo "fetch-xwin: target/xwin already has a sysroot; nothing to do"
	exit 0
fi

echo "fetch-xwin: installing xwin into target/xwin-bin (cargo)"
cargo install xwin --locked ${XWIN_VERSION:+--version "$XWIN_VERSION"} --root target/xwin-bin
echo "fetch-xwin: downloading the MSVC CRT/STL + Windows SDK into target/xwin (accepting Microsoft's license)"
target/xwin-bin/bin/xwin --accept-license --cache-dir target/xwin-cache --arch x86_64 splat --output target/xwin

echo "fetch-xwin: wrote target/xwin (MSVC CRT/STL + Windows SDK headers)"
