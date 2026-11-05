/* json_text_gen.c - the committed generator of the committed differential corpus for the Rust
 * port of bridge/json_text.h (WP4.H1 round 2).
 *
 * Includes the bridge header read-only and records, for every case, what the COMPILED C
 * produces - hex-encoded source, capacity, and result - as TAB-separated lines. Its stdout is
 * the committed corpus crates/cs2-bridge/tests/data/json_text_corpus.txt: regenerate with the
 * commands below and the Rust test json_text.rs::corpus_matches_the_c_generator fails if the
 * two ever drift apart.
 *
 * Build & run (from the repo root; the probe fixture is our own tracked file
 * hammer-mcp/fixtures/vpcf/probe.vpcf):
 *   cc -std=c11 -O1 -Wall -Wextra -I bridge \
 *       -o target/scratch/json_text_gen crates/cs2-bridge/tests/data/json_text_gen.c
 *   target/scratch/json_text_gen hammer-mcp/fixtures/vpcf/probe.vpcf \
 *       > crates/cs2-bridge/tests/data/json_text_corpus.txt
 *
 * Case list: the 13 unescape/end cases of bridge/test_json_text.c exactly as that test builds
 * their src (accepted bodies carry their appended closing quote, test_json_text.c:37; the
 * check_cap cases keep their small caps), the probe-fixture roundtrip both as an unescape and
 * an end case, plus the negative/passthrough extras the Rust test gates on: raw invalid UTF-8,
 * 0x7F, \u007f, lone u-escape fragments, raw NUL, zero-length input, cap 0, exact-cap accepts.
 *
 * Build & run:
 *   cc -std=c11 -O1 -Wall -Wextra -I bridge \
 *       -o target/scratch/gen_corpus target/scratch/gen_corpus.c
 *   target/scratch/gen_corpus hammer-mcp/fixtures/vpcf/probe.vpcf > target/scratch/json_text_corpus.txt
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef unsigned long long u64;      /* the bridge's own typedef; json_text.h needs only this */
#include "json_text.h"

static void hex_write(const unsigned char *p, size_t n) {
    static const char *hex = "0123456789abcdef";
    for (size_t i = 0; i < n; i++) {
        putchar(hex[p[i] >> 4]);
        putchar(hex[p[i] & 15]);
    }
}

/* one unescape case: src is the string BODY (closing quote included where the C test appends
   it); cap is the output capacity. Records ok + the NUL-terminated decoded bytes, or err + the
   C reason string. */
static void u_case(const char *name, const unsigned char *src, u64 srcLen, u64 cap) {
    static unsigned char out[65536];
    const char *reason = 0;
    u64 outLen = 0;
    int got = json_text_unescape((const char *)src, srcLen, (char *)out, cap, &outLen, &reason);
    printf("U\t%s\t", name);
    hex_write(src, (size_t)srcLen);
    printf("\t%llu\t", (unsigned long long)cap);
    if (got) {
        if (outLen + 1 > sizeof out) { printf("FAIL corpus: outLen overflow\n"); exit(2); }
        printf("ok\t");
        hex_write(out, (size_t)outLen + 1);      /* the NUL terminator travels with the bytes */
    } else {
        printf("err\t%s", reason);
    }
    putchar('\n');
}

static void e_case(const char *name, const unsigned char *src, u64 srcLen) {
    const char *e = json_text_end((const char *)src, srcLen);
    printf("E\t%s\t", name);
    hex_write(src, (size_t)srcLen);
    printf("\t%lld\n", e ? (long long)(e - (const char *)src) : -1LL);
}

/* byte-array builder so cases can carry raw control/invalid-UTF-8 bytes */
static unsigned char body[65536];
static u64 bodyLen;
static void reset(void) { bodyLen = 0; }
static void put(unsigned char c) { body[bodyLen++] = c; }
static void puts_(const char *s) { while (*s) put((unsigned char)*s++); }

/* read_file/escape are the C test's own (test_json_text.c:76-113), verbatim */
static size_t escape(const unsigned char *in, size_t n, unsigned char *out, size_t cap) {
    static const char *hex = "0123456789abcdef";
    size_t o = 0, i;
    for (i = 0; i < n; i++) {
        unsigned c = in[i];
        if (c == '"' || c == '\\') {
            if (o + 2 > cap) { printf("FAIL escape: buffer too small\n"); exit(2); }
            out[o++] = '\\'; out[o++] = (unsigned char)c;
        } else if (c < 0x20) {
            if (o + 6 > cap) { printf("FAIL escape: buffer too small\n"); exit(2); }
            out[o++] = '\\'; out[o++] = 'u'; out[o++] = '0'; out[o++] = '0';
            out[o++] = (unsigned char)hex[c >> 4]; out[o++] = (unsigned char)hex[c & 15];
        } else {
            out[o++] = (unsigned char)c;
        }
    }
    if (o >= cap) { printf("FAIL escape: buffer too small\n"); exit(2); }
    out[o] = 0;
    return o;
}

static unsigned char *read_file(const char *path, size_t *len) {
    FILE *f = fopen(path, "rb");
    unsigned char *buf;
    long n;
    if (!f) { printf("FAIL fixture: cannot open %s\n", path); exit(2); }
    fseek(f, 0, SEEK_END);
    n = ftell(f);
    if (n < 0) { printf("FAIL fixture: cannot size %s\n", path); exit(2); }
    fseek(f, 0, SEEK_SET);
    buf = (unsigned char *)malloc((size_t)n + 1);
    if (!buf) { printf("FAIL fixture: out of memory\n"); exit(2); }
    if (fread(buf, 1, (size_t)n, f) != (size_t)n) { printf("FAIL fixture: short read\n"); exit(2); }
    fclose(f);
    *len = (size_t)n;
    return buf;
}

