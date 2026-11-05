/* test_entity_json.c - HOST unit test for the entity JSON scanners in bridge/mcp_bridge.c (FIX-B).
 *
 * The scanners the m2.15 entity ops depend on live in the `entity JSON region` of mcp_bridge.c,
 * which cannot compile off Windows (__declspec imports, the pinned/generated headers). bridge/
 * build.sh lifts that region out of the source verbatim into a generated header and compiles this
 * test against it, so the cases below exercise the shipped text, not a copy of it - and a
 * regression inside the region fails the build. Same wiring as test_json_text.c / test_kv3_check.c
 * (host clang, no MSVC target flag, run before the DLL link).
 *
 * What it pins (items in target/review-opus.md / target/review-1.md / target/review-opus-3.md):
 *  - json_find_top only matches TOP-LEVEL members: `keyValues.origin` (a string) must not win over
 *    the real `origin` array (opus #1), for entity.create, for setKeyValues' `values`, for
 *    expectDoc and for a top-level `nodeID` (opus #5 / ds #1).
 *  - batch_op_next is string/escape aware: a `}` or `{` inside a keyvalue does not end the op and
 *    the op's own params object comes out complete (opus #2).
 *  - kv_next decodes \uXXXX (incl. surrogate pairs), \b, \f, \/ (opus #16), reads exponent number
 *    lexemes while keeping the raw token text (opus #8), and reports a value that does not fit the
 *    caller's buffer instead of cutting it - with every later key still parsed (opus #7, ds #3).
 *  - the integer/float bounds (opus N3, re-review L6): json_int_at refuses a lexeme above
 *    INT32_MAX (2147483647, a 20-digit id) instead of wrapping through signed overflow, json_int
 *    refuses only one that would overflow long long, and json_num / the json_float* helpers refuse
 *    a literal that is, or narrows to, inf. kv_next's *number token* is no longer refused as
 *    malformed: it is consumed and flagged (`over = 2`) with its text kept in `nval`, so the apply
 *    pass can refuse just that key (a String attribute still stores the text) and the keys after it
 *    still parse — `return 0` used to drop the whole rest of the object.
 *  - kv_next's true/false/null are compared whole, so a frame that ends inside a literal is
 *    malformed and the scan stops at its NUL (L6; the case runs on a guard-page buffer whose last
 *    byte is the terminator, so a read past it faults rather than passing silently).
 *  - batch_has_create decides the edit.batch undo label (review section 3): a batch of nothing but
 *    entity.setKeyValues ops gets "MCP: set keyvalues", not "MCP: batch".
 *  - sb_addjson escapes control characters and replaces every invalid or cut UTF-8 sequence with
 *    U+FFFD (opus #10); utf8_prefix (the trim probe_str cuts with) never cuts inside a sequence.
 *
 * Usage: test_entity_json <path to bridge/mcp_bridge.c>
 * The source is read at test time: a missing file, a missing/duplicated region marker or a region
 * without the scanners is a FAILURE (exit 2), never a skip.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>                     /* the guard-page case below (host test only) */
#include <unistd.h>                       /* sysconf(_SC_PAGESIZE) */

typedef unsigned char u8;            /* the bridge's own typedefs; the region needs nothing else */
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;

#include "entity_json_region.h"

static int g_cases, g_failures;

/* one case: `ok` is the verdict, `detail` the printed reason */
static void step(const char *name, int ok, const char *detail) {
    g_cases++;
    if (ok) printf("PASS %-34s %s\n", name, detail);
    else { printf("FAIL %-34s %s\n", name, detail); g_failures++; }
}

/* the bytes of a C string as a printable hex row (failure messages only) */
static const char *hexdump(const char *s) {
    static char buf[256];
    u32 n = 0;
    for (const unsigned char *p = (const unsigned char *)s; *p && n + 4 < sizeof buf; p++) {
        static const char *h = "0123456789abcdef";
        buf[n++] = h[*p >> 4];
        buf[n++] = h[*p & 15];
        buf[n++] = ' ';
    }
    buf[n] = 0;
    return buf;
}

static int is_bytes(const char *got, const char *want, size_t wantLen) {
    return strlen(got) == wantLen && (wantLen == 0 || memcmp(got, want, wantLen) == 0);
}

/* ---- 1. json_find_top: top-level members only (review opus #1 / opus #5 / ds #1) ------------ */

/* what hammer-mcp's edit.batch sends for entity.create: serde_json writes the struct fields in key
   order, so keyValues (and its `origin` string) precedes the top-level `origin` array. */
static const char *k_create =
    "{\"angles\":[0,0,0],\"class\":\"info_target\","
    "\"keyValues\":{\"origin\":\"10 20 30\",\"targetname\":\"mcp_ent\"},"
    "\"origin\":[512,1024,0]}";

static void top_level_cases(void) {
    const char *old = json_find(k_create, "origin");
    const char *top = json_find_top(k_create, "origin");
    float v[3] = { -1, -1, -1 };
    int got = json_float3_top(k_create, "origin", v);
    char cls[64];
    char detail[192];

    snprintf(detail, sizeof detail, "old->%c top->%c float3=%d [%g,%g,%g]", *old, *top, got,
             (double)v[0], (double)v[1], (double)v[2]);
    step("json_find_top_origin_beats_nested", old && *old == '"' && top && *top == '['
         && got && v[0] == 512.0f && v[1] == 1024.0f && v[2] == 0.0f, detail);

    step("json_find_top_class", json_string_top(k_create, "class", cls, sizeof cls)
         && strcmp(cls, "info_target") == 0, cls);

    const char *kv = json_find_top(k_create, "keyValues");
    static const char want_kv[] = "{\"origin\":\"10 20 30\"";
    step("json_find_top_keyvalues_object", kv && *kv == '{'
         && strncmp(kv, want_kv, sizeof want_kv - 1) == 0, kv ? kv : "(none)");

    /* a member that only exists at depth 2 must not be found at the top level */
    step("json_find_top_nested_only_is_absent",
         json_find_top(k_create, "targetname") == 0 && json_find(k_create, "targetname") != 0,
         "targetname lives in keyValues only");

    /* a key lexeme inside a string value is not a member: the real one after it is found */
    const char *inside = "{\"note\":\"has \\\"origin\\\": 5 inside\",\"origin\":[7,8,9]}";
    float w[3] = { 0, 0, 0 };
    got = json_float3_top(inside, "origin", w);
    step("json_find_top_skips_string_contents", got && w[0] == 7.0f && w[2] == 9.0f,
         "a quoted key inside a string is not a member");

    /* a nested `nodeID` (a keyvalue of that name, or a nested object) must not be read as the op's
       nodeID: that is the (int) cast aliasing of review opus #5 / ds #1 */
    const char *setkv = "{\"expectDoc\":{\"id\":\"d1\",\"epoch\":4},"
                        "\"values\":{\"nodeID\":7},\"nodeID\":2000000000}";
    long long id = -1, oldid = -1;
    int strict = json_int_top(setkv, "nodeID", &id);
    int loose = json_int(setkv, "nodeID", &oldid);
    snprintf(detail, sizeof detail, "top=%s%lld old=%s%lld", strict ? "" : "none ", id,
             loose ? "" : "none ", oldid);
    step("json_find_top_nodeid_beats_nested", strict && id == 2000000000LL && loose
         && oldid == 7, detail);

    /* the top-level id is the out-of-range lexeme the caller must refuse: the depth-blind finder
       still hands back the nested 7, while json_int_top sees the top-level 4294967326 and refuses
       it (review opus N3: through the (int) cast that id used to alias node 30) */
    const char *alias = "{\"values\":{\"nodeID\":7},\"nodeID\":4294967326}";
    long long top_id = -1, nested_id = -1;
    step("json_find_top_nodeid_out_of_range_refused",
         json_find_top(alias, "nodeID") != 0 && json_int_top(alias, "nodeID", &top_id) == 0
         && json_int(alias, "nodeID", &nested_id) == 1 && nested_id == 7,
         "the top-level 4294967326 is refused, not wrapped onto node 30");

    /* expectDoc too: an `epoch` inside it is not a top-level epoch */
    const char *two = "{\"expectDoc\":{\"id\":\"d1\",\"epoch\":4},\"values\":{}}";
    long long ep = 0;
    step("json_find_top_expectdoc", json_int_top(two, "epoch", &ep) == 0
         && json_int(two, "epoch", &ep) == 1 && ep == 4,
         "epoch is inside expectDoc, not top level");

    step("json_int_at_rejects_fraction_and_exponent",
         json_int_at("1.5", &id) == 0 && json_int_at("1e5", &id) == 0
         && json_int_at("abc", &id) == 0 && json_int_at("true", &id) == 0,
         "a nodeID/limit must be a plain decimal integer");
}

