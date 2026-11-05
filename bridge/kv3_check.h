/* kv3_check.h - bridge-side pre-load gate for a KV3-text file (m2.12 G1 lexical gate, G2 grammar).
 *
 * Why this exists: pet's CPetSession::ReloadDocument (session vtable slot 7, S2_SLOT_CBaseEditorSession_ReloadDocument)
 * on a file pet cannot parse does NOT stop at "a document with no root" (the old claim, which is
 * wrong). Live 2026-10-13 it recursed until the Wine main thread's stack overflowed
 * ("err:virtual:virtual_setup_exception stack overflow"), the main thread never returned and the
 * whole process wedged (SIGTERM did not stop it). Three inputs did that live:
 *   1. the whole file `{{{ not kv3\n`;
 *   2. a valid kv3 header followed by a truncated body;
 *   3. a BALANCED, syntactically-broken document (`_class = ` then `}` on the next line: a key
 *      with no value) - the input the old bracket/string/comment lexer passed straight through.
 * A wrong root `_class` does not wedge but kills the process outright (unhandled exception
 * c0000409, fail-fast), so it is a refusal here too. Unknown classes/fields inside the document are
 * tolerated by pet (see the "pet load robustness" table in protocol-diff-m2.12.md), so they are NOT
 * checked. pet.open needs the same gate because it loads through the same loader, and it cannot ask
 * pet for the host path (pet resolves its own addon-relative path), so this header also carries the
 * pure `<app>\content\csgo_addons\<addon>\<path>` derivation the gate resolves with.
 *
 * So this is a real KV3-text GRAMMAR check, not a bracket balance: it parses the document the way
 * Valve writes it - header, exactly one value at the root (which must be an object), `key = value`
 * members separated by whitespace (no commas between members), comma-separated arrays (a trailing
 * comma is legal), strings/raw strings/numbers/literals/flagged values/blob tokens, and comments
 * wherever whitespace is allowed - and refuses anything it cannot parse. It is strict on structure
 * and lenient on token content. It does NOT prove pet can build a document (only pet can); it
 * proves the file is the shape pet's loader is written for.
 *
 * Portability: pure C, no #include, no libc, no Windows types. The only requirement is that the
 * includer's u64 typedef is already in scope (mcp_bridge.c's own; test_kv3_check.c redefines the
 * identical one). Memory is indexed, never a C string: a NUL byte is a refusal, not a terminator.
 * No static mutable state: every entry point is re-entrant.
 *
 * Entry points, `1` = loadable, `0` = refuse with a short static *reason (never NULL, never a
 * buffer the caller owns):
 *   kv3_check(buf, len, &reason)   header names kv3 + KV3-text grammar (any `format:`)
 *   vpcf_check(buf, len, &reason)  the above with `format:vpcf` in the header, plus the ROOT
 *                                  object's own `_class` == "CParticleSystemDefinition"
 * plus the pure pet.open path derivation (kv3_addon_name / kv3_app_root / kv3_host_path), which
 * refuses with "no_addon" / "no_app_root" and lets the host unit test cover what would otherwise
 * only be exercised live.
 */
#ifndef KV3_CHECK_H
#define KV3_CHECK_H

/* bracket nesting cap: deeper than any real KV3 document (the deepest file in the installed content
 * tree - a .vmdl - nests 19 levels, and a .vpcf's operator lists 3-4); a runaway or truncated file
 * that keeps opening brackets is refused long before pet could recurse. It is also what keeps the
 * recursive-descent parse below bounded: the raw depth is checked first. */
#define KV3_CHECK_DEPTH_CAP 256

/* 1 when w (an ASCII literal) occurs in b[from..to), else 0. */
static int kv3_has(const char *b, u64 from, u64 to, const char *w) {
    u64 wl = 0, k, m;
    while (w[wl]) wl++;
    if (!wl || to < from || to - from < wl) return 0;
    for (k = from; k + wl <= to; k++) {
        for (m = 0; m < wl; m++) if (b[k + m] != w[m]) break;
        if (m == wl) return 1;
    }
    return 0;
}

