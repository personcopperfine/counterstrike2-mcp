/* map_path.h - the pure helpers of `map.open` (m2.16).
 *
 * `map.open` takes a path addon-content-relative (`maps/x.vmap`): the form Hammer's own
 * `CBaseEditorApp::OpenFile` resolves itself against CONTENT then GAME. Three helpers keep that one
 * call free of all three of its failure modes (idabins docs/flows/file-edit-reload-compile.md §B):
 *
 *   map_open_path_ok     refuses every path shape that could make OpenFile pop a modal box
 *                        ("Cannot open ...", "outside the content dir") and block the tools main
 *                        thread until a person clicks it. Only a CANONICAL file under the addon's
 *                        maps/ is accepted (review M1: Win32 opens `maps//x.vmap`, `maps/./x.vmap`,
 *                        `maps/sub./x.vmap` and `maps/sub /x.vmap` for the same file, so a second
 *                        spelling would miss the session-leak gate and leak a session; the gate
 *                        refuses them instead of normalising, so only one spelling reaches
 *                        OpenFile).
 *   map_vmap_header_ok   refuses a file that does not carry the DMX header the crate's own reader
 *                        accepts for binary v9 (review H1: OpenFileInternal pops a modal
 *                        "Error opening file" QMessageBox on a load failure, which wedges the
 *                        drain - the bridge is the second line of defence, the Rust tool parses
 *                        the whole file with `hammer-mcp/src/vmap` before it sends anything).
 *   map_path_eq          decides whether a session the app already holds IS the file being asked
 *                        for. OpenFileInternal's own session-match slot (35, +0x118) is the
 *                        `xor al,al; ret` stub `hammer!0x18009DBC0` in this build, so it never
 *                        matches ANY session and calling OpenFile for an already-open file creates a
 *                        SECOND, independent session for the same file (the first one is leaked and
 *                        keeps its own undo history and view state). map.open walks the vector
 *                        (S2_OFF_CBaseEditorApp_m_Sessions: count, data pointer at +8) itself first and answers `existing:true`
 *                        instead of calling OpenFile.
 *
 * All three are pure: no Windows API, no game objects, no libc. The host unit test
 * (bridge/test_map_path.c, wired into build.sh) covers exactly this shipped text, with the path
 * shapes the live runtime really has (`Z:\app\content\csgo_addons\mcp_test\maps\...`).
 *
 * Portability: the only requirement is the includer's u8/u32 typedefs (bridge/mcp_bridge.c's own;
 * test_map_path.c redefines the identical ones). No static mutable state: re-entrant.
 */
#ifndef MAP_PATH_H
#define MAP_PATH_H

/* 512, the same bound modeldoc.open/pet.open put on their addon-relative path (the Rust tools
   refuse a longer one before dispatch). */
#define MAP_PATH_MAX 512u

/* How many leading bytes of a .vmap map_vmap_header_ok needs: the header is
   `<!-- dmx encoding binary 9 format <fmt> <ver> -->` plus the NUL that ends it, and the
   `<fmt>`/`<ver>` tail is not fixed, so the caller hands over a window and the helper looks for
   the NUL inside it. 512 is far more than any DMX header (live: 43 bytes for a vmap) and a
   caller can always grow it. */
#define MAP_VMAP_HEADER_MAX 512u

/* 1 when c is an ASCII separator in either spelling */
static int __attribute__((unused)) map_sep(char c) { return c == '\\' || c == '/'; }

/* ASCII whitespace: the byte-level twin of the reader's `split_whitespace` for a DMX header.
   Rust's char::is_whitespace also accepts the Unicode spaces and the reader accepts 0x85/0xA0;
   no DMX writer emits one inside the header, and a refusal there is the safe direction. */
static int __attribute__((unused)) map_hdr_ws(char c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\v' || c == '\f';
}

/* lowercase an ASCII byte (only ASCII is folded: a UTF-8 byte passes through) */
static int __attribute__((unused)) map_lc(char c) {
    u8 b = (u8)c;
    return (b >= 'A' && b <= 'Z') ? (b | 0x20) : b;
}

/* 1 when `path` is a usable addon-content-relative map path, 0 = `*reason` names the gate that
 * refused it (the caller turns it into `-32602 InvalidParams {reason}`; the reasons are the
 * sentences the Rust tool's own local check mirrors, so a refused path is never dispatched).
 *
 * Refused: an empty path, one over MAP_PATH_MAX bytes, a control character or 0x7F, an absolute
 * form (`\x`, `/x`, `C:x`), a `:` (a Windows file name cannot contain one: what it would name is an
 * alternate data stream, never a map), a `\` (a canonical map path uses '/' only), anything that is
 * not a `.vmap`, anything not under `maps/`, an empty basename (`maps/.vmap`), two consecutive dots
 * (the same `..` rule modeldoc.open/pet.open use: the host gate and OpenFile's content-dir check
 * both take a traversed path as it is spelled, so only `..` needs a refusal here), and the review
 * M1 canonical-form rules: an empty segment (`//`), a `.`/`..` segment, a segment ending in `.` or
 * a space (`maps/.vmap` is caught by the basename rule above with the clearer sentence).
 */