/* ---- 1b. the integer and float bounds (review opus N3) -------------------------------------- */

static void int_bound_cases(void) {
    char detail[192];
    long long v = -1;
    /* 2147483647 is the biggest id/limit every caller accepts (NODEID_MAX / ENTITY_FIND_MAX); one
       more is refused, and a 20-digit lexeme (2^64 + 30, which used to wrap to 30 through the
       signed overflow) is refused instead of aliasing another node. The bound is checked before
       every multiply, so no accumulation can wrap. */
    int max_ok = json_int_at("2147483647", &v) == 1 && v == 2147483647LL;
    long long over_v = -1, twenty_v = -1;
    int over = json_int_at("2147483648", &over_v);
    int twenty = json_int_at("18446744073709551646", &twenty_v);
    int huge = json_int_at("99999999999999999999999999", &v);
    int zero = json_int_at("0", &v) == 1 && v == 0;
    int neg = json_int_at("-1", &v) == 1 && v == -1;      /* the sign parses: -32602, not a parse error */
    int negmin = json_int_at("-2147483648", &v);          /* INT32_MIN is outside every caller's range */
    snprintf(detail, sizeof detail,
             "max=%d 2147483648=%d(%lld) 20digit=%d(%lld wrapped) huge=%d zero=%d neg=%d min=%d",
             max_ok, over, over_v, twenty, twenty_v, huge, zero, neg, negmin);
    step("json_int_at_bounded_to_int32", max_ok && over == 0 && twenty == 0 && huge == 0 && zero
         && neg && negmin == 0, detail);

    /* json_int (the lenient finder behind every other integer param: epoch, width/height/format,
       frames, node.list's limit, protocol, deadlineMs, and the game/pet-render params) refuses a
       lexeme that would overflow long long; a value any caller sends is unaffected */
    long long e = -1;
    int epoch = json_int("{\"epoch\":4}", "epoch", &e) == 1 && e == 4;
    int width = json_int("{\"width\":1920}", "width", &e) == 1 && e == 1920;
    int deadline = json_int("{\"deadlineMs\":0}", "deadlineMs", &e) == 1 && e == 0;
    int e_over = json_int("{\"epoch\":18446744073709551646}", "epoch", &e);
    int e_huge = json_int("{\"epoch\":99999999999999999999999999}", "epoch", &e);
    snprintf(detail, sizeof detail, "epoch=%d width=%d deadlineMs=%d over=%d huge=%d", epoch, width,
             deadline, e_over, e_huge);
    step("json_int_no_signed_overflow", epoch && width && deadline && e_over == 0 && e_huge == 0,
         detail);

    /* floats: a literal that accumulates to inf is refused rather than stored as an origin. 1e400
       is inf in the double accumulator; 400 digits with no exponent is inf by accumulation. */
    static char digits[512];
    double d_big = -1, d_exp = -1, d_20 = -1;
    const char *exp400 = "1e400";
    const char *exp20 = "1e20";
    digits[0] = '1';
    for (int i = 1; i <= 400; i++) digits[i] = '0';
    digits[401] = 0;
    const char *big = digits;
    int exp_inf = json_num(&big, &d_big) == 0;
    int lit_inf = json_num(&exp400, &d_exp) == 0;
    int e_big = json_num(&exp20, &d_20) == 1 && d_20 > 1e19 && d_20 < 1e21;
    snprintf(detail, sizeof detail, "1e400=%d(%g) 400-digit=%d(%g) 1e20=%d(%g)", exp_inf, d_exp,
             lit_inf, d_big, e_big, d_20);
    step("json_num_rejects_non_finite", exp_inf && lit_inf && e_big, detail);

    /* a finite double that narrows to +-inf as float is refused too (the origin is a float), and
       the caller's variable is left untouched - `fov`/`znear`/`zfar` ignore the return value */
    float f3[3] = { 7, 8, 9 }, narrow_v;
    float f1 = 42.0f, narrow1_v;
    int narrow = json_float3_at("[1e39,0,0]", f3) == 0;
    narrow_v = f3[0];
    int narrow1 = json_float1_at("1e39", &f1) == 0;
    narrow1_v = f1;
    int untouched = narrow_v == 7.0f && narrow1_v == 42.0f;
    int normal = json_float3_at("[1,2.5,3]", f3) == 1 && f3[0] == 1.0f && f3[2] == 3.0f;
    int radius = json_float1_at("1e20", &f1) == 1 && f1 > 1e19f;
    snprintf(detail, sizeof detail, "1e39_3=%d(origin=%g) 1e39_1=%d(%g) untouched=%d normal=%d "
             "1e20=%d(%g)", narrow, (double)narrow_v, narrow1, (double)narrow1_v, untouched, normal,
             radius, (double)f1);
    step("json_float_rejects_narrowed_inf", narrow && narrow1 && untouched && normal && radius,
         detail);
}

/* ---- 2. the batch splitter is string/escape aware (review opus #2) -------------------------- */

static const char *k_ops =
    "[{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\","
    "\"keyValues\":{\"message\":\"a}b{c\",\"targetname\":\"x\"},\"origin\":[1,2,3]}},"
    "{\"method\":\"entity.setKeyValues\",\"params\":{\"handle\":\"$0\",\"values\":{\"k\":\"v\"}}}]";

static const char *k_want_op =
    "{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\","
    "\"keyValues\":{\"message\":\"a}b{c\",\"targetname\":\"x\"},\"origin\":[1,2,3]}}";

static const char *k_want_params =
    "{\"class\":\"info_target\",\"keyValues\":{\"message\":\"a}b{c\",\"targetname\":\"x\"},"
    "\"origin\":[1,2,3]}";