/* 1 when the len bytes at s are exactly the ASCII literal w. */
static int kv3_eq(const char *s, u64 len, const char *w) {
    u64 i = 0;
    while (w[i]) {
        if (i >= len || s[i] != w[i]) return 0;
        i++;
    }
    return i == len;
}

/* 1 when the len bytes at s are the ASCII literal w, ASCII-case-insensitively. Upstream KV3 member
 * names are hashed with MurmurHash2LowerCase (CUtlStringToken in tier0/utlstringtoken.h, used by
 * CKV3MemberName in tier0/keyvalues3.h), so a member name is case-insensitive to the game. */
static int kv3_ci_eq(const char *s, u64 len, const char *w) {
    u64 i = 0;
    for (; w[i]; i++) {
        char a, b;
        if (i >= len) return 0;
        a = s[i];
        b = w[i];
        if (a >= 'A' && a <= 'Z') a = (char)(a + 32);
        if (b >= 'A' && b <= 'Z') b = (char)(b + 32);
        if (a != b) return 0;
    }
    return i == len;
}

/* 1 when the len bytes at s are the root particle-system class name (exact case: the class name is
 * a schema lookup key, not a member name). */
static int kv3_root_class_ok(const char *s, u64 len) {
    return kv3_eq(s, len, "CParticleSystemDefinition");
}

/* ---- the grammar pass: recursive descent over the token stream ---- */

typedef struct {
    const char *b;              /* the whole buffer, indexed (never a C string) */
    u64 n;
    u64 i;                      /* cursor */
    int depth;                  /* open objects + arrays (<= KV3_CHECK_DEPTH_CAP, enforced up front) */
    const char *reason;         /* set on refusal */
    int rootSeen;               /* the ROOT object has a `_class` member */
    int rootClassOk;            /* every root `_class` member is exactly the class name */
} Kv3;

/* the mutually recursive productions (the grammar below is recursive descent) */
static int kv3_value(Kv3 *p, int allowFlag, const char **strText, u64 *strLen);
static int kv3_value_token(Kv3 *p, int allowFlag, const char **strText, u64 *strLen);
static int kv3_object(Kv3 *p, int isRoot);
static int kv3_array(Kv3 *p);

/* skip whitespace and comments. 0 = refused (an unterminated block comment), *p->reason set. */
static int kv3_trivia(Kv3 *p) {
    for (;;) {
        unsigned char c;
        if (p->i >= p->n) return 1;
        c = (unsigned char)p->b[p->i];
        if (c == ' ' || c == '\t' || c == '\r' || c == '\n' || c == '\v' || c == '\f') {
            p->i++;
            continue;
        }
        if (c == '/' && p->i + 1 < p->n && p->b[p->i + 1] == '/') {
            p->i += 2;
            while (p->i < p->n && p->b[p->i] != '\n') p->i++;
            continue;
        }
        if (c == '/' && p->i + 1 < p->n && p->b[p->i + 1] == '*') {
            u64 j = p->i + 2;
            while (j + 1 < p->n && !(p->b[j] == '*' && p->b[j + 1] == '/')) j++;
            if (j + 1 >= p->n) { p->reason = "unterminated_comment"; return 0; }
            p->i = j + 2;
            continue;
        }
        return 1;
    }
}

/* one `"..."` string (escapes) or a `"""..."""` raw string at the cursor. On success the two
 * out-params describe the text INSIDE a plain `"..."` string; a raw string leaves them 0 (it is
 * text, not a name). Either out-param may be 0 when the caller does not care. */
