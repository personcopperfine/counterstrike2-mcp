#!/bin/bash
# The loader's export gate: one symbol, the one the loader needs and nothing else.
#
#   cs2mcp_mmload.dll   (Wine)  : exactly {DllMain}
#   libcs2mcp_mmload.so (Linux) : exactly {dlopen}
#
# The loader is not a plugin. `DllMain` is not an API either: the PE loader calls it through the
# image's entry point (`_DllMainCRTStartup` -> `DllMain`), it is not an entry point the game looks
# up. It is allowed because rustc's cdylib export list contains every `pub` `#[no_mangle]` item and
# `DllMain` is one; that is the only name the image has today (`llvm-readobj --coff-exports` on the
# staged DLL: one entry, `DllMain`, RVA 0x1FC0). An ordinal-only export cannot be reviewed, so it
# is refused outright.
#
# On Linux the interposer must define exactly `dlopen`: every other `dlopen` in the process goes
# through it, so an extra definition (a Rust helper that leaked into the dynamic symbol table, a
# second interposer symbol) would be new behaviour in the dynamic loader's namespace. The linker
# adds no symbol of its own for this toolchain (`llvm-readelf --dyn-syms` on the staged .so: one
# defined entry, `dlopen`; the rest are undefined libc/libgcc imports).
#
# The DLL is also held to `+crt-static` for the same reason as the bridge (`check-exports.sh`): it
# is injected into cs2.exe under Wine, whose prefix does not necessarily carry vcruntime140.dll. The
# check reads the artifact, not the intent, because an environment `RUSTFLAGS` would silently
# replace the `.cargo/config.toml` flags and the link would still succeed.
#
# Usage: bridge/check-loader-exports.sh <file.dll|file.so>
set -euo pipefail

FILE="${1:?usage: check-loader-exports.sh <file.dll|file.so>}"
[ -f "$FILE" ] || { echo "check-loader-exports.sh: no such file: $FILE" >&2; exit 2; }

READOBJ="${READOBJ:-llvm-readobj}"
NM="${NM:-llvm-nm}"
[ -x "$(command -v "$READOBJ")" ] || { echo "check-loader-exports.sh: $READOBJ not found (llvm tools)" >&2; exit 2; }
[ -x "$(command -v "$NM")" ] || { echo "check-loader-exports.sh: $NM not found (llvm tools)" >&2; exit 2; }

STATUS=0
case "$FILE" in
*.dll)
    EXPECTED=("DllMain")
    EXPORT_DUMP="$("$READOBJ" --coff-exports "$FILE")"
    ORDINALS="$(printf '%s\n' "$EXPORT_DUMP" | grep -c '^  *Ordinal: ' || true)"
    mapfile -t NAMES < <(printf '%s\n' "$EXPORT_DUMP" | sed -n 's/^  *Name: //p' | sort -u)
    if [ "$ORDINALS" != "${#NAMES[@]}" ]; then
        echo "check-loader-exports.sh: $FILE has $ORDINALS export entries but ${#NAMES[@]} named ones:" \
             "an unnamed (ordinal-only) export cannot be reviewed and is refused" >&2
        STATUS=1
    fi

    for imp in $("$READOBJ" --coff-imports "$FILE" | sed -n 's/^  *Name: //p'); do
        case "$imp" in
        VCRUNTIME*|vcruntime*|MSVCP*|msvcp*|api-ms-win-crt-*|ucrtbase*|CONCRT*|concrt*)
            echo "check-loader-exports.sh: $FILE imports '$imp': the DLL is not crt-static" \
                 "(-Ctarget-feature=+crt-static is missing from the rustflags; see .cargo/config.toml)" >&2
            STATUS=1
            ;;
        esac
    done
    ;;
*.so)
    EXPECTED=("dlopen")
    mapfile -t NAMES < <("$NM" -D --defined-only "$FILE" | awk 'NF>=3 { print $NF }' | sort -u)
    ;;
*)
    echo "check-loader-exports.sh: $FILE is neither a .dll nor a .so" >&2
    exit 2
    ;;
esac

echo "check-loader-exports.sh: $FILE exports ${#NAMES[@]} name(s):"
printf '  %s\n' "${NAMES[@]:-<none>}"

for want in "${EXPECTED[@]}"; do
    if ! printf '%s\n' "${NAMES[@]}" | grep -qx "$want"; then
        echo "check-loader-exports.sh: MISSING $want in $FILE" >&2
        STATUS=1
    fi
done

for name in "${NAMES[@]}"; do
    [ -n "$name" ] || continue
    allowed=0
    for want in "${EXPECTED[@]}"; do
        [ "$name" = "$want" ] && allowed=1 && break
    done
    if [ "$allowed" = 0 ]; then
        echo "check-loader-exports.sh: UNEXPECTED export '$name' in $FILE" >&2
        STATUS=1
    fi
done

[ "$STATUS" = 0 ] || { echo "check-loader-exports.sh: FAILED for $FILE" >&2; exit 1; }
echo "check-loader-exports.sh: OK - $FILE exports exactly ${EXPECTED[*]}"