static void batch_cases(void) {
    char opbuf[8192], pbuf[8192];
    const char *p = k_ops + 1;
    int n = 0, ok = 1;
    static char first_op[4096], first_params[4096];
    char detail[128];
    first_op[0] = first_params[0] = 0;
    while (batch_op_next(&p, opbuf, sizeof opbuf, pbuf, sizeof pbuf)) {
        if (n == 0) {
            snprintf(first_op, sizeof first_op, "%s", opbuf);
            snprintf(first_params, sizeof first_params, "%s", pbuf);
        }
        n++;
        if (n > 8) { ok = 0; break; }              /* a splitter that does not advance would hang */
    }
    snprintf(detail, sizeof detail, "ops=%d op[0]=%zu bytes params[0]=%zu bytes", n,
             strlen(first_op), strlen(first_params));
    step("batch_op_next_two_ops", ok && n == 2 && p >= k_ops && *p == ']', detail);
    step("batch_op_next_braces_in_string", strcmp(first_op, k_want_op) == 0,
         strcmp(first_op, k_want_op) == 0 ? "op 0 complete" : hexdump(first_op));
    step("batch_op_next_params_complete", strcmp(first_params, k_want_params) == 0,
         strcmp(first_params, k_want_params) == 0 ? "params 0 complete" : hexdump(first_params));

    /* a truncated op object stops the walk instead of emitting a cut op */
    {
        const char *bad = "{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\"";
        const char *q = bad;
        char b1[256], b2[256];
        step("batch_op_next_unterminated_stops",
             batch_op_next(&q, b1, sizeof b1, b2, sizeof b2) == 0,
             "no closing brace -> 0, never a cut op");
    }
}

/* ---- 2b. the edit.batch undo-group label (review section 3) --------------------------------- */

/* what the edit.batch arm opens: "MCP: batch" only when the batch created something, else
   "MCP: set keyvalues" (a pop of that one reverts nothing, so hammer-mcp must not treat it as a
   restore and lower its unsaved-edit counter) */
static const char *batch_label(const char *ops) {
    return batch_has_create(ops) ? "MCP: batch" : "MCP: set keyvalues";
}

static void label_cases(void) {
    static const char *set_only =
        "[{\"method\":\"entity.setKeyValues\",\"params\":{\"handle\":\"$0\",\"values\":{\"k\":\"v\"}}},"
        "{\"method\":\"entity.setKeyValues\",\"params\":{\"nodeID\":30,\"values\":{\"k2\":\"v2\"}}}]";
    static const char *create_only =
        "[{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\"}}]";
    static const char *create_then_set =
        "[{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\"}},"
        "{\"method\":\"entity.setKeyValues\",\"params\":{\"handle\":\"$0\",\"values\":{\"k\":\"v\"}}}]";
    static const char *set_then_create =
        "[{\"method\":\"entity.setKeyValues\",\"params\":{\"handle\":\"$0\",\"values\":{\"k\":\"v\"}}},"
        "{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\"}}]";
    char detail[256];
    snprintf(detail, sizeof detail, "set-only=%s create-only=%s create-first=%s create-last=%s empty=%s",
             batch_label(set_only), batch_label(create_only), batch_label(create_then_set),
             batch_label(set_then_create), batch_label("[]"));
    step("batch_label_set_only_is_keyvalues",
         strcmp(batch_label(set_only), "MCP: set keyvalues") == 0
         && strcmp(batch_label("[]"), "MCP: set keyvalues") == 0, detail);
    step("batch_label_with_create_is_batch",
         strcmp(batch_label(create_only), "MCP: batch") == 0
         && strcmp(batch_label(create_then_set), "MCP: batch") == 0
         && strcmp(batch_label(set_then_create), "MCP: batch") == 0, detail);
    step("batch_label_reads_the_method_member",
         batch_has_create("[{\"method\":\"entity.setKeyValues\",\"params\":{\"values\":"
                          "{\"note\":\"entity.create\",\"class\":\"entity.create\"}}}]") == 0
         && batch_has_create("[{\"method\":\"entity.createX\",\"params\":{}}]") == 0,
         "a create-named value or a longer method name is not a create op");
}

/* review W2 #7: the edit.batch arm tracks the nodes the group already recorded (`recorded[]`) so a
   second RecordNodeTree cannot happen - the old rule was the 16-entry handle table, which a 17th
   create or a second setKeyValues on one node walked straight past. The set is per batch (plain
   locals in the batch arm) and the helper is what both the create and the setKeyValues paths ask. */
static void recorded_cases(void) {
    int ids[32];
    int n = 0;
    step("batch_recorded_empty", !batch_node_recorded(ids, n, 30),
         "an empty set holds nothing");
    ids[n++] = 30;
    ids[n++] = 45;
    step("batch_recorded_hit", batch_node_recorded(ids, n, 30)
         && batch_node_recorded(ids, n, 45), "both ids are recorded");
    step("batch_recorded_miss", !batch_node_recorded(ids, n, 31),
         "an id the group never saw is not recorded");
    /* nodeID 0 is a real id, not a sentinel (idabins' `wantId` uses -1 for "list everything") */
    int zero[4] = { 0 };
    step("batch_recorded_zero_id", batch_node_recorded(zero, 1, 0),
         "nodeID 0 is matched like any other");
    step("batch_recorded_negative_miss", !batch_node_recorded(ids, n, -1),
         "-1 is the walk's own 'no id', never a recorded node");

    /* the decision the edit.batch arm calls: the first call on a node records it (1) AND remembers
       it; the second call on the same node answers 0 (no second RecordNodeTree in the group) */
    int set[32];
    int m = 0;
    step("batch_record_decision_first_records",
         batch_record_decision(set, &m, 32, 30) == 1 && m == 1 && set[0] == 30,
         "the first setKeyValues on a node records it");
    step("batch_record_decision_second_skips",
         batch_record_decision(set, &m, 32, 30) == 0 && m == 1,
         "a second op on the SAME node in the same batch does not record it again (W2 #7)");
    step("batch_record_decision_other_node",
         batch_record_decision(set, &m, 32, 45) == 1 && m == 2,
         "another node is recorded on its own first write");
    step("batch_record_decision_negative_never",
         batch_record_decision(set, &m, 32, -1) == 0 && m == 2,
         "-1 (an unresolved node) is never recorded and never appended");

    /* the cap: past `cap` the decision answers 1 (record - the pre-W2 behaviour, safe) but cannot
       remember the node, so a later op on it records again rather than losing the undo */
    int full[2];
    int f = 0;
    batch_record_decision(full, &f, 2, 1);
    batch_record_decision(full, &f, 2, 2);
    int third = batch_record_decision(full, &f, 2, 3);
    step("batch_record_decision_cap", f == 2 && third == 1
         && batch_record_decision(full, &f, 2, 1) == 0,
         "the table stops growing at cap; a recorded node is still recognized");
}

/* ---- 3. kv_next: escapes, exponents, overlong values (opus #7/#8/#16, ds #3) ---------------- */

typedef struct { char name[64]; int kind; char sval[1200]; char nval[64]; int over; } KvRow;

/* run kv_next over the members of the object at `obj`: the caller skips the opening brace, exactly
   like apply_keyvalues does (`p = vp + 1`). Collects what each key held. */
static int kv_all(const char *obj, KvRow *rows, int max) {
    const char *p = obj + 1;
    int n = 0;
    while (p) {
        int over = 0, kind = 0;
        long long ival = 0;
        double fval = 0;
        if (n >= max) return -1;
        p = kv_next(p, rows[n].name, sizeof rows[n].name, &kind, rows[n].sval,
                    sizeof rows[n].sval, &ival, &fval, rows[n].nval, sizeof rows[n].nval, &over);
        if (!p) break;
        rows[n].kind = kind;
        rows[n].over = over;
        n++;
    }
    return n;
}

