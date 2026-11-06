#!/usr/bin/env bash
#
# fetch-metamod.sh linux|win64 — Metamod:Source for the package: the latest official
# AlliedModders 2.0 dev build, from the mmsdrop listing. Not pinned on purpose: every release takes
# the newest dev build, and records exactly which one it took. (`cargo bridge metamod` builds
# Linux from source for development; releases do not use it, and AMBuild cannot cross-build win64
# here.)
#
#   crates/metamod/fetch-metamod.sh win64 -> target/metamod/win64/addons/metamod/
#                                              bin/win64/server.dll
#                                              bin/win64/metamod.2.cs2.dll
#                                              metaplugins.ini
#   crates/metamod/fetch-metamod.sh linux -> target/metamod/linux/addons/metamod/
#                                              bin/linux64/server.so
#                                              bin/linuxsteamrt64/libserver.so
#                                              bin/linuxsteamrt64/metamod.2.cs2.so
#                                              metaplugins.ini, README.txt
#   each with target/metamod/<plat>/BUILDINFO-download.txt
#                                             (build name, version, URL, archive SHA-256, file SHA-256s)
#
# Only those files are taken (what crates/cs2-native/src/plan.rs and `cargo bridge install` install:
# the CS2 loader and core); the other games' binaries and the VSP loader vdfs in the archive are not.
#
# Environment: MMSDROP (listing base, default https://mms.alliedmods.net/mmsdrop/2.0),
# MM_ARCHIVE=<file> (use this archive instead of downloading; still hashed and recorded),
# MM_SHA256=<hex> (refuse an archive with another hash; MM_WIN64_SHA256 / MM_LINUX_SHA256 are the
# per-platform spellings the release workflow passes), MM_OUT (default target/metamod).
# Downloads go to target/metamod-dl/ (never /tmp). Needs curl and python3.
set -euo pipefail

plat=${1:-}
case "$plat" in
  win64) listing=mmsource-latest-windows suffix=windows ext=zip
         want_sha=${MM_SHA256:-${MM_WIN64_SHA256:-}}
         want=(addons/metamod/bin/win64/server.dll addons/metamod/bin/win64/metamod.2.cs2.dll
               addons/metamod/metaplugins.ini) ;;
  linux) listing=mmsource-latest-linux suffix=linux ext=tar.gz
         want_sha=${MM_SHA256:-${MM_LINUX_SHA256:-}}
         want=(addons/metamod/bin/linux64/server.so addons/metamod/bin/linuxsteamrt64/libserver.so
               addons/metamod/bin/linuxsteamrt64/metamod.2.cs2.so addons/metamod/metaplugins.ini
               addons/metamod/README.txt) ;;
  *) echo "usage: $0 linux|win64" >&2; exit 2 ;;
esac

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
BASE=${MMSDROP:-https://mms.alliedmods.net/mmsdrop/2.0}
OUTROOT=${MM_OUT:-$ROOT/target/metamod}
DL=$ROOT/target/metamod-dl

log()  { printf '[metamod-%s] %s\n' "$plat" "$*" >&2; }
fail() { printf '[metamod-%s] FAIL: %s\n' "$plat" "$*" >&2; exit 1; }

mkdir -p "$DL"
archive=${MM_ARCHIVE:-${MM_WIN64_ZIP:-}}
if [ -n "$archive" ]; then
  name=$(basename "$archive")
  url="(local file: $archive)"
else
  # the listing's pointer file names the newest build
  name=$(curl -fsSL --max-time 60 --retry 3 "$BASE/$listing" | tr -d '\r\n ')
  [[ "$name" =~ ^mmsource-2\.0\.0-git[0-9]+-$suffix\.${ext//./\\.}$ ]] || fail "unexpected $listing entry: '$name'"
  url="$BASE/$name"
  archive=$DL/$name
  if [ -s "$archive" ]; then
    log "cached $archive"
  else
    log "downloading $url"
    curl -fsSL --max-time 600 --retry 3 -o "$archive.part" "$url"
    mv "$archive.part" "$archive"
  fi
fi
[ -s "$archive" ] || fail "no archive at $archive"
sha=$(sha256sum "$archive" | cut -d' ' -f1)
if [ -n "$want_sha" ] && [ "$sha" != "$want_sha" ]; then
  fail "$name: sha256 $sha, expected $want_sha"
fi
version=$(sed -n "s/^mmsource-\(.*\)-$suffix\.$ext\$/\1/p" <<<"$name")

stage=$OUTROOT/.$plat.tmp
rm -rf "$stage"
mkdir -p "$stage"
# member by member, by exact name: no archive path is trusted to pick its own destination, and
# only regular files are taken (no link in a tarball can point the write elsewhere)
python3 -I - "$archive" "$stage" "${want[@]}" <<'PY'
import sys, zipfile, tarfile, pathlib
src, stage, want = sys.argv[1], pathlib.Path(sys.argv[2]), sys.argv[3:]
def members():
    if src.endswith(".zip"):
        with zipfile.ZipFile(src) as z:
            names = set(z.namelist())
            for w in want:
                yield w, (z.read(w) if w in names else None)
    else:
        with tarfile.open(src, "r:gz") as t:
            byname = {m.name.removeprefix("./"): m for m in t.getmembers()}
            for w in want:
                m = byname.get(w)
                yield w, (t.extractfile(m).read() if m is not None and m.isreg() else None)
for w, data in members():
    if data is None:
        sys.exit(f"{w} is not a regular file in {src}")
    if w.endswith(".dll") and data[:2] != b"MZ":
        sys.exit(f"{w} is not a PE file")
    if w.endswith(".so") and data[:4] != b"\x7fELF":
        sys.exit(f"{w} is not an ELF file")
    p = stage / w
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)
    if w.endswith(".so"):
        p.chmod(0o755)
PY

{
  echo "Metamod:Source $plat — the official AlliedModders dev build (downloaded, not built here)"
  echo "fetched_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "build=$name"
  echo "version=$version"
  echo "url=$url"
  echo "archive_sha256=$sha"
  echo "listing=$BASE/ ($listing)"
  ( cd "$stage" && find addons -type f | sort | while read -r f; do
      printf 'sha256 %s  %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$f"
    done )
} > "$stage/BUILDINFO-download.txt"

rm -rf "${OUTROOT:?}/$plat"
mv "$stage" "$OUTROOT/$plat"
log "staged $OUTROOT/$plat from $name (sha256 $sha)"
cat "$OUTROOT/$plat/BUILDINFO-download.txt"