static int kv3_string(Kv3 *p, const char **text, u64 *tlen) {
    const char *b = p->b;
    u64 n = p->n, j;
    if (text) *text = 0;
    if (tlen) *tlen = 0;
    if (p->i + 2 < n && b[p->i + 1] == '"' && b[p->i + 2] == '"') {   /* """...""" raw string */
        j = p->i + 3;
        for (;;) {
            if (j + 2 >= n) { p->reason = "unterminated_string"; return 0; }
            if (b[j] == '"' && b[j + 1] == '"' && b[j + 2] == '"') break;
            j++;
        }
        p->i = j + 3;
        return 1;
    }
    j = p->i + 1;
    for (;;) {
        if (j >= n) { p->reason = "unterminated_string"; return 0; }
        if (b[j] == '\\') { j += 2; continue; }
        if (b[j] == '"') break;
        j++;
    }
    if (text) *text = b + p->i + 1;
    if (tlen) *tlen = j - (p->i + 1);
    p->i = j + 1;
    return 1;
}

/* one binary blob `#[ hex bytes ]` (whitespace between the bytes, any of them). */
static int kv3_blob(Kv3 *p) {
    const char *b = p->b;
    u64 n = p->n, j;
    if (p->i + 1 >= n || b[p->i + 1] != '[') { p->reason = "bad_token"; return 0; }
    j = p->i + 2;
    for (;;) {
        unsigned char c;
        if (j >= n) { p->reason = "unbalanced"; return 0; }         /* the blob never closes */
        c = (unsigned char)b[j];
        if (c == ']') break;
        if (!((c >= '0' && c <= '9') || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F')
              || c == ' ' || c == '\t' || c == '\r' || c == '\n' || c == '\v' || c == '\f')) {
            p->reason = "bad_token";
            return 0;
        }
        j++;
    }
    p->i = j + 1;
    return 1;
}

/* the length of a number at b[i..n): int or float, optional sign, optional exponent (which
 * requires its own digits). 0 = not a number. */
static u64 kv3_number(const char *b, u64 n, u64 i) {
    u64 j = i;
    int digits = 0;
    if (j < n && (b[j] == '+' || b[j] == '-')) j++;
    while (j < n && b[j] >= '0' && b[j] <= '9') { j++; digits++; }
    if (j < n && b[j] == '.') {
        j++;
        while (j < n && b[j] >= '0' && b[j] <= '9') { j++; digits++; }
    }
    if (!digits) return 0;
    if (j < n && (b[j] == 'e' || b[j] == 'E')) {
        u64 k = j + 1;
        int ed = 0;
        if (k < n && (b[k] == '+' || b[k] == '-')) k++;
        while (k < n && b[k] >= '0' && b[k] <= '9') { k++; ed++; }
        if (ed) j = k;                        /* `1e` is not a number: stop before the `e` */
    }
    return j - i;
}

static int kv3_object(Kv3 *p, int isRoot) {
    const char *b = p->b;
    u64 n = p->n;
    p->i++;                                   /* the `{` */
    p->depth++;
    for (;;) {
        unsigned char c;
        int isClass = 0;
        const char *sv = 0;
        u64 sl = 0;
        if (!kv3_trivia(p)) return 0;
        if (p->i >= n) { p->reason = "unbalanced"; return 0; }       /* truncated inside the object */
        c = (unsigned char)b[p->i];
        if (c == '}') { p->i++; p->depth--; return 1; }
        if (c == ']') { p->reason = "unbalanced"; return 0; }
        if (c == ',') { p->reason = "comma_in_object"; return 0; }
        if (c == '"') {                                             /* a quoted key */
            const char *ks = 0;
            u64 kl = 0;
            if (!kv3_string(p, &ks, &kl)) return 0;
            isClass = isRoot && kv3_ci_eq(ks, kl, "_class");
        } else if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || c == '_') {
            u64 s = p->i;                                           /* identifier key */
            while (p->i < n) {
                unsigned char d = (unsigned char)b[p->i];
                if ((d >= 'A' && d <= 'Z') || (d >= 'a' && d <= 'z') || (d >= '0' && d <= '9')
                    || d == '_' || d == '.') p->i++;
                else break;
            }
            isClass = isRoot && kv3_ci_eq(b + s, p->i - s, "_class");
        } else {
            p->reason = "bad_key";
            return 0;
        }
        if (!kv3_trivia(p)) return 0;
        if (p->i >= n) { p->reason = "unbalanced"; return 0; }
        if (b[p->i] != '=') { p->reason = "expected_equals"; return 0; }
        p->i++;
        if (!kv3_value(p, 1, &sv, &sl)) return 0;                   /* the value is required */
        if (isClass) {                                              /* only the ROOT object's own,
                                                                       and every one of them counts */
            int ok = (sv != 0) && kv3_root_class_ok(sv, sl);
            if (!p->rootSeen) { p->rootSeen = 1; p->rootClassOk = ok; }
            else p->rootClassOk = p->rootClassOk && ok;
        }
    }
}