static int __attribute__((unused)) map_open_path_ok(const char *path, const char **reason) {
    u32 n = 0, i, base;
    *reason = "?";
    if (!path || !path[0]) {
        *reason = "path: a non-empty path relative to the addon content root (maps/... .vmap) is required";
        return 0;
    }
    while (path[n]) n++;
    if (n > MAP_PATH_MAX) {
        *reason = "path: 1..512 chars";
        return 0;
    }
    if (map_sep(path[0]) || (n > 1 && path[1] == ':')) {
        *reason = "path: relative to the addon content root (maps/...), not absolute";
        return 0;
    }
    for (i = 0; i < n; i++) {
        u8 c = (u8)path[i];
        if (c < 0x20 || c == 0x7f) {
            *reason = "path: no control characters";
            return 0;
        }
        if (c == ':') {
            /* a Windows file name cannot contain ':'; the only thing it can name is an alternate
               data stream of another file, which is not a map */
            *reason = "path: no ':'";
            return 0;
        }
        if (c == '\\') {
            /* review M1: the canonical spelling uses '/' (the Rust tool refuses '\' too, so this
               is the raw-client gate). A '\' would also be folded by map_path_eq, which is exactly
               the second-spelling problem the canonical form removes. */
            *reason = "path: no '\\' (use '/' separators)";
            return 0;
        }
    }
    if (n < 6 || path[n - 5] != '.' || map_lc(path[n - 4]) != 'v' || map_lc(path[n - 3]) != 'm'
        || map_lc(path[n - 2]) != 'a' || map_lc(path[n - 1]) != 'p') {
        *reason = "path: a .vmap file";
        return 0;
    }
    if (!(map_lc(path[0]) == 'm' && map_lc(path[1]) == 'a' && map_lc(path[2]) == 'p'
          && map_lc(path[3]) == 's' && path[4] == '/')) {
        *reason = "path: under maps/ (the addon's map directory)";
        return 0;
    }
    base = 5;                                   /* the first byte after "maps/" */
    if (n <= base + 5) {                        /* "maps/" + ".vmap": a name with no basename */
        *reason = "path: a map file name under maps/";
        return 0;
    }
    for (i = 0; i + 1 < n; i++) {               /* the same `..` rule modeldoc.open/pet.open use */
        if (path[i] == '.' && path[i + 1] == '.') {
            *reason = "path: no '..'";
            return 0;
        }
    }
    /* review M1: canonical segments only (the shape gate is where the second spelling dies).
       A segment is [start, i) with i at the terminating '/' or at n; the last one cannot be empty
       (the ".vmap" suffix check above), so an empty segment is always a `//`. */
    {
        u32 start = base;
        for (i = base; i <= n; i++) {
            if (i != n && path[i] != '/') continue;
            if (i == start) {
                *reason = "path: no empty path segment ('//')";
                return 0;
            }
            if (i - start == 1 && path[start] == '.') {
                *reason = "path: no '.' or '..' path segment";
                return 0;
            }
            if (path[i - 1] == '.' || path[i - 1] == ' ') {
                *reason = "path: no path segment ending in '.' or a space";
                return 0;
            }
            start = i + 1;                         /* the next segment starts after the '/' */
        }
    }
    return 1;
}

/* 1 when `head` (the first `len` bytes of a .vmap) carries a DMX header binary v9 accepts, 0 =
 * `*reason` names why not (the caller logs it; the wire answer is `InvalidMapFile`).
 *
 * This mirrors the reader's own precondition (`hammer-mcp/src/vmap/dmx.rs`, oracle
 * `bridge/vmapreader/vmap_dmx.py`): the file starts with the bytes of the header up to a NUL, the
 * header starts with `<!-- dmx `, its first three whitespace-separated words are `encoding`,
 * `binary` and the encoding version, and there are at least six words (encoding, binary, 9,
 * format, <fmt>, <ver>). The encoding version must be 9 and the word after the first three must be
 * present - a `binary 8` or a `keyvalues2` header is refused here, exactly as the reader refuses
 * it. The first 16 bytes are NOT enough to tell those apart (`<!-- dmx encodin` is shared by all
 * three spellings), which is why this compares the words.
 *
 * The reader also accepts `parts[3]` being any word (it never checks one), and a version word with
 * leading zeros or a sign; this accepts those too (both parse to the same i32), so nothing the
 * reader would accept is refused here.
 */