static void kv_cases(void) {
    static KvRow rows[16];
    char detail[256];
    int n;

    /* exponents: serde_json writes |v| >= 1e16 and |v| < 1e-5 that way (opus #8) */
    n = kv_all("{\"a\":1e20,\"b\":-1.5E-7,\"c\":4,\"d\":-2.25}", rows, 16);
    snprintf(detail, sizeof detail, "keys=%d a=%s b=%s c=%s", n, rows[0].nval, rows[1].nval,
             rows[2].nval);
    step("kv_next_exponent_tokens", n == 4 && rows[0].kind == 3
         && strcmp(rows[0].nval, "1e20") == 0 && rows[1].kind == 3
         && strcmp(rows[1].nval, "-1.5E-7") == 0 && rows[2].kind == 2
         && strcmp(rows[2].nval, "4") == 0 && rows[3].kind == 3, detail);
    {
        const char *q = "1e20";
        const char *q2 = "-1.5E-7";
        const char *q3 = "5.5e-17";
        const char *q4 = "1e";                    /* a bare exponent: the token stops at the `e` */
        double d = 0, d2 = 0, d3 = 0, d4 = 0;
        int ok = json_num(&q, &d) && d > 1e19 && d < 1e21 && *q == 0
              && json_num(&q2, &d2) && d2 < 0 && d2 > -1e-6 && *q2 == 0
              && json_num(&q3, &d3) && d3 > 0 && d3 < 1e-16 && *q3 == 0
              && json_num(&q4, &d4) && d4 == 1.0 && *q4 == 'e';
        char d5[96];
        snprintf(d5, sizeof d5, "1e20=%g -1.5E-7=%g 5.5e-17=%g 1e=%g", d, d2, d3, d4);
        step("json_num_exponents", ok, d5);
    }

    /* \uXXXX (with a surrogate pair), \b, \f and \/ (opus #16) */
    n = kv_all("{\"s\":\"\\u0041\\u00e9\\ud83d\\ude00\\b\\f\\/\"}", rows, 16);
    {
        static const char want_esc[] = { 'A', (char)0xC3, (char)0xA9, (char)0xF0, (char)0x9F,
                                         (char)0x98, (char)0x80, '\b', '\f', '/' };
        int ok = n == 1 && rows[0].kind == 5
                 && is_bytes(rows[0].sval, want_esc, sizeof want_esc);
        snprintf(detail, sizeof detail, "keys=%d bytes=%s", n, hexdump(rows[0].sval));
        step("kv_next_unicode_escapes", ok, detail);
    }
    n = kv_all("{\"classname\\u0000x\":\"a\\u0000b\"}", rows, 16);   /* \u0000 must not end the C string */
    step("kv_next_nul_escape_replaced", n == 1 && strcmp(rows[0].name, "classname") != 0
         && is_bytes(rows[0].sval, "a\xEF\xBF\xBD" "b", 5), hexdump(rows[0].sval));
    n = kv_all("{\"s\":\"\\ud83d\"}", rows, 16);                    /* a lone surrogate */
    step("kv_next_lone_surrogate", n == 1 && is_bytes(rows[0].sval, "\xEF\xBF\xBD", 3),
         hexdump(rows[0].sval));
    n = kv_all("{\"a\\u0062\":1,\"x\":\"\\x\"}", rows, 16);         /* escaped name, unknown escape */
    step("kv_next_name_and_unknown_escape", n == 2 && strcmp(rows[0].name, "ab") == 0
         && strcmp(rows[1].sval, "x") == 0, rows[0].name);
    n = kv_all("{\"k\":\"a\\\"b\"}", rows, 16);                     /* an escaped quote */
    step("kv_next_escaped_quote", n == 1 && strcmp(rows[0].sval, "a\"b") == 0, rows[0].sval);

    /* an overlong value is reported, not cut, and the keys after it still parse (opus #7, ds #3) */
    {
        static char obj[2 * (KV_VALUE_MAX + 8) + 64];
        static char val[KV_VALUE_MAX + 2];
        char small[16];
        int over = 0, kind = 0, ok;
        long long ival = 0;
        double fval = 0;

        /* exactly KV_VALUE_MAX bytes fit the caller's buffer (Rust caps at 512, the bridge at 1024) */
        memset(val, 'a', KV_VALUE_MAX);
        val[KV_VALUE_MAX] = 0;
        snprintf(obj, sizeof obj, "{\"long\":\"%s\",\"after\":42}", val);
        {
            const char *p = obj + 1;
            p = kv_next(p, rows[0].name, sizeof rows[0].name, &kind, rows[0].sval, KV_VALUE_MAX + 1,
                        &ival, &fval, rows[0].nval, sizeof rows[0].nval, &over);
            ok = p && over == 0 && (u32)strlen(rows[0].sval) == KV_VALUE_MAX;
            p = p ? kv_next(p, rows[1].name, sizeof rows[1].name, &kind, rows[1].sval, 1200,
                            &ival, &fval, rows[1].nval, sizeof rows[1].nval, &over) : 0;
            ok = ok && p && strcmp(rows[1].name, "after") == 0 && kind == 2 && ival == 42;
        }
        step("kv_next_value_fits_cap", ok, "KV_VALUE_MAX bytes decode whole, the next key parses");

        /* one byte more than the buffer: over=1 and the following key still parses */
        memset(val, 'a', KV_VALUE_MAX + 1);
        val[KV_VALUE_MAX + 1] = 0;
        snprintf(obj, sizeof obj, "{\"long\":\"%s\",\"after\":\"ok\"}", val);
        {
            const char *p = obj + 1;
            p = kv_next(p, rows[0].name, sizeof rows[0].name, &kind, small, sizeof small,
                        &ival, &fval, rows[0].nval, sizeof rows[0].nval, &over);
            ok = p && over == 1;
            over = 0;
            p = p ? kv_next(p, rows[1].name, sizeof rows[1].name, &kind, rows[1].sval, 1200,
                            &ival, &fval, rows[1].nval, sizeof rows[1].nval, &over) : 0;
            ok = ok && p && strcmp(rows[1].name, "after") == 0 && strcmp(rows[1].sval, "ok") == 0
                 && over == 0;
        }
        step("kv_next_overlong_reports_and_continues", ok,
             "overlong value -> over=1, later keys are not lost");
    }

    n = kv_all("{\"a\":null,\"b\":true,\"c\":false}", rows, 16);
    step("kv_next_null_and_bools", n == 3 && rows[0].kind == 5 && rows[0].sval[0] == 0
         && rows[1].kind == 4 && rows[2].kind == 4, "null/true/false");

    /* the empty value: `""` decodes to an empty string with over=0, and the key after it parses.
       This is the input `apply_keyvalues` hands to `dm_intern` for an empty keyvalue: the symbol
       comes back with a NULL m_pString (Hammer's empty string). 4ff4432 no longer passes a NULL
       symbol on unconditionally for OTHER intern failures, but the empty string IS the NULL
       symbol, so this write still goes through and answers applied:true (m2.16 fix - a NULL
       symbol used to skip the write with no warning at all). */
    n = kv_all("{\"a\":\"\",\"b\":\"x\"}", rows, 16);
    step("kv_next_empty_string_value", n == 2 && rows[0].kind == 5 && rows[0].sval[0] == 0
         && rows[0].over == 0 && rows[1].kind == 5 && strcmp(rows[1].sval, "x") == 0,
         "an empty value decodes whole and the next key still parses");

    /* a number token the four DME setters cannot hold is *flagged*, never wrapped and never a
       silent end of the object (review opus N3, re-review L6): `over = 2` consumes the token,
       keeps its text in `nval` (a String attribute still takes it - `1e39` as the text "1e39" is
       the m2.15 §3.4 behaviour) and the keys after it still parse. Before this, `return 0` ended
       both passes, so `{"a":1e39,"b":"x"}` dropped `b` as well. */
    n = kv_all("{\"a\":18446744073709551646,\"b\":\"x\"}", rows, 16);   /* 20 digits, past 2^63 */
    int big_int = n == 2 && rows[0].over == 2 && rows[0].kind == 2
                  && strcmp(rows[0].nval, "18446744073709551646") == 0
                  && rows[1].over == 0 && strcmp(rows[1].name, "b") == 0
                  && strcmp(rows[1].sval, "x") == 0;
    n = kv_all("{\"a\":1e400,\"b\":\"x\"}", rows, 16);                  /* double overflow */
    int big_exp = n == 2 && rows[0].over == 2 && rows[0].kind == 3
                  && strcmp(rows[0].nval, "1e400") == 0
                  && rows[1].over == 0 && strcmp(rows[1].sval, "x") == 0;
    n = kv_all("{\"a\":1e39,\"b\":\"x\"}", rows, 16);                   /* finite double, inf float */
    int big_f32 = n == 2 && rows[0].over == 2 && rows[0].kind == 3
                  && strcmp(rows[0].nval, "1e39") == 0
                  && rows[1].over == 0 && strcmp(rows[1].sval, "x") == 0;
    /* a value between INT32_MAX and 2^63 is a *representable* token (over=0): the int32 refusal is
       the apply pass's, on an int attribute (the host test cannot reach apply_keyvalues) */
    n = kv_all("{\"a\":4294967296,\"b\":8000000000000000000,\"c\":-2147483648}", rows, 16);
    int ok_nums = n == 3 && rows[0].over == 0 && rows[0].kind == 2
                  && strcmp(rows[0].nval, "4294967296") == 0
                  && rows[1].over == 0 && rows[1].kind == 2
                  && strcmp(rows[1].nval, "8000000000000000000") == 0
                  && rows[2].over == 0 && rows[2].kind == 2;
    snprintf(detail, sizeof detail, "20digit=%d 1e400=%d 1e39=%d representable=%d (n=%d)", big_int,
             big_exp, big_f32, ok_nums, n);
    step("kv_next_flags_unrepresentable_numbers", big_int && big_exp && big_f32 && ok_nums, detail);
}