static int kv3_array(Kv3 *p) {
    const char *b = p->b;
    p->i++;                                   /* the `[` */
    p->depth++;
    for (;;) {
        unsigned char c;
        if (!kv3_trivia(p)) return 0;
        if (p->i >= p->n) { p->reason = "unbalanced"; return 0; }
        c = (unsigned char)b[p->i];
        if (c == ']') { p->i++; p->depth--; return 1; }
        if (c == '}') { p->reason = "unbalanced"; return 0; }
        if (!kv3_value(p, 1, 0, 0)) return 0;
        if (!kv3_trivia(p)) return 0;
        if (p->i >= p->n) { p->reason = "unbalanced"; return 0; }
        c = (unsigned char)b[p->i];
        if (c == ',') { p->i++; continue; }      /* a trailing comma is legal (Valve writes one) */
        if (c == ']') { p->i++; p->depth--; return 1; }
        p->reason = "expected_comma";
        return 0;
    }
}

/* one value TOKEN, with the cursor already past any trivia. allowFlag: a `name:value` flag is legal
 * at this position. The flag's own value is parsed with allowFlag = 0 and NO trivia, so flags
 * cannot chain (`a:b:"x"`), a bare `name:` is refused, and `resource: "x"` (whitespace between the
 * colon and the value) is refused - Valve writes `resource:"..."`. On success the two out-params
 * describe a PLAIN `"..."` value (0 for every other value kind, including a flag's value and a raw
 * string): only a bare string is a class name. */
static int kv3_value_token(Kv3 *p, int allowFlag, const char **strText, u64 *strLen) {
    const char *b = p->b;
    u64 n = p->n;
    unsigned char c;
    if (strText) *strText = 0;
    if (strLen) *strLen = 0;
    if (p->i >= n) { p->reason = p->depth ? "unbalanced" : "expected_value"; return 0; }
    c = (unsigned char)b[p->i];
    if (c == '{') return kv3_object(p, 0);
    if (c == '[') return kv3_array(p);
    if (c == '"') return kv3_string(p, strText, strLen);
    if (c == '#') return kv3_blob(p);
    if ((c >= '0' && c <= '9') || c == '-' || c == '+' || c == '.') {
        u64 l = kv3_number(b, n, p->i);
        if (!l) { p->reason = "bad_token"; return 0; }
        p->i += l;
        return 1;
    }
    if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || c == '_') {
        u64 s = p->i, e = p->i;
        while (e < n) {
            unsigned char d = (unsigned char)b[e];
            if ((d >= 'A' && d <= 'Z') || (d >= 'a' && d <= 'z') || (d >= '0' && d <= '9')
                || d == '_' || d == '.') e++;
            else break;
        }
        if (kv3_eq(b + s, e - s, "true") || kv3_eq(b + s, e - s, "false")
            || kv3_eq(b + s, e - s, "null")) {
            p->i = e;
            return 1;                            /* a literal wins over a flag of the same name */
        }
        if (allowFlag && e < n && b[e] == ':') { /* flagged value `name:value`, value required */
            p->i = e + 1;
            return kv3_value_token(p, 0, 0, 0);
        }
        p->reason = "bad_token";                 /* a bare word is not a value */
        return 0;
    }
    p->reason = "expected_value";
    return 0;
}