static int __attribute__((unused)) map_vmap_header_ok(const char *head, u32 len, const char **reason) {
    u32 n = 0, i, words = 0;
    *reason = "?";
    if (!head || !len) {
        *reason = "empty file";
        return 0;
    }
    while (n < len && head[n]) n++;                /* the header string ends at its NUL */
    if (n == len) {
        *reason = "no NUL-terminated DMX header in the first bytes";
        return 0;
    }
    if (n < 8 || head[0] != '<' || head[1] != '!' || head[2] != '-' || head[3] != '-'
        || head[4] != ' ' || head[5] != 'd' || head[6] != 'm' || head[7] != 'x') {
        *reason = "not a DMX file (the header must start with '<!-- dmx ')";
        return 0;
    }
    for (i = 8; i < n;) {
        u32 start;
        while (i < n && map_hdr_ws(head[i])) i++;
        if (i >= n) break;
        start = i;
        while (i < n && !map_hdr_ws(head[i])) i++;
        words++;
        if (words == 1) {
            if (i - start != 8 || head[start] != 'e' || head[start + 1] != 'n'
                || head[start + 2] != 'c' || head[start + 3] != 'o' || head[start + 4] != 'd'
                || head[start + 5] != 'i' || head[start + 6] != 'n' || head[start + 7] != 'g') {
                *reason = "the DMX header's first word is not 'encoding'";
                return 0;
            }
        } else if (words == 2) {
            if (i - start != 6 || head[start] != 'b' || head[start + 1] != 'i'
                || head[start + 2] != 'n' || head[start + 3] != 'a' || head[start + 4] != 'r'
                || head[start + 5] != 'y') {
                *reason = "the DMX encoding is not 'binary'";
                return 0;
            }
        } else if (words == 3) {
            u32 k = start, v = 0;
            if (k < i && (head[k] == '+' || head[k] == '-')) k++;
            if (k >= i) {                      /* "" or a lone sign: the reader cannot parse it */
                *reason = "the DMX binary encoding version is not a number";
                return 0;
            }
            for (; k < i; k++) {
                if (head[k] < '0' || head[k] > '9') {
                    *reason = "the DMX binary encoding version is not a number";
                    return 0;
                }
                v = v * 10 + (u32)(head[k] - '0');
                if (v > 9999u) v = 9999u;      /* saturate: any other value is "not 9" anyway */
            }
            if (head[start] == '-' || v != 9) {
                *reason = "the DMX binary encoding version is not 9";
                return 0;
            }
        }
    }
    if (words < 6) {
        *reason = "the DMX header is too short (encoding, binary, 9, format, <fmt>, <ver>)";
        return 0;
    }
    return 1;
}

/* byte equality of n bytes, folding ASCII case and both separator spellings */
static int __attribute__((unused)) map_path_eq_n(const char *a, const char *b, u32 n) {
    u32 i;
    for (i = 0; i < n; i++) {
        int ca = map_lc(a[i]), cb = map_lc(b[i]);
        if (map_sep((char)ca)) ca = '/';
        if (map_sep((char)cb)) cb = '/';
        if (ca != cb) return 0;
    }
    return 1;
}

/* 1 when the two paths name the same file. Separators (`\` and `/`) and ASCII case are folded and a
 * trailing separator is dropped; after that the two must be byte-equal.
 *
 * REVIEW L2: the relative-tail rule is gone. OpenFileInternal always stores a full path (it uses
 * either `V_IsAbsolutePath` or the CONTENT/GAME `RelativePathToFullPath`, and returns 0 when
 * neither resolves), and the resolved host path map.open compares against is always absolute
 * (`editor_app_root` + `kv3_host_path`), so a tail match can never be the right answer - it could
 * only claim a DIFFERENT file (another addon's `maps/x.vmap`). Two paths that are not equal after
 * folding are two files. Non-ASCII case and 8.3 short names are not folded (also Low in the
 * review); neither is a component's trailing `.`/space, which cannot occur on either side
 * (map_open_path_ok refuses it in the requested path, and Hammer stores what its resolver found).
 */
static int __attribute__((unused)) map_path_eq(const char *a, const char *b) {
    u32 la = 0, lb = 0;
    if (!a || !b) return 0;
    while (a[la]) la++;
    while (b[lb]) lb++;
    while (la && map_sep(a[la - 1])) la--;      /* a trailing separator is not part of the name */
    while (lb && map_sep(b[lb - 1])) lb--;
    if (!la || !lb) return 0;
    return la == lb && map_path_eq_n(a, b, la);
}

#endif /* MAP_PATH_H */