/* ---- 3b. kv_next's literals stop at the terminator (review L6) ------------------------------ */

/* a NUL-terminated copy of `s` whose NUL is the LAST byte of a mapped page, with the next page
   unmapped: a scan that reads past the terminator faults here instead of seeing a garbage byte.
   The mappings are left to the process exit (one per case). */
static char *guard_page_copy(const char *s) {
    long pg = sysconf(_SC_PAGESIZE);
    char *base = (char *)mmap(0, (size_t)pg * 2, PROT_READ | PROT_WRITE,
                              MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (base == MAP_FAILED) return 0;
    if (mprotect(base + pg, (size_t)pg, PROT_NONE) != 0) return 0;
    size_t len = strlen(s);
    if (len + 1 > (size_t)pg) return 0;
    char *p = base + pg - (long)(len + 1);
    memcpy(p, s, len + 1);
    return p;
}

static void literal_cases(void) {
    static KvRow rows[4];
    char detail[192];

    /* a complete literal that ends the frame: it parses and the returned pointer IS the
       terminator, not 1-3 bytes past it */
    {
        const char *obj = "{\"k\":true";
        const char *end = obj + strlen(obj);
        int kind = -1, over = 0;
        long long ival = 0;
        double fval = 0;
        const char *p = kv_next(obj + 1, rows[0].name, sizeof rows[0].name, &kind, rows[0].sval,
                                sizeof rows[0].sval, &ival, &fval, rows[0].nval, sizeof rows[0].nval,
                                &over);
        snprintf(detail, sizeof detail, "kind=%d ival=%lld back=%ld", kind, ival,
                 p ? (long)(p - end) : -99L);
        step("kv_next_literal_at_frame_end", p == end && kind == 4 && ival == 1, detail);
    }

    /* a frame that ends INSIDE a literal. The old scan recognised `n` from one byte and stepped
       four, and `tr`/`fa` from two and stepped four/five, so these handed back a pointer past the
       NUL (the next kv_next call then walked into unmapped memory). Each must be malformed
       (kv_next -> 0) and must not read past its terminator. */
    {
        static const char *frames[] = { "{\"k\":t", "{\"k\":tr", "{\"k\":tru", "{\"k\":n",
                                        "{\"k\":nu", "{\"k\":nul", "{\"k\":f", "{\"k\":fa",
                                        "{\"k\":fal", "{\"k\":fals" };
        u32 nf = (u32)(sizeof frames / sizeof frames[0]);
        int maps = 0, parsed = 0;
        for (u32 i = 0; i < nf; i++) {
            char *frame = guard_page_copy(frames[i]);
            int kind = -1, over = 0;
            long long ival = 0;
            double fval = 0;
            if (!frame) { maps++; continue; }
            const char *p = kv_next(frame + 1, rows[0].name, sizeof rows[0].name, &kind,
                                    rows[0].sval, sizeof rows[0].sval, &ival, &fval, rows[0].nval,
                                    sizeof rows[0].nval, &over);
            if (p || kind != -1) parsed++;      /* a value would mean a jump past the NUL */
        }
        snprintf(detail, sizeof detail, "%u frames: %d unmapped, %d not malformed", nf, maps,
                 parsed);
        step("kv_next_truncated_literal_is_malformed", maps == 0 && parsed == 0, detail);
    }

    /* the complete literals still parse, including one that ends the object */
    {
        int kind = -1, over = 0;
        long long ival = 0;
        double fval = 0;
        const char *obj = "{\"k\":null}";
        const char *p = kv_next(obj + 1, rows[0].name, sizeof rows[0].name, &kind,
                                rows[0].sval, sizeof rows[0].sval, &ival, &fval, rows[0].nval,
                                sizeof rows[0].nval, &over);
        step("kv_next_null_complete", p && *p == '}' && kind == 5 && rows[0].sval[0] == 0,
             "a complete null still reads as the empty string");
    }
}

/* ---- 4. sb_addjson: escaping and UTF-8 replacement (review opus #10) ------------------------ */

static void addjson_case(const char *name, const char *in, const char *want, size_t wantLen) {
    static char buf[512];
    SB b;
    char detail[192];
    sb_init(&b, buf, sizeof buf);
    sb_addjson(&b, in);
    snprintf(detail, sizeof detail, "ovf=%d bytes=%s", b.ovf, hexdump(buf));
    step(name, !b.ovf && is_bytes(buf, want, wantLen), detail);
}

/* intentional non-ASCII and invalid UTF-8 bytes; every literal here stays ASCII in this file */
static const char k_plain[]    = { '"', 'h', 'i', '"' };
static const char k_escapes[]  = { '"', '\\', '"', '\\', '\\', '\\', 'n', '\\', 't', '\\', 'r',
                                   '\\', 'u', '0', '0', '0', '1', '"' };
static const char k_utf8_2[]   = { '"', 'c', 'a', 'f', (char)0xC3, (char)0xA9, '"' };
static const char k_utf8_4[]   = { '"', (char)0xF0, (char)0x9F, (char)0x98, (char)0x80, '"' };
static const char k_bad_bytes[]= { '"', 'a', (char)0xEF, (char)0xBF, (char)0xBD,
                                   (char)0xEF, (char)0xBF, (char)0xBD, 'b', '"' };
static const char k_cut_seq[]  = { '"', 'a', (char)0xEF, (char)0xBF, (char)0xBD, '"' };
static const char k_lone_cont[]= { '"', (char)0xEF, (char)0xBF, (char)0xBD, '"' };
static const char k_overlong[] = { '"', (char)0xEF, (char)0xBF, (char)0xBD, (char)0xEF,
                                   (char)0xBF, (char)0xBD, '"' };
static const char k_surrogate[]= { '"', (char)0xEF, (char)0xBF, (char)0xBD, '"' };

static void addjson_cases(void) {
    addjson_case("sb_addjson_plain", "hi", k_plain, sizeof k_plain);
    /* " \ LF TAB CR 0x01 -> \" \\ \n \t \r \u0001 */
    addjson_case("sb_addjson_escapes", "\"\\\n\t\r\x01", k_escapes, sizeof k_escapes);
    addjson_case("sb_addjson_valid_utf8", "caf\xC3\xA9", k_utf8_2, sizeof k_utf8_2);
    addjson_case("sb_addjson_four_byte_utf8", "\xF0\x9F\x98\x80", k_utf8_4, sizeof k_utf8_4);
    addjson_case("sb_addjson_invalid_bytes", "a\xFF\xFE" "b", k_bad_bytes, sizeof k_bad_bytes);
    addjson_case("sb_addjson_cut_sequence", "a\xE2\x82", k_cut_seq, sizeof k_cut_seq);
    addjson_case("sb_addjson_lone_continuation", "\x80", k_lone_cont, sizeof k_lone_cont);
    addjson_case("sb_addjson_overlong_sequence", "\xC0\xAF", k_overlong, sizeof k_overlong);
    addjson_case("sb_addjson_raw_surrogate", "\xED\xA0\x80", k_surrogate, sizeof k_surrogate);

    /* the overflow flag still stops a cut JSON line */
    {
        char small[8];
        SB b;
        sb_init(&b, small, sizeof small);
        sb_addjson(&b, "0123456789");
        step("sb_addjson_overflow_flag", b.ovf == 1 && small[0] == '"', "a long string sets ovf");
    }
}

/* ---- 5. utf8_prefix: a reader never cuts inside a sequence (review opus #10) ---------------- */

static void utf8_cases(void) {
    char detail[128];
    u32 a = utf8_prefix("abc", 3);
    u32 b = utf8_prefix("caf\xC3\xA9", 5);                 /* a complete é at the end */
    u32 c = utf8_prefix("caf\xC3\xA9", 4);                 /* cut inside é */
    u32 d = utf8_prefix("caf\xC3", 4);                     /* only the lead byte */
    u32 e = utf8_prefix("\xF0\x9F\x98", 3);                /* a 4-byte emoji, 3 of 4 bytes */
    u32 f = utf8_prefix("\xF0\x9F\x98\x80", 4);
    u32 g = utf8_prefix("caf\xC3\xA9", 0);
    snprintf(detail, sizeof detail, "abc=%u eacute=%u cut=%u lead=%u 3of4=%u 4of4=%u 0=%u", a, b,
             c, d, e, f, g);
    step("utf8_prefix_cuts_on_boundary", a == 3 && b == 5 && c == 3 && d == 3 && e == 0 && f == 4
         && g == 0, detail);
}

/* ---- 6. json_bal_end / json_value_end / the remaining helpers ------------------------------ */

static void scanner_cases(void) {
    const char *o = "{\"a\":{\"b\":\"}\"},\"c\":[1,2]}tail";
    const char *e = json_bal_end(o);
    step("json_bal_end_nested", e && e - o == 25 && strcmp(e, "tail") == 0,
         e ? "the outer } closes the span" : "0");

    const char *s = "\"a\\\"b\"x";
    const char *se = json_value_end(s);
    step("json_value_end_string_escape", se && se - s == 6, "an escaped quote does not end it");
    const char *scal = "12.5,rest";
    step("json_value_end_scalar", json_value_end(scal) == scal + 4, "a scalar stops at the comma");
    step("json_bal_end_unterminated", json_bal_end("{\"a\":[1") == 0, "an open span -> 0");

    {
        const char *two = "{\"a\":{\"class\":\"nested\"},\"class\":\"top\"}";
        char out[32];
        int ok = json_string(two, "class", out, sizeof out) && strcmp(out, "nested") == 0;
        ok = ok && json_string_top(two, "class", out, sizeof out) && strcmp(out, "top") == 0;
        step("json_string_top_vs_nested", ok, out);
    }
    {
        const char *wp = "{\"origin\":[0,0,0],\"radius\":1e20}";
        float r = 0, c[3] = { 1, 1, 1 };
        int ok = json_float1_top(wp, "radius", &r) && r > 1e19
                 && json_float3_top(wp, "origin", c) && c[0] == 0.0f;
        char detail[64];
        snprintf(detail, sizeof detail, "radius=%g origin=%g", (double)r, (double)c[0]);
        step("json_float_radius_exponent", ok, detail);
    }
    {
        /* the depth-blind finders still read the nested first match: that is the difference the
           entity ops must not have (json_float3 on k_create sees keyValues.origin, a string) */
        float v[3] = { 0, 0, 0 }, r = 0;
        int ok = json_float3(k_create, "origin", v) == 0
              && json_float1("{\"fov\":90}", "fov", &r) && r == 90.0f;
        step("json_float3_find_is_depth_blind", ok,
             ok ? "nested origin is a string -> 0" : "expected the nested string to fail");
    }
    {
        char buf[32];
        float nan = 0.0f;
        SB b;
        nan /= 0.0f;
        sb_init(&b, buf, sizeof buf);
        sb_addi(&b, -32602); sb_add(&b, "/"); sb_addf(&b, 1.5f); sb_add(&b, "/");
        sb_addf(&b, nan);
        step("sb_addi_addf", strcmp(buf, "-32602/1.500/null") == 0, buf);
    }
}

/* ---- m2.18: node.setTransform value gates (WP-B, re-props.md Q2) -----------------------------
 * The three transform arrays are validated BEFORE the undo group opens, so a refused call leaves
 * no empty group behind. These are the negative cases: a 0/negative/NaN/over-1000 scale and an
 * over-1048576 origin component must all be refused (json_float3_top refuses a non-finite component
 * on its own, but the gates must too - the ops call json_float3_top and then the gate). */

static void xform_cases(void) {
    float ok3[3] = { 1.0f, 2.0f, 3.0f };
    step("xform_scales_accept", xform_scales_ok(ok3), "1,2,3 is a legal scale");

    float frac[3] = { 0.5f, 1.0f, 1.0f };
    step("xform_scales_accept_fraction", xform_scales_ok(frac), "0.5 is inside (0, 1000]");

    float max[3] = { 1000.0f, 1000.0f, 1000.0f };
    step("xform_scales_accept_max", xform_scales_ok(max), "1000 is the inclusive upper bound");

    float zero[3] = { 1.0f, 0.0f, 1.0f };
    step("xform_scales_refuse_zero", !xform_scales_ok(zero), "a 0 scale degenerates the transform");

    float neg[3] = { 1.0f, -1.0f, 1.0f };
    step("xform_scales_refuse_negative", !xform_scales_ok(neg), "a negative scale is refused");

    float over[3] = { 1.0f, 1000.5f, 1.0f };
    step("xform_scales_refuse_over_max", !xform_scales_ok(over), "1000.5 is above the bound");

    float nan = 0.0f, inf = 1.0f;
    nan /= 0.0f;
    inf /= 0.0f;
    float f_nan[3] = { 1.0f, nan, 1.0f }, f_inf[3] = { 1.0f, inf, 1.0f };
    step("xform_scales_refuse_nan_inf", !xform_scales_ok(f_nan) && !xform_scales_ok(f_inf),
         "NaN and +inf are not scales (NaN also compares false against 0)");

    float o_ok[3] = { 1048576.0f, -1048576.0f, 0.0f };
    step("xform_origin_accept_edges", xform_origin_ok(o_ok), "the bound is inclusive per component");

    float o_over[3] = { 0.0f, 0.0f, 1048577.0f };
    step("xform_origin_refuse_over", !xform_origin_ok(o_over), "1048577 is above the bound");

    float o_nan[3] = { nan, 0.0f, 0.0f };
    step("xform_origin_refuse_nan", !xform_origin_ok(o_nan), "a NaN component is refused");

    /* the refusal TEXT is built from the gate (review W2 #5: it said 65536 while the gate was
       1048576, so a client "fixing" its value from the message stayed over the bound) */
    char why[96];
    xform_origin_reason(why, sizeof why);
    step("xform_origin_text_names_the_bound",
         strcmp(why, "origin: each |component| <= 1048576") == 0, why);
    step("xform_origin_text_has_no_old_bound", strstr(why, "65536") == 0, why);

    /* `at least one of the three` (the op refuses an empty transform request) */
    step("xform_any_truth", xform_any(1, 0, 0) && xform_any(0, 1, 0) && xform_any(0, 0, 1)
         && xform_any(1, 1, 1) && !xform_any(0, 0, 0),
         "any non-zero presence flag passes, all-zero is refused");
}

/* ---- m2.18: entity.addConnections param gates (WP-B, re-props.md Q3) ------------------------- */

/* `parse` returns the count or -1; a small wrapper keeps the cases one line each */
static int conn_parse(const char *json, int *count) {
    static ConnSpec specs[CONN_MAX];
    return conn_array_parse(json, specs, CONN_MAX, count) ? *count : -1;
}

static void conn_cases(void) {
    int n = 0;
    step("conn_parse_minimal", conn_parse("[{\"output\":\"OnUser1\",\"target\":\"t\",\"input\":\"Kill\"}]", &n) == 1,
         "one entry with only the three required names");

    /* defaults: param "", delay 0, timesToFire -1 */
    static ConnSpec specs[CONN_MAX];
    conn_array_parse("[{\"output\":\"O\",\"target\":\"T\",\"input\":\"I\"}]", specs, CONN_MAX, &n);
    step("conn_parse_defaults", n == 1 && specs[0].param[0] == 0 && specs[0].delay == 0.0f
         && specs[0].timesToFire == -1, "param/delay/timesToFire defaults");

    char big[8192];
    {
        int k = 0;
        k += snprintf(big + k, sizeof big - (size_t)k, "[");
        for (int i = 0; i < CONN_MAX; i++)
            k += snprintf(big + k, sizeof big - (size_t)k, "%s{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\"}",
                          i ? "," : "");
        k += snprintf(big + k, sizeof big - (size_t)k, "]");
        step("conn_parse_64_ok", conn_parse(big, &n) == CONN_MAX, "64 entries is the documented maximum");
        /* one more entry than the maximum: refused, not truncated */
        char big2[8320];
        k = snprintf(big2, sizeof big2, "%s", big);
        big2[k - 1] = 0;
        snprintf(big2 + k - 1, sizeof big2 - (size_t)(k - 1), ",%s]", "{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\"}");
        step("conn_parse_65_refused", conn_parse(big2, &n) == -1, "65 entries is above the maximum");
    }

    step("conn_parse_empty_refused", conn_parse("[]", &n) == -1, "at least one connection is required");
    step("conn_parse_missing_input_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\"}]", &n) == -1, "input is required");
    step("conn_parse_empty_name_refused",
         conn_parse("[{\"output\":\"\",\"target\":\"t\",\"input\":\"i\"}]", &n) == -1,
         "an empty output is refused");
    step("conn_parse_unknown_member_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"targt\":\"typo\"}]", &n) == -1,
         "an unknown member is refused instead of dropped (a typo must not lose a field)");
    step("conn_parse_number_output_refused",
         conn_parse("[{\"output\":5,\"target\":\"t\",\"input\":\"i\"}]", &n) == -1,
         "a number where a name belongs is refused");
    step("conn_parse_param_optional_empty_ok",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"param\":\"\"}]", &n) == 1,
         "an empty param is legal (it is the default)");

    /* delay: [0, 3600], int or decimal, and no negative */
    step("conn_parse_delay_int_ok",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":3600}]", &n) == 1,
         "3600 is the inclusive upper bound");
    conn_array_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":0.5}]", specs, CONN_MAX, &n);
    step("conn_parse_delay_decimal", n == 1 && specs[0].delay == 0.5f, "a decimal delay is kept");
    step("conn_parse_delay_negative_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":-0.5}]", &n) == -1,
         "a negative delay is refused");
    step("conn_parse_delay_over_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":3600.5}]", &n) == -1,
         "3600.5 is above the bound");
    step("conn_parse_delay_string_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":\"1\"}]", &n) == -1,
         "a string delay is refused");

    /* timesToFire: -1 (always) or >= 1; 0 and a float are refused */
    conn_array_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":-1}]", specs, CONN_MAX, &n);
    step("conn_parse_times_always", n == 1 && specs[0].timesToFire == -1, "-1 means always");
    conn_array_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":3}]", specs, CONN_MAX, &n);
    step("conn_parse_times_count", n == 1 && specs[0].timesToFire == 3, "a positive count is kept");
    step("conn_parse_times_zero_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":0}]", &n) == -1,
         "0 is not a legal firing count");
    step("conn_parse_times_float_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":1.5}]", &n) == -1,
         "a float count is refused");
    step("conn_parse_times_below_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":-2}]", &n) == -1,
         "-2 is below the always sentinel");

    /* string length + control characters */
    char s255[CONN_STR_MAX + 1], s256[CONN_STR_MAX + 2], huge[600];
    for (int i = 0; i < CONN_STR_MAX; i++) s255[i] = 'x';
    s255[CONN_STR_MAX] = 0;
    for (int i = 0; i < CONN_STR_MAX + 1; i++) s256[i] = 'x';
    s256[CONN_STR_MAX + 1] = 0;
    step("conn_text_255_ok", conn_text_ok(s255, 1), "255 bytes is the maximum");
    step("conn_text_256_refused", !conn_text_ok(s256, 1), "256 bytes is above it");
    step("conn_text_empty_required", !conn_text_ok("", 1), "an empty required string is refused");
    step("conn_text_empty_optional", conn_text_ok("", 0), "an empty optional (param) is fine");
    step("conn_text_newline_refused", !conn_text_ok("a\nb", 1), "a newline is a control character");
    step("conn_text_del_refused", !conn_text_ok("a\x7f" "b", 1), "DEL is refused");
    step("conn_text_utf8_ok", conn_text_ok("caf\xc3\xa9", 1), "high bytes are not controls");
    /* the same rule through the array parser, including the cut-value refusal */
    snprintf(huge, sizeof huge, "[{\"output\":\"%s\",\"target\":\"t\",\"input\":\"i\"}]", s256);
    step("conn_parse_long_name_refused", conn_parse(huge, &n) == -1, "a 256-byte name is refused");
    snprintf(huge, sizeof huge, "[{\"output\":\"%s\",\"target\":\"t\",\"input\":\"i\"}]", s255);
    step("conn_parse_255_name_ok", conn_parse(huge, &n) == 1, "a 255-byte name is accepted");
    step("conn_parse_control_refused",
         conn_parse("[{\"output\":\"a\tb\",\"target\":\"t\",\"input\":\"i\"}]", &n) == -1,
         "an escaped tab is still a control character");
    step("conn_parse_malformed_refused",
         conn_parse("[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\"", &n) == -1,
         "an unterminated entry is refused");
}