/* one value, trivia skipped first (every value position except a flag's own). */
static int kv3_value(Kv3 *p, int allowFlag, const char **strText, u64 *strLen) {
    if (!kv3_trivia(p)) return 0;
    return kv3_value_token(p, allowFlag, strText, strLen);
}

/* the depth cap is enforced on the RAW token stream (strings/comments skipped whole): the grammar
 * pass below must never recurse on `{{{{...`, which is a depth bomb before it is a bad key. */
static int kv3_shape(const char *b, u64 n, u64 i, const char **reason) {
    int d = 0;
    while (i < n) {
        unsigned char c = (unsigned char)b[i];
        if (c == ' ' || c == '\t' || c == '\r' || c == '\n' || c == '\v' || c == '\f') { i++; continue; }
        if (c == '/' && i + 1 < n && b[i + 1] == '/') {
            i += 2;
            while (i < n && b[i] != '\n') i++;
            continue;
        }
        if (c == '/' && i + 1 < n && b[i + 1] == '*') {
            u64 j = i + 2;
            while (j + 1 < n && !(b[j] == '*' && b[j + 1] == '/')) j++;
            if (j + 1 >= n) { *reason = "unterminated_comment"; return 0; }
            i = j + 2;
            continue;
        }
        if (c == '"') {
            if (i + 2 < n && b[i + 1] == '"' && b[i + 2] == '"') {
                u64 j = i + 3;
                for (;;) {
                    if (j + 2 >= n) { *reason = "unterminated_string"; return 0; }
                    if (b[j] == '"' && b[j + 1] == '"' && b[j + 2] == '"') break;
                    j++;
                }
                i = j + 3;
            } else {
                u64 j = i + 1;
                for (;;) {
                    if (j >= n) { *reason = "unterminated_string"; return 0; }
                    if (b[j] == '\\') { j += 2; continue; }
                    if (b[j] == '"') break;
                    j++;
                }
                i = j + 1;
            }
            continue;
        }
        if (c == '{' || c == '[') {
            if (d >= KV3_CHECK_DEPTH_CAP) { *reason = "too_deep"; return 0; }
            d++;
            i++;
            continue;
        }
        if (c == '}' || c == ']') {
            if (d) d--;                          /* balance itself is the grammar pass's job */
            i++;
            continue;
        }
        i++;
    }
    return 1;
}

/* wantVpcf: the header must name format:vpcf and the root object must carry the particle-system
 * class. The reasons (all short static strings) are the ones the bridge reports in
 * -32602 UnloadableFile {reason}; `ok` on success. */
