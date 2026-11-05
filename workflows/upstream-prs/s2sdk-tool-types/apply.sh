#!/bin/sh
# Apply the s2sdk tool-types patches onto alliedmodders/s2sdk cs2 (base ab23a7e3) as commits
# authored by the current git identity. The patches carry no author line on purpose.
# Usage: run inside the s2sdk checkout: sh /path/to/apply.sh
set -e
dir=$(cd "$(dirname "$0")" && pwd)
name=$(git config user.name); email=$(git config user.email)
[ -n "$name" ] && [ -n "$email" ] || { echo "set git user.name and user.email first" >&2; exit 1; }
for p in "$dir"/*.patch; do
	sed "2i From: $name <$email>" "$p" | git am -q --keep-cr
done
echo "applied $(ls "$dir"/*.patch | wc -l) patches"
