/* json_text.h - bridge-side JSON string-body unescaper for pet.apply (m2.13 WP-B1).
 *
 * pet.apply carries the whole document as a JSON string; the bridge has no JSON library
 * (freestanding, no CRT), so this is the exact inverse of the escape set the Rust client's
 * serde_json emits: `\" \\ \/ \b \f \n \r \t` and `\uXXXX`. Anything else is refused, never
 * guessed: a lone surrogate, a `\u0000`, a `\uXXXX` above 0x7F (serde writes non-ASCII as raw
 * UTF-8, so an escaped non-ASCII is a different producer), an unknown escape, an unterminated
 * string, and a raw byte < 0x20 inside the string all fail closed. The caller sizes the output
 * from the escaped length (unescaping never grows: every escape is at least one byte in, one
 * byte out).
 *
 * Portability: pure C, no #include, no libc, no Windows types - the only requirement is the
 * includer's u64 typedef (bridge/mcp_bridge.c's own; test_json_text.c redefines the identical
 * one). No static mutable state: re-entrant.
 *
 *   json_text_end(src, srcLen)
 *       Returns a pointer to the closing quote of the string whose body starts at `src` (i.e.
 *       `src` is the byte after the opening quote), or 0 when there is none within srcLen. Used
 *       for sizing. An escaped quote (`\"`) does not end the string.
 *
 *   json_text_unescape(src, srcLen, out, outCap, *outLen, *reason)
 *       1 = ok, `*outLen` set, `out` holds that many bytes (not NUL-terminated - the text is
 *       binary-safe). 0 = `*reason` is one of:
 *         "unterminated"       no closing quote within srcLen (or a trailing backslash)
 *         "bad_escape"         an unknown escape, or a bad hex digit in `\uXXXX`
 *         "nul"                `\u0000`
 *         "non_ascii_escape"   `\uXXXX` with XXXX > 0x7F (not a lone surrogate)
 *         "surrogate"          `\uXXXX` in 0xD800..0xDFFF
 *         "control_byte"       a raw byte < 0x20 inside the string
 *         "too_long"           a decoded byte or the terminating NUL would exceed outCap
 *   On accept out[*outLen] == 0 (the text goes to C-string APIs such as tier0's LoadKV3).
 */
#ifndef JSON_TEXT_H
#define JSON_TEXT_H

static const char *__attribute__((unused)) json_text_end(const char *src, u64 srcLen) {
    u64 i = 0;
    while (i < srcLen) {
        char c = src[i];
        if (c == '\\') {                    /* the byte after an escape is never the closing quote */
            if (i + 1 >= srcLen) return 0;
            i += 2;
            continue;
        }
        if (c == '"') return src + i;
        i++;
    }
    return 0;
}

static int json_text_unescape(const char *src, u64 srcLen, char *out, u64 outCap, u64 *outLen,
                              const char **reason) {
    u64 i = 0, o = 0;
    *outLen = 0;
    *reason = "unterminated";
    while (i < srcLen) {
        unsigned c = (unsigned char)src[i];
        if (c == '"') {                     /* the closing quote: done */
            if (o >= outCap) { *reason = "too_long"; return 0; }
            out[o] = 0;                     /* callers hand `out` to C-string APIs (LoadKV3) */
            *outLen = o;
            return 1;
        }
        if (c == '\\') {
            unsigned e;
            i++;
            if (i >= srcLen) { *reason = "unterminated"; return 0; }
            e = (unsigned char)src[i++];
            switch (e) {
                case '"':  c = '"';  break;
                case '\\': c = '\\'; break;
                case '/':  c = '/';  break;
                case 'b':  c = '\b'; break;
                case 'f':  c = '\f'; break;
                case 'n':  c = '\n'; break;
                case 'r':  c = '\r'; break;
                case 't':  c = '\t'; break;
                case 'u': {
                    u64 v = 0;
                    int k;
                    if (i + 4 > srcLen) { *reason = "unterminated"; return 0; }
                    for (k = 0; k < 4; k++) {
                        unsigned h = (unsigned char)src[i + k];
                        unsigned d;
                        if (h >= '0' && h <= '9') d = h - '0';
                        else if (h >= 'a' && h <= 'f') d = h - 'a' + 10;
                        else if (h >= 'A' && h <= 'F') d = h - 'A' + 10;
                        else { *reason = "bad_escape"; return 0; }
                        v = v * 16 + d;
                    }
                    i += 4;
                    if (v == 0) { *reason = "nul"; return 0; }
                    if (v > 0x7F) {
                        *reason = (v >= 0xD800 && v <= 0xDFFF) ? "surrogate" : "non_ascii_escape";
                        return 0;
                    }
                    c = (unsigned)v;
                    break;
                }
                default: *reason = "bad_escape"; return 0;
            }
        } else {
            if (c < 0x20) { *reason = "control_byte"; return 0; }
            i++;
        }
        if (o + 1 > outCap) { *reason = "too_long"; return 0; }
        out[o++] = (char)c;
    }
    *reason = "unterminated";
    return 0;
}

#endif /* JSON_TEXT_H */