static int kv3_parse(const char *buf, u64 len, int wantVpcf, const char **reason) {
    Kv3 p;
    u64 i = 0, hdr, j;
    *reason = "?";
    for (j = 0; j < len; j++) if (!buf[j]) { *reason = "binary"; return 0; }
    if (len >= 3 && (unsigned char)buf[0] == 0xEF && (unsigned char)buf[1] == 0xBB
        && (unsigned char)buf[2] == 0xBF) i = 3;
    while (i < len && (buf[i] == ' ' || buf[i] == '\t' || buf[i] == '\r' || buf[i] == '\n'
                       || buf[i] == '\v' || buf[i] == '\f')) i++;
    if (i + 4 > len || buf[i] != '<' || buf[i + 1] != '!' || buf[i + 2] != '-'
        || buf[i + 3] != '-') {
        *reason = "no_kv3_header";
        return 0;
    }
    i += 4;
    hdr = i;
    while (i + 3 <= len && !(buf[i] == '-' && buf[i + 1] == '-' && buf[i + 2] == '>')) i++;
    if (i + 3 > len) { *reason = "no_kv3_header"; return 0; }        /* the header never closes */
    if (!kv3_has(buf, hdr, i, "kv3")) { *reason = "no_kv3_header"; return 0; }
    if (wantVpcf && !kv3_has(buf, hdr, i, "format:vpcf")) { *reason = "not_vpcf"; return 0; }
    i += 3;
    if (!kv3_shape(buf, len, i, reason)) return 0;
    p.b = buf;
    p.n = len;
    p.i = i;
    p.depth = 0;
    p.reason = "ok";
    p.rootSeen = 0;
    p.rootClassOk = 0;
    if (!kv3_trivia(&p)) { *reason = p.reason; return 0; }
    if (p.i >= len || buf[p.i] != '{') { *reason = "no_root_object"; return 0; }
    if (!kv3_object(&p, 1)) { *reason = p.reason; return 0; }
    if (!kv3_trivia(&p)) { *reason = p.reason; return 0; }
    if (p.i < len) { *reason = "trailing_content"; return 0; }       /* a second root, a stray `}` */
    if (wantVpcf) {
        if (!p.rootSeen) { *reason = "no_root_class"; return 0; }
        if (!p.rootClassOk) { *reason = "wrong_root_class"; return 0; }
    }
    *reason = "ok";
    return 1;
}

/* any KV3-text document (kv3 header + grammar). A translation unit that only needs one of the two
 * entry points must not trip -Wunused-function (the bridge's .inc calls vpcf_check, the corpus
 * runner calls both); this is the same __attribute__((unused)) idiom the bridge already uses for a
 * statically-linked function it does not always call, and clang accepts it on every target. */
static int __attribute__((unused)) kv3_check(const char *buf, u64 len, const char **reason) {
    return kv3_parse(buf, len, 0, reason);
}

/* a .vpcf pet can load: kv3 header with format:vpcf + grammar + root `_class`. */
static int __attribute__((unused)) vpcf_check(const char *buf, u64 len, const char **reason) {
    return kv3_parse(buf, len, 1, reason);
}

/* ---- the pet.open host-path derivation (pure: the caller supplies the exe path and the command
   line, so the host unit test covers what would otherwise only ever run live). pet.open's caller
   names the file addon-content-relative (`particles/x.vpcf`) because that is what pet's OpenFile
   resolves itself; the gate turns it into the same file on the host:
   <app>\content\csgo_addons\<addon>\<path with '/' -> '\'>.
   The three helpers carry __attribute__((unused)) for the same reason the entry points do: an
   includer that only runs the grammar (the corpus runner) must not trip -Wunused-function. ---- */

/* append s to out[*len] within cap, keeping out NUL-terminated. */
static int kv3_append(char *out, u64 *len, u64 cap, const char *s) {
    u64 n = *len;
    if (!cap) return 0;
    for (u64 i = 0; s[i]; i++) {
        if (n + 1 >= cap) return 0;
        out[n++] = s[i];
    }
    out[n] = 0;
    *len = n;
    return 1;
}

/* the value of `-addon <name>` in a command line: `-addon mcp_test` (runtime.sh's form),
 * `-addon=mcp_test` or `-addon "mcp_test"`; the name may only be [A-Za-z0-9_] (a content directory
 * name). 0 = absent, empty, or not that shape (`no_addon`). */
