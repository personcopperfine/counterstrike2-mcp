/* entity_json_gen.c - the differential corpus generator for the C entity JSON region.
 *
 * Reads the committed case list (crates/cs2-bridge/tests/data/entity_json_cases.txt, one
 * `<op> <arg>...` per line) and prints one deterministic line per case on stdout. Each line
 * echoes its inputs and then the results of the region function the op names, with every byte
 * string as lowercase hex ('-' = empty), every float as its IEEE bit pattern in hex and every
 * integer in decimal. crates/cs2-bridge/tests/data/entity_json_corpus.txt is this program's
 * output, committed; `crates/cs2-bridge/tests/entity_json.rs` rebuilds this file with `cc`
 * against the region extracted from bridge/mcp_bridge.c (the build.sh extraction) and requires
 * its stdout and the Rust port's own output to equal the committed bytes.
 *
 * Usage: entity_json_gen <cases file>          (region header on the include path)
 */
#define _POSIX_C_SOURCE 200809L
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>

typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;

#include "entity_json_region.h"

#define INMAX 8192

static char a1[INMAX], a2[INMAX];

static int hexv(char c) {
    if (c >= '0' && c <= '9') return c - '0';
    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
    return -1;
}

/* decode the hex token `t` into `out` (NUL-terminated), zeroing the whole buffer first so a
   scanner reading past the terminator (a `start` beyond the string) sees zeros, exactly like
   the Rust port's `at()`. `-` is the empty string. */
static size_t unhex(const char *t, char *out) {
    memset(out, 0, INMAX);
    if (!t || strcmp(t, "-") == 0) return 0;
    size_t n = 0;
    for (const char *p = t; p[0]; p += 2) {
        int hi = hexv(p[0]);
        int lo = p[1] ? hexv(p[1]) : -1;
        if (hi < 0 || lo < 0) { fprintf(stderr, "gen: bad hex token %s\n", t); exit(2); }
        if (n + 1 >= INMAX) { fprintf(stderr, "gen: token too long\n"); exit(2); }
        out[n++] = (char)((hi << 4) | lo);
    }
    out[n] = 0;
    return n;
}

static void phex(const char *s) {
    if (!s || !*s) { fputs("-", stdout); return; }
    for (const u8 *p = (const u8 *)s; *p; p++) printf("%02x", *p);
}

static u32 fbits(float f) { u32 u; memcpy(&u, &f, 4); return u; }
static u64 dbits(double d) { u64 u; memcpy(&u, &d, 8); return u; }
static float vf32(const char *t) { u32 u = (u32)strtoul(t, 0, 16); float f; memcpy(&f, &u, 4); return f; }
static void pf32(u32 u) { printf("%08x", u); }
static void pf64(u64 u) { printf("%016llx", (unsigned long long)u); }

static long long atoll_arg(const char *t) { return strtoll(t, 0, 10); }
static unsigned long long atoull_arg(const char *t) { return strtoull(t, 0, 10); }
static long atol_arg(const char *t) { return strtol(t, 0, 10); }

/* `30,45` or `-` (empty) -> out[]. */
static int parse_ids(const char *t, int *out, int cap) {
    int n = 0;
    if (!t || strcmp(t, "-") == 0 || !*t) return 0;
    const char *p = t;
    while (*p) {
        if (n >= cap) { fprintf(stderr, "gen: too many ids\n"); exit(2); }
        char *end = 0;
        long v = strtol(p, &end, 10);
        if (end == p) { fprintf(stderr, "gen: bad id list %s\n", t); exit(2); }
        out[n++] = (int)v;
        p = end;
        if (*p == ',') p++;
        else if (*p) { fprintf(stderr, "gen: bad id list %s\n", t); exit(2); }
    }
    return n;
}

static void pids(const int *v, int n) {
    if (n <= 0) { fputs("-", stdout); return; }
    for (int i = 0; i < n; i++) printf("%s%d", i ? "," : "", v[i]);
}