int main(int argc, char **argv) {
    long long count = 0;

    if (argc != 2) { printf("usage: gen_corpus <probe.vpcf>\n"); return 2; }

    /* the 11 check/check_cap cases of test_json_text.c:185-196, same src construction */
    reset(); puts_("\\n\\t\\r\\\"\\\\\\/"); put('"');                      /* quote appended */
    u_case("unescape_short_escapes", body, bodyLen, 4096); count++;
    reset(); puts_("\\u0041"); put('"');
    u_case("unescape_u_ascii", body, bodyLen, 4096); count++;
    reset(); puts_("\\u0000");
    u_case("refuse_u0000", body, bodyLen, 4096); count++;
    reset(); puts_("\\u00e9");
    u_case("refuse_u_non_ascii", body, bodyLen, 4096); count++;
    reset(); puts_("\\ud83d");
    u_case("refuse_surrogate", body, bodyLen, 4096); count++;
    reset(); puts_("abc");
    u_case("refuse_unterminated", body, bodyLen, 4096); count++;
    reset(); puts_("abc\\");
    u_case("refuse_unterminated_backslash", body, bodyLen, 4096); count++;
    reset(); put('a'); put(0x09); put('b');                               /* raw tab, as the C */
    u_case("refuse_raw_control_byte", body, bodyLen, 4096); count++;
    reset(); puts_("\\x");
    u_case("refuse_bad_escape", body, bodyLen, 4096); count++;
    reset(); puts_("abcdef");
    u_case("refuse_too_long", body, bodyLen, 3); count++;
    reset(); puts_("abc"); put('"');
    u_case("refuse_no_room_for_nul", body, bodyLen, 3); count++;

    /* the 2 end_cases of test_json_text.c:116-137: src is "a\\\"b\\\"c\"junk" minus last byte */
    reset(); puts_("a\\\"b\\\"c\"junk"); bodyLen--;
    e_case("end_skips_escaped_quote", body, bodyLen); count++;
    reset(); puts_("abc");
    e_case("end_unterminated_is_null", body, bodyLen); count++;

    /* the probe roundtrip, as the C test builds it (test_json_text.c:149-166) */
    {
        size_t rawLen, escLen;
        unsigned char *raw = read_file(argv[1], &rawLen);
        unsigned char *esc = (unsigned char *)malloc(rawLen * 6 + 1);
        if (!esc) { printf("FAIL roundtrip: out of memory\n"); exit(2); }
        escLen = escape(raw, rawLen, esc, rawLen * 6 + 1);
        /* memcpy does not advance bodyLen: bump it explicitly (put() would write body[0]).
           body = esc + '"', the src the C test builds at test_json_text.c:150-155. */
        reset(); memcpy(body, esc, escLen); body[escLen] = '"'; bodyLen = (u64)escLen + 1;
        u_case("roundtrip_probe_unescape", body, bodyLen, (u64)rawLen + 1); count++;
        /* the C test's end runs over json+1 with length escLen+1: the escaped body plus the
           closing quote json[escLen+1] (test_json_text.c:147-156) - same buffer as above */
        e_case("roundtrip_probe_end", body, bodyLen); count++;
        free(raw); free(esc);
    }

    /* the negative/passthrough extras the Rust test gates on */
    reset(); put('"');
    u_case("accept_empty_body", body, bodyLen, 8); count++;
    reset(); puts_("ab"); put('"');
    u_case("accept_exact_cap", body, bodyLen, 3); count++;
    reset(); puts_("a"); put('"');
    u_case("refuse_cap_zero", body, bodyLen, 0); count++;
    reset(); puts_("a"); put(0x80); put(0xC3); put(0x28); put(0xFF); puts_("z"); put('"');
    u_case("accept_raw_invalid_utf8", body, bodyLen, 4096); count++;
    reset(); put(' '); put(0x7F); put('~'); put('"');
    u_case("accept_raw_del_and_space", body, bodyLen, 4096); count++;
    reset(); puts_("\\u00");
    u_case("refuse_u_truncated", body, bodyLen, 4096); count++;
    reset(); puts_("\\u00g0");
    u_case("refuse_u_bad_hex", body, bodyLen, 4096); count++;
    reset(); puts_("\\u007f"); put('"');
    u_case("accept_u007f", body, bodyLen, 4096); count++;
    reset(); puts_("\\/\\\""); put('"');
    u_case("accept_slash_quote", body, bodyLen, 4096); count++;
    reset(); put('a'); put(0x00); put('b'); put('"');
    u_case("refuse_raw_nul", body, bodyLen, 4096); count++;
    reset();
    u_case("refuse_zero_length_input", body, bodyLen, 4096); count++;

    /* end extras */
    reset(); puts_("a\\\"b\\");
    e_case("end_trailing_backslash", body, bodyLen); count++;
    reset();
    e_case("end_empty_input", body, bodyLen); count++;
    reset(); put('"'); puts_("x");
    e_case("end_first_byte", body, bodyLen); count++;

    printf("COUNT\t%lld\n", count);
    return 0;
}
