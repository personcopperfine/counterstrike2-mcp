#!/bin/bash
# Fail unless <file>'s exported functions are exactly {CreateInterface}.
#
# The Rust cdylib must be a drop-in for the C bridge's `mcp_bridge.dll`, whose only exported
# symbol is `CreateInterface` (`bridge/mcp_bridge.c:6846`: the single `__declspec(dllexport)` in
# that file). One extra export is a leak of Rust internals into the game's symbol space and, for
# a tool plugin, an invitation for the engine to call something that was never meant to be an
# entry point - so this is a hard gate, not a warning.
#
# Usage: bridge/check-exports.sh <file.dll|file.so>
set -euo pipefail

FILE="${1:?usage: check-exports.sh <file.dll|file.so>}"
[ -f "$FILE" ] || { echo "check-exports.sh: no such file: $FILE" >&2; exit 2; }

READOBJ="${READOBJ:-llvm-readobj}"
NM="${NM:-llvm-nm}"
[ -x "$(command -v "$READOBJ")" ] || { echo "check-exports.sh: $READOBJ not found (llvm tools)" >&2; exit 2; }
[ -x "$(command -v "$NM")" ] || { echo "check-exports.sh: $NM not found (llvm tools)" >&2; exit 2; }

# Allowed beyond CreateInterface: EMPTY, on purpose.
#
# PE: the Rust cdylib's export table is exactly what `#[no_mangle]` asks for
# (`llvm-readobj --coff-exports` on the staged DLL: one entry, `CreateInterface`).
#
# ELF: the linker adds no symbol of its own for this toolchain - `llvm-readelf --dyn-syms` on the
# staged .so shows 53 dynamic symbols, 52 undefined imports from libc/libgcc and exactly one
# defined: `CreateInterface`. So nothing is allow-listed. If a future toolchain does add a
# linker-generated dynamic symbol (`_init`/`_fini`/`__bss_start`/`_edata`/`_end` are the usual
# candidates), this gate fails and the name must be justified here before it is added - the point
# of the check is that the export set is exactly `CreateInterface`, not that it is close to it.
ALLOWED=()
STATUS=0
case "$FILE" in
*.dll)
    # llvm-readobj --coff-exports prints one "  Name: <symbol>" per exported name and one
    # "  Ordinal: <n>" per exported entry. A block with an ordinal but no name is an export that
    # slips past a name-only check, so the two counts must agree.
    EXPORT_DUMP="$("$READOBJ" --coff-exports "$FILE")"
    ORDINALS="$(printf '%s\n' "$EXPORT_DUMP" | grep -c '^  *Ordinal: ' || true)"
    mapfile -t NAMES < <(printf '%s\n' "$EXPORT_DUMP" | sed -n 's/^  *Name: //p' | sort -u)
    if [ "$ORDINALS" != "${#NAMES[@]}" ]; then
        echo "check-exports.sh: $FILE has $ORDINALS export entries but ${#NAMES[@]} named ones:" \
             "an unnamed (ordinal-only) export cannot be reviewed and is refused" >&2
        STATUS=1
    fi

    # The DLL must not depend on a CRT the tools' Wine prefix may not carry. `+crt-static`
    # (.cargo/config.toml) is what keeps those imports out; if RUSTFLAGS or
    # CARGO_ENCODED_RUSTFLAGS in the environment replaced that config, the link still succeeds and
    # the DLL silently starts importing vcruntime140.dll - so the artifact is checked, not the
    # intent.
    for imp in $("$READOBJ" --coff-imports "$FILE" | sed -n 's/^  *Name: //p'); do
        case "$imp" in
        VCRUNTIME*|vcruntime*|MSVCP*|msvcp*|api-ms-win-crt-*|ucrtbase*|CONCRT*|concrt*)
            echo "check-exports.sh: $FILE imports '$imp': the DLL is not crt-static" \
                 "(-Ctarget-feature=+crt-static is missing from the rustflags; see .cargo/config.toml)" >&2
            STATUS=1
            ;;
        esac
    done
    ;;
*.so)
    mapfile -t NAMES < <("$NM" -D --defined-only "$FILE" | awk 'NF>=3 { print $NF }' | sort -u)
    ;;
*)
    echo "check-exports.sh: $FILE is neither a .dll nor a .so" >&2
    exit 2
    ;;
esac

echo "check-exports.sh: $FILE exports ${#NAMES[@]} name(s):"
printf '  %s\n' "${NAMES[@]:-<none>}"

if ! printf '%s\n' "${NAMES[@]}" | grep -qx 'CreateInterface'; then
    echo "check-exports.sh: MISSING CreateInterface in $FILE" >&2
    STATUS=1
fi

for name in "${NAMES[@]}"; do
    [ -n "$name" ] || continue
    [ "$name" = "CreateInterface" ] && continue
    allowed=0
    for a in "${ALLOWED[@]:-}"; do
        [ "$name" = "$a" ] && allowed=1 && break
    done
    if [ "$allowed" = 0 ]; then
        echo "check-exports.sh: UNEXPECTED export '$name' in $FILE" >&2
        STATUS=1
    fi
done

[ "$STATUS" = 0 ] || { echo "check-exports.sh: FAILED for $FILE" >&2; exit 1; }
echo "check-exports.sh: OK - $FILE exports exactly CreateInterface"