int main(int argc, char **argv) {
    if (argc != 2) { fprintf(stderr, "usage: entity_json_gen <cases file>\n"); return 2; }
    FILE *f = fopen(argv[1], "r");
    if (!f) { fprintf(stderr, "gen: cannot open %s\n", argv[1]); return 2; }

    char *line = 0;
    size_t linecap = 0;
    ssize_t got;
    while ((got = getline(&line, &linecap, f)) != -1) {
        if (got == 0 || line[0] == '\n' || line[0] == '#') continue;
        char *tok[8];
        int nt = 0;
        char *save = 0;
        for (char *t = strtok_r(line, " \t\r\n", &save); t && nt < 8; t = strtok_r(0, " \t\r\n", &save))
            tok[nt++] = t;
        if (nt == 0) continue;
        const char *op = tok[0];
        char *x1 = nt > 1 ? tok[1] : (char *)"-";
        char *x2 = nt > 2 ? tok[2] : (char *)"-";
        char *x3 = nt > 3 ? tok[3] : (char *)"-";
        char *x4 = nt > 4 ? tok[4] : (char *)"-";

        if (!strcmp(op, "utf8_seq")) {
            size_t n = unhex(x1, a1);
            (void)n;
            printf("utf8_seq|%s|%s|%u\n", x1, x2, utf8_seq(a1, (u32)atol_arg(x2)));
        } else if (!strcmp(op, "utf8_prefix")) {
            unhex(x1, a1);
            printf("utf8_prefix|%s|%s|%u\n", x1, x2, utf8_prefix(a1, (u32)atol_arg(x2)));
        } else if (!strcmp(op, "json_find") || !strcmp(op, "json_find_top")) {
            unhex(x1, a1);
            unhex(x2, a2);
            const char *r = strstr(op, "_top") ? json_find_top(a1, a2) : json_find(a1, a2);
            printf("%s|%s|%s|%ld\n", op, x1, x2, r ? (long)(r - a1) : -1L);
        } else if (!strcmp(op, "json_string") || !strcmp(op, "json_string_top")) {
            char out[64];
            unhex(x1, a1);
            unhex(x2, a2);
            memset(out, 0, sizeof out);
            int ok = strstr(op, "_top") ? json_string_top(a1, a2, out, sizeof out)
                                        : json_string(a1, a2, out, sizeof out);
            printf("%s|%s|%s|%d|", op, x1, x2, ok);
            phex(out);
            fputc('\n', stdout);
        } else if (!strcmp(op, "json_int") || !strcmp(op, "json_int_top")) {
            long long v = 0;
            unhex(x1, a1);
            unhex(x2, a2);
            int ok = strstr(op, "_top") ? json_int_top(a1, a2, &v) : json_int(a1, a2, &v);
            printf("%s|%s|%s|", op, x1, x2);
            if (ok) printf("%lld\n", v); else fputs("-\n", stdout);
        } else if (!strcmp(op, "json_float1") || !strcmp(op, "json_float3")
                   || !strcmp(op, "json_float1_top") || !strcmp(op, "json_float3_top")) {
            unhex(x1, a1);
            unhex(x2, a2);
            int top = strstr(op, "_top") != 0;
            int three = strstr(op, "float3") != 0;
            if (three) {
                float v[3] = { 0, 0, 0 };
                int ok = top ? json_float3_top(a1, a2, v) : json_float3(a1, a2, v);
                printf("%s|%s|%s|", op, x1, x2);
                if (ok) printf("%08x,%08x,%08x\n", fbits(v[0]), fbits(v[1]), fbits(v[2]));
                else fputs("-\n", stdout);
            } else {
                float v = 0;
                int ok = top ? json_float1_top(a1, a2, &v) : json_float1(a1, a2, &v);
                printf("%s|%s|%s|", op, x1, x2);
                if (ok) printf("%08x\n", fbits(v)); else fputs("-\n", stdout);
            }
        } else if (!strcmp(op, "json_int_at")) {
            long long v = 0;
            unhex(x1, a1);
            printf("json_int_at|%s|%s|", x1, x2);
            if (json_int_at(a1 + atol_arg(x2), &v)) printf("%lld\n", v); else fputs("-\n", stdout);
        } else if (!strcmp(op, "json_float1_at")) {
            float v = 0;
            unhex(x1, a1);
            printf("json_float1_at|%s|%s|", x1, x2);
            if (json_float1_at(a1 + atol_arg(x2), &v)) printf("%08x\n", fbits(v));
            else fputs("-\n", stdout);
        } else if (!strcmp(op, "json_float3_at")) {
            float v[3] = { 0, 0, 0 };
            unhex(x1, a1);
            printf("json_float3_at|%s|%s|", x1, x2);
            if (json_float3_at(a1 + atol_arg(x2), v))
                printf("%08x,%08x,%08x\n", fbits(v[0]), fbits(v[1]), fbits(v[2]));
            else fputs("-\n", stdout);
        } else if (!strcmp(op, "json_num")) {
            unhex(x1, a1);
            const char *p = a1 + atol_arg(x2);
            double d = 0;
            printf("json_num|%s|%s|", x1, x2);
            if (json_num(&p, &d)) printf("%ld|", (long)(p - a1)), pf64(dbits(d)), fputc('\n', stdout);
            else fputs("-|-\n", stdout);
        } else if (!strcmp(op, "json_bal_end")) {
            unhex(x1, a1);
            const char *r = json_bal_end(a1 + atol_arg(x2));
            printf("json_bal_end|%s|%s|%ld\n", x1, x2, r ? (long)(r - a1) : -1L);
        } else if (!strcmp(op, "json_value_end")) {
            unhex(x1, a1);
            const char *r = json_value_end(a1 + atol_arg(x2));
            printf("json_value_end|%s|%s|%ld\n", x1, x2, (long)(r - a1));
        } else if (!strcmp(op, "json_string_at")) {
            char out[64];
            unhex(x1, a1);
            memset(out, 0, sizeof out);
            int ok = json_string_at(a1 + atol_arg(x2), out, sizeof out);
            printf("json_string_at|%s|%s|%d|", x1, x2, ok);
            phex(out);
            fputc('\n', stdout);
        } else if (!strcmp(op, "json_estr")) {
            char ub[4] = { 0, 0, 0, 0 };
            unhex(x1, a1);
            u32 outn = 0;
            u32 used = json_estr(a1 + atol_arg(x2), ub, &outn);
            printf("json_estr|%s|%s|%u|%u|", x1, x2, used, outn);
            if (!outn) fputs("-", stdout);
            else for (u32 i = 0; i < outn; i++) printf("%02x", (u8)ub[i]);
            fputc('\n', stdout);
        } else if (!strcmp(op, "kv_next") || !strcmp(op, "kv_next_small")) {
            char name[64], sval[1200], nval[64];
            memset(name, 0, sizeof name);
            memset(sval, 0, sizeof sval);
            memset(nval, 0, sizeof nval);
            if (!strcmp(op, "kv_next_small")) {
                char ns[8], vs[16], nvs[4];
                memset(ns, 0, sizeof ns);
                memset(vs, 0, sizeof vs);
                memset(nvs, 0, sizeof nvs);
                int kind = 0, over = 0; long long ival = 0; double fval = 0;
                unhex(x1, a1);
                const char *p = kv_next(a1 + atol_arg(x2), ns, sizeof ns, &kind, vs, sizeof vs,
                                        &ival, &fval, nvs, sizeof nvs, &over);
                printf("%s|%s|%s|", op, x1, x2);
                if (!p) fputs("-|-|-|-|-|-|-|-\n", stdout);
                else { printf("%ld|%d|%lld|", (long)(p - a1), kind, ival); pf64(dbits(fval));
                       printf("|%d|", over); phex(ns); fputc('|', stdout); phex(vs);
                       fputc('|', stdout); phex(nvs); fputc('\n', stdout); }
            } else {
                int kind = 0, over = 0; long long ival = 0; double fval = 0;
                unhex(x1, a1);
                const char *p = kv_next(a1 + atol_arg(x2), name, sizeof name, &kind, sval,
                                        sizeof sval, &ival, &fval, nval, sizeof nval, &over);
                printf("%s|%s|%s|", op, x1, x2);
                if (!p) fputs("-|-|-|-|-|-|-|-\n", stdout);
                else { printf("%ld|%d|%lld|", (long)(p - a1), kind, ival); pf64(dbits(fval));
                       printf("|%d|", over); phex(name); fputc('|', stdout); phex(sval);
                       fputc('|', stdout); phex(nval); fputc('\n', stdout); }
            }
        } else if (!strcmp(op, "batch_op_next")) {
            char opbuf[64], pbuf[64];
            unhex(x1, a1);
            memset(opbuf, 0, sizeof opbuf);
            memset(pbuf, 0, sizeof pbuf);
            const char *p = a1 + atol_arg(x2);
            int ok = batch_op_next(&p, opbuf, sizeof opbuf, pbuf, sizeof pbuf);
            printf("batch_op_next|%s|%s|%d|%ld|", x1, x2, ok, (long)(p - a1));
            phex(opbuf); fputc('|', stdout); phex(pbuf); fputc('\n', stdout);
        } else if (!strcmp(op, "batch_has_create")) {
            unhex(x1, a1);
            printf("batch_has_create|%s|%d\n", x1, batch_has_create(a1));
        } else if (!strcmp(op, "batch_node_recorded")) {
            int ids[64];
            int n = parse_ids(x1, ids, 64);
            printf("batch_node_recorded|%s|%s|%d\n", x1, x2, batch_node_recorded(ids, n, (int)atol_arg(x2)));
        } else if (!strcmp(op, "batch_record_decision")) {
            int ids[64];
            (void)parse_ids(x1, ids, 64);
            int count = (int)atol_arg(x2);
            int cap = (int)atol_arg(x3);
            int id = (int)atol_arg(x4);
            int r = batch_record_decision(ids, &count, cap, id);
            printf("batch_record_decision|%s|%s|%s|%s|%d|%d|", x1, x2, x3, x4, r, count);
            pids(ids, count);
            fputc('\n', stdout);
        } else if (!strcmp(op, "sb_addjson")) {
            char buf[512];
            unhex(x1, a1);
            SB b;
            sb_init(&b, buf, sizeof buf);
            sb_addjson(&b, a1);
            printf("sb_addjson|%s|%d|", x1, b.ovf);
            phex(buf);
            fputc('\n', stdout);
        } else if (!strcmp(op, "sb_addf")) {
            char buf[512];
            SB b;
            sb_init(&b, buf, sizeof buf);
            sb_addf(&b, vf32(x1));
            printf("sb_addf|%s|%d|", x1, b.ovf);
            phex(buf);
            fputc('\n', stdout);
        } else if (!strcmp(op, "sb_addi")) {
            char buf[512];
            SB b;
            sb_init(&b, buf, sizeof buf);
            sb_addi(&b, atoll_arg(x1));
            printf("sb_addi|%s|%d|", x1, b.ovf);
            phex(buf);
            fputc('\n', stdout);
        } else if (!strcmp(op, "sb_addnum")) {
            char buf[512];
            SB b;
            sb_init(&b, buf, sizeof buf);
            sb_addnum(&b, atoull_arg(x1));
            printf("sb_addnum|%s|%d|", x1, b.ovf);
            phex(buf);
            fputc('\n', stdout);
        } else if (!strcmp(op, "f_finite")) {
            printf("f_finite|%s|%d\n", x1, f_finite(vf32(x1)));
        } else if (!strcmp(op, "xform_origin_ok") || !strcmp(op, "xform_scales_ok")) {
            float v[3];
            for (int i = 0; i < 3; i++) v[i] = vf32(i == 0 ? x1 : i == 1 ? x2 : x3);
            int ok = strstr(op, "origin") ? xform_origin_ok(v) : xform_scales_ok(v);
            printf("%s|%s|%s|%s|%d\n", op, x1, x2, x3, ok);
        } else if (!strcmp(op, "xform_any")) {
            printf("xform_any|%s|%s|%s|%d\n", x1, x2, x3,
                   xform_any((int)atol_arg(x1), (int)atol_arg(x2), (int)atol_arg(x3)));
        } else if (!strcmp(op, "xform_origin_reason")) {
            char dst[96];
            memset(dst, 0, sizeof dst);
            xform_origin_reason(dst, sizeof dst);
            printf("xform_origin_reason|");
            phex(dst);
            fputc('\n', stdout);
        } else if (!strcmp(op, "conn_text_ok")) {
            unhex(x1, a1);
            printf("conn_text_ok|%s|%s|%d\n", x1, x2, conn_text_ok(a1, (int)atol_arg(x2)));
        } else if (!strcmp(op, "conn_array_parse")) {
            ConnSpec specs[CONN_MAX];
            int count = 0;
            unhex(x1, a1);
            int ok = conn_array_parse(a1, specs, CONN_MAX, &count);
            printf("conn_array_parse|%s|", x1);
            if (!ok) { fputs("-\n", stdout); continue; }
            printf("%d", count);
            for (int i = 0; i < count; i++) {
                printf("|");
                phex(specs[i].output); fputc(',', stdout);
                phex(specs[i].target); fputc(',', stdout);
                phex(specs[i].input); fputc(',', stdout);
                phex(specs[i].param); fputc(',', stdout);
                pf32(fbits(specs[i].delay));
                printf(",%d", specs[i].timesToFire);
            }
            fputc('\n', stdout);
        } else if (!strcmp(op, "rm_mode_ok")) {
            printf("rm_mode_ok|%s|%s|%d\n", x1, x2,
                   rm_mode_ok((int)atol_arg(x1), (int)atol_arg(x2)));
        } else if (!strcmp(op, "rm_indices_parse")) {
            int v[RM_INDEX_MAX];
            int count = 0;
            unhex(x1, a1);
            int ok = rm_indices_parse(a1, v, RM_INDEX_MAX, &count);
            printf("rm_indices_parse|%s|", x1);
            if (!ok) { fputs("-\n", stdout); continue; }
            printf("%d:", count);
            for (int i = 0; i < count; i++) printf("%s%d", i ? "," : "", v[i]);
            fputc('\n', stdout);
        } else if (!strcmp(op, "rm_sort_desc")) {
            int v[64];
            int n = parse_ids(x1, v, 64);
            rm_sort_desc(v, n);
            printf("rm_sort_desc|%s|", x1);
            pids(v, n);
            fputc('\n', stdout);
        } else {
            fprintf(stderr, "gen: unknown op %s\n", op);
            return 2;
        }
    }
    free(line);
    fclose(f);
    return 0;
}
