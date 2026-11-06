#!/usr/bin/env bash
#
# fetch-metamod-win64.sh — the win64 Metamod:Source for the package: the latest official
# AlliedModders 2.0 dev build for Windows, from the mmsdrop listing (AMBuild cannot cross-build
# win64 here: build-metamod.sh's win64 STOP). Not pinned on purpose: every release takes the newest
# dev build, and records exactly which one it took.
#
#   bridge/metamod/fetch-metamod-win64.sh  -> target/metamod/win64/addons/metamod/
#                                               bin/win64/server.dll
#                                               bin/win64/metamod.2.cs2.dll
#                                               metaplugins.ini
#                                             target/metamod/win64/BUILDINFO-download.txt
#                                               (build name, version, URL, archive SHA-256, file SHA-256s)
#
# Only those three files are taken (what crates/cs2-native/src/plan.rs installs on Windows); the
# loader vdfs and the other games' DLLs in the archive are not.
#
# Environment: MMSDROP (listing base, default https://mms.alliedmods.net/mmsdrop/2.0),
# MM_WIN64_ZIP=<file> (use this archive instead of downloading; still hashed and recorded),
# MM_WIN64_SHA256=<hex> (refuse an archive with another hash), MM_OUT (default target/metamod).
# Downloads go to target/metamod-dl/ (never /tmp). Needs curl and python3.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
BASE=${MMSDROP:-https://mms.alliedmods.net/mmsdrop/2.0}
OUTROOT=${MM_OUT:-$ROOT/target/metamod}
DL=$ROOT/target/metamod-dl

log()  { printf '[metamod-win64] %s\n' "$*" >&2; }
fail() { printf '[metamod-win64] FAIL: %s\n' "$*" >&2; exit 1; }

mkdir -p "$DL"
if [ -n "${MM_WIN64_ZIP:-}" ]; then
  zip=$MM_WIN64_ZIP
  name=$(basename "$zip")
  url="(local file: $zip)"
else
  # the listing's pointer file names the newest build
  name=$(curl -fsSL --max-time 60 --retry 3 "$BASE/mmsource-latest-windows" | tr -d '\r\n ')
  [[ "$name" =~ ^mmsource-2\.0\.0-git[0-9]+-windows\.zip$ ]] || fail "unexpected latest-windows entry: '$name'"
  url="$BASE/$name"
  zip=$DL/$name
  if [ -s "$zip" ]; then
    log "cached $zip"
  else
    log "downloading $url"
    curl -fsSL --max-time 600 --retry 3 -o "$zip.part" "$url"
    mv "$zip.part" "$zip"
  fi
fi
[ -s "$zip" ] || fail "no archive at $zip"
sha=$(sha256sum "$zip" | cut -d' ' -f1)
if [ -n "${MM_WIN64_SHA256:-}" ] && [ "$sha" != "$MM_WIN64_SHA256" ]; then
  fail "$name: sha256 $sha, expected MM_WIN64_SHA256=$MM_WIN64_SHA256"
fi
version=$(sed -n 's/^mmsource-\(.*\)-windows\.zip$/\1/p' <<<"$name")

stage=$OUTROOT/.win64.tmp
rm -rf "$stage"
mkdir -p "$stage/addons/metamod/bin/win64"
# member by member, by exact name: no archive path is trusted to pick its own destination
python3 -I - "$zip" "$stage" <<'PY'
import sys, zipfile, pathlib
zip_path, stage = sys.argv[1], pathlib.Path(sys.argv[2])
want = ["addons/metamod/bin/win64/server.dll", "addons/metamod/bin/win64/metamod.2.cs2.dll", "addons/metamod/metaplugins.ini"]
with zipfile.ZipFile(zip_path) as z:
    names = set(z.namelist())
    for w in want:
        if w not in names:
            sys.exit(f"{w} is not in {zip_path}")
        data = z.read(w)
        if w.endswith(".dll") and data[:2] != b"MZ":
            sys.exit(f"{w} is not a PE file")
        (stage / w).write_bytes(data)
PY

{
  echo "Metamod:Source win64 — the official AlliedModders dev build (downloaded, not built here)"
  echo "fetched_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "build=$name"
  echo "version=$version"
  echo "url=$url"
  echo "archive_sha256=$sha"
  echo "listing=$BASE/ (mmsource-latest-windows)"
  ( cd "$stage" && find addons -type f | sort | while read -r f; do
      printf 'sha256 %s  %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$f"
    done )
} > "$stage/BUILDINFO-download.txt"

rm -rf "$OUTROOT/win64"
mv "$stage" "$OUTROOT/win64"
log "staged $OUTROOT/win64 from $name (sha256 $sha)"
cat "$OUTROOT/win64/BUILDINFO-download.txt"