/* ---- m2.18: entity.removeConnections index gates (WP-B, re-props.md Q3) ---------------------- */

static void rm_cases(void) {
    step("rm_mode_exclusive", rm_mode_ok(1, 0) && rm_mode_ok(0, 1) && !rm_mode_ok(1, 1) && !rm_mode_ok(0, 0),
         "exactly one of indices/match");

    int v[RM_INDEX_MAX], n = 0;
    step("rm_parse_single", rm_indices_parse("[3]", v, RM_INDEX_MAX, &n) && n == 1 && v[0] == 3,
         "[3]");
    step("rm_parse_multi", rm_indices_parse("[0, 2, 5]", v, RM_INDEX_MAX, &n) && n == 3
         && v[0] == 0 && v[1] == 2 && v[2] == 5, "[0, 2, 5]");
    step("rm_parse_duplicate_refused", !rm_indices_parse("[2,2]", v, RM_INDEX_MAX, &n),
         "a duplicate index is refused (the set must be the set that is removed)");
    step("rm_parse_empty_refused", !rm_indices_parse("[]", v, RM_INDEX_MAX, &n), "[] removes nothing");
    step("rm_parse_negative_refused", !rm_indices_parse("[-1]", v, RM_INDEX_MAX, &n), "a negative index");
    step("rm_parse_fraction_refused", !rm_indices_parse("[1.5]", v, RM_INDEX_MAX, &n),
         "an index is an integer, not a number token");
    step("rm_parse_exponent_refused", !rm_indices_parse("[1e2]", v, RM_INDEX_MAX, &n),
         "an exponent lexeme is refused (the nodeID rule)");
    step("rm_parse_over_i32_refused", !rm_indices_parse("[2147483648]", v, RM_INDEX_MAX, &n),
         "above INT32_MAX: the (int) cast would alias another entry");
    step("rm_parse_missing_comma_refused", !rm_indices_parse("[1 2]", v, RM_INDEX_MAX, &n),
         "a missing comma is malformed");
    step("rm_parse_not_array_refused", !rm_indices_parse("3", v, RM_INDEX_MAX, &n), "not an array");

    /* the cap: 512 ok, 513 refused */
    {
        char big[8192];
        int k = snprintf(big, sizeof big, "[");
        for (int i = 0; i < RM_INDEX_MAX; i++)
            k += snprintf(big + k, sizeof big - (size_t)k, "%s%d", i ? "," : "", i);
        k += snprintf(big + k, sizeof big - (size_t)k, "]");
        step("rm_parse_512_ok", rm_indices_parse(big, v, RM_INDEX_MAX, &n) && n == RM_INDEX_MAX,
             "512 indices is the maximum");
        int k2 = snprintf(big, sizeof big, "[");
        for (int i = 0; i <= RM_INDEX_MAX; i++)
            k2 += snprintf(big + k2, sizeof big - (size_t)k2, "%s%d", i ? "," : "", i);
        snprintf(big + k2, sizeof big - (size_t)k2, "]");
        step("rm_parse_513_refused", !rm_indices_parse(big, v, RM_INDEX_MAX, &n), "513 is above it");
    }

    /* descending order: RemoveConnection shifts the entries after the removed one, so the set is
       walked highest-first; the helper is what the op iterates */
    {
        int d[5] = { 2, 7, 0, 7, 3 };
        rm_sort_desc(d, 5);
        step("rm_sort_desc_order", d[0] == 7 && d[1] == 7 && d[2] == 3 && d[3] == 2 && d[4] == 0,
             "descending, descending-stable for equal keys");
        rm_sort_desc(d, 1);
        step("rm_sort_desc_single", d[0] == 7, "a one-element set is already ordered");
    }
    /* the (index << 16) id RemoveConnection takes, checked as the shift the op does */
    step("rm_index_shift", (u32)3 << 16 == 196608u && (u32)0 << 16 == 0u,
         "idx << 16 is the id form (low word 0)");
}