static int __attribute__((unused)) kv3_addon_name(const char *cmdline, char *out, u64 cap) {
    const char *c = cmdline;
    if (!c || !cap) return 0;
    for (; *c; c++) {
        const char *p;
        u64 n = 0;
        if (c[0] != '-' || !kv3_ci_eq(c + 1, 5, "addon")) continue;
        if (c[6] != ' ' && c[6] != '\t' && c[6] != '=' && c[6] != '"') continue;
        p = c + 6;
        if (*p == '=') p++;
        while (*p == ' ' || *p == '\t') p++;
        if (*p == '"') {                        /* the whole quoted token must be the name */
            p++;
            while (*p && *p != '"') {
                char ch = *p;
                if (!((ch >= 'A' && ch <= 'Z') || (ch >= 'a' && ch <= 'z')
                      || (ch >= '0' && ch <= '9') || ch == '_')) return 0;
                if (n + 1 >= cap) return 0;
                out[n++] = ch;
                p++;
            }
            if (*p != '"') return 0;            /* unterminated */
            p++;
            if (*p && *p != ' ' && *p != '\t') return 0;
        } else {
            while (*p && *p != ' ' && *p != '\t' && *p != '"') {
                char ch = *p;
                if (!((ch >= 'A' && ch <= 'Z') || (ch >= 'a' && ch <= 'z')
                      || (ch >= '0' && ch <= '9') || ch == '_')) return 0;
                if (n + 1 >= cap) return 0;
                out[n++] = ch;
                p++;
            }
        }
        if (!n) return 0;
        out[n] = 0;
        return 1;
    }
    return 0;
}

/* <app> out of a module path: the last four path components must be game\bin\win64\cs2.exe
 * (ASCII-case-insensitively, both separators), the rest is <app>. 0 = not that layout
 * (`no_app_root`). */
static int __attribute__((unused)) kv3_app_root(const char *exe, u64 n, char *out, u64 cap) {
    u64 w[5], s = 0, alen;
    int wc = 0;
    if (!exe || !n || !cap || n >= 1024) return 0;
    for (;;) {                                  /* the start offset of each component, last 5 kept */
        u64 e = s;
        while (e < n && exe[e] != '\\' && exe[e] != '/') e++;
        if (wc < 5) w[wc++] = s;
        else { w[0] = w[1]; w[1] = w[2]; w[2] = w[3]; w[3] = w[4]; w[4] = s; }
        if (e >= n) break;
        s = e + 1;
    }
    if (wc < 4) return 0;
    if (!kv3_ci_eq(exe + w[wc - 4], w[wc - 3] - w[wc - 4] - 1, "game")) return 0;
    if (!kv3_ci_eq(exe + w[wc - 3], w[wc - 2] - w[wc - 3] - 1, "bin")) return 0;
    if (!kv3_ci_eq(exe + w[wc - 2], w[wc - 1] - w[wc - 2] - 1, "win64")) return 0;
    if (!kv3_ci_eq(exe + w[wc - 1], n - w[wc - 1], "cs2.exe")) return 0;
    alen = w[wc - 4];                           /* everything before the `game` component */
    if (alen && (exe[alen - 1] == '\\' || exe[alen - 1] == '/')) alen--;
    if (!alen || alen + 1 > cap) return 0;
    for (u64 i = 0; i < alen; i++) out[i] = exe[i];
    out[alen] = 0;
    return 1;
}

/* <app>\content\csgo_addons\<addon>\<rel with '/' -> '\'> */
static int __attribute__((unused)) kv3_host_path(const char *app, const char *addon, const char *rel, char *out, u64 cap) {
    u64 n = 0;
    if (!app || !app[0] || !addon || !addon[0] || !rel || !rel[0] || !cap) return 0;
    out[0] = 0;
    if (!kv3_append(out, &n, cap, app)) return 0;
    if (!kv3_append(out, &n, cap, "\\content\\csgo_addons\\")) return 0;
    if (!kv3_append(out, &n, cap, addon)) return 0;
    if (!kv3_append(out, &n, cap, "\\")) return 0;
    for (u64 i = 0; rel[i]; i++) {
        if (n + 1 >= cap) return 0;
        out[n++] = (rel[i] == '/') ? '\\' : rel[i];
        out[n] = 0;
    }
    return 1;
}

#endif /* KV3_CHECK_H */