/* ---- the source really carries the region this test compiled --------------------------------- */

static int check_region_in_source(const char *path) {
    FILE *f = fopen(path, "rb");
    char *text;
    long n;
    if (!f) { printf("FAIL region source: cannot open %s\n", path); return 2; }
    fseek(f, 0, SEEK_END);
    n = ftell(f);
    fseek(f, 0, SEEK_SET);
    text = (char *)malloc((size_t)n + 1);
    if (!text) { printf("FAIL region source: out of memory\n"); fclose(f); return 2; }
    if (fread(text, 1, (size_t)n, f) != (size_t)n) {
        printf("FAIL region source: short read\n"); free(text); fclose(f); return 2;
    }
    fclose(f);
    text[n] = 0;
    g_cases++;
    if (strstr(text, "/* ---- entity JSON region begin ---- */") == NULL
        || strstr(text, "/* ---- entity JSON region end ---- */") == NULL
        || strstr(text, "static const char *json_find_top(") == NULL
        || strstr(text, "static const char *kv_next(") == NULL
        || strstr(text, "static int batch_op_next(") == NULL
        || strstr(text, "static int xform_scales_ok(") == NULL
        || strstr(text, "static int conn_array_parse(") == NULL
        || strstr(text, "static int rm_indices_parse(") == NULL) {
        printf("FAIL %-34s %s does not carry the entity JSON region markers\n", "region_in_source",
               path);
        g_failures++;
    } else {
        printf("PASS %-34s %s carries the marked region\n", "region_in_source", path);
    }
    free(text);
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        printf("usage: test_entity_json <bridge/mcp_bridge.c>\n");
        return 2;
    }
    if (check_region_in_source(argv[1]) != 0) return 2;

    top_level_cases();
    int_bound_cases();
    batch_cases();
    label_cases();
    recorded_cases();
    kv_cases();
    literal_cases();
    addjson_cases();
    utf8_cases();
    scanner_cases();
    xform_cases();          /* m2.18: node.setTransform value gates */
    conn_cases();           /* m2.18: entity.addConnections param gates */
    rm_cases();             /* m2.18: entity.removeConnections index gates */

    printf("%d cases, %d failures\n", g_cases, g_failures);
    return g_failures ? 1 : 0;
}
