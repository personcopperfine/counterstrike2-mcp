/*
 * mcp_bridge.c - Phase 2/3 bridge tool DLL. Milestone M2.2 (task 0).
 *
 * Contract: docs/data/bridge-protocol.md. Status and remaining work: docs/README.md (Project status).
 *
 * History: M0 admission test (worknotes/m0/m0tool.c) -> M1 TCP+hello/ping ->
 * M2 queue/drain/doc.active -> M2.1 external-review hardening -> M2.2
 * prerequisites (phase 3 task 0):
 *   (a) module identity: generated pins (bridge/pins.py ->
 *       bridge_pins.h) for hammer.dll and toolframework2.dll - whole-file
 *       SHA-256 + mapped PE TimeDateStamp/SizeOfImage/CheckSum + debug RSDS
 *       GUID/age + prologue byte windows + vtable slot contents. Modules are
 *       verified on a worker thread (NOT_LOADED -> VERIFYING -> VERIFIED or
 *       MISMATCH), pinned once verified, and every method is gated on the
 *       verified mask; mismatch fails closed (-32017 PrerequisiteFailed) and
 *       capability JSON is built from one method table (built by build_caps).
 *   (d) deadlineMs defaults to 30000 (0 = none, negative = InvalidParams);
 *       the IPC thread sweeps and expires queued requests so a client gets a
 *       definite QueueTimeout while Hammer is stuck in a modal.
 *
 * Freestanding (no CRT): clang --target=x86_64-pc-windows-msvc + lld-link with
 * llvm-dlltool import libs (kernel32/ws2_32/bcrypt). Build: build.sh.
 * Evidence log: Z:\app\mcp_bridge.log
 */
typedef unsigned char u8;
typedef unsigned short u16;
typedef unsigned int u32;
typedef unsigned long long u64;
typedef void *HANDLE;
typedef int BOOL;

/* Include order (design R7, review-c1 §3): pins first, then the
   generated SDK header, then the hand-written typed interfaces. The ties (s2sdk.h's
   S2_UTLVEC_EXTERNAL alias and the S2HandleVec shape) are enforced by s2sdk_gen_ties.h,
   included as the LAST line of this TU (after mcp_bridge_game.inc). */
#include "bridge_pins.h"
#include "s2sdk_gen.h"
#include "s2sdk.h"
#include <float.h>               /* FLT_MAX: the FinishRenderingViews time argument */

/* ---------------- imports ---------------- */
__declspec(dllimport) u32 __stdcall GetCurrentProcessId(void);
__declspec(dllimport) u64 __stdcall GetTickCount64(void);
__declspec(dllimport) HANDLE __stdcall CreateThread(void *sa, u64 stack, u32(__stdcall *fn)(void *), void *param, u32 flags, u32 *tid);
__declspec(dllimport) u32 __stdcall WaitForSingleObject(HANDLE h, u32 ms);
__declspec(dllimport) BOOL __stdcall CloseHandle(HANDLE h);
__declspec(dllimport) HANDLE __stdcall GetProcessHeap(void);
__declspec(dllimport) void *__stdcall HeapAlloc(HANDLE heap, u32 flags, u64 bytes);
__declspec(dllimport) BOOL __stdcall HeapFree(HANDLE heap, u32 flags, void *mem);
__declspec(dllimport) void __stdcall InitializeCriticalSection(void *cs);
__declspec(dllimport) void __stdcall EnterCriticalSection(void *cs);
__declspec(dllimport) void __stdcall LeaveCriticalSection(void *cs);
__declspec(dllimport) BOOL __stdcall SleepConditionVariableCS(void *cv, void *cs, u32 ms);
__declspec(dllimport) void __stdcall WakeConditionVariable(void *cv);
__declspec(dllimport) void __stdcall WakeAllConditionVariable(void *cv);
__declspec(dllimport) HANDLE __stdcall CreateFileA(const char *name, u32 access, u32 share, void *sa, u32 disp, u32 flags, HANDLE tmpl);
__declspec(dllimport) BOOL __stdcall WriteFile(HANDLE h, const void *buf, u32 len, u32 *written, void *ov);
__declspec(dllimport) BOOL __stdcall MoveFileExA(const char *from, const char *to, u32 flags);
__declspec(dllimport) BOOL __stdcall DeleteFileA(const char *name);
__declspec(dllimport) BOOL __stdcall CreateDirectoryA(const char *path, void *sa);
__declspec(dllimport) u32 __stdcall GetEnvironmentVariableA(const char *name, char *buf, u32 size);
__declspec(dllimport) void *__stdcall GetModuleHandleA(const char *name);
__declspec(dllimport) void *__stdcall GetProcAddress(void *mod, const char *name);
__declspec(dllimport) const char *__stdcall GetCommandLineA(void);
__declspec(dllimport) BOOL __stdcall GetModuleHandleExW(u32 flags, const void *name, void **mod);
__declspec(dllimport) u32 __stdcall GetModuleFileNameA(void *mod, char *buf, u32 size);
__declspec(dllimport) int __stdcall ReadProcessMemory(void *proc, const void *addr, void *buf, u64 size, u64 *read);
__declspec(dllimport) BOOL __stdcall ReadFile(HANDLE h, void *buf, u32 len, u32 *read, void *ov);
__declspec(dllimport) u32 __stdcall GetFileSize(HANDLE h, u32 *high);
__declspec(dllimport) u32 __stdcall GetFileType(HANDLE h);
__declspec(dllimport) void __stdcall Sleep(u32 ms);
__declspec(dllimport) void __stdcall ExitProcess(u32 code);
__declspec(dllimport) int __stdcall BCryptGenRandom(void *alg, u8 *buf, u32 len, u32 flags);
__declspec(dllimport) int __stdcall BCryptOpenAlgorithmProvider(void **alg, const unsigned short *id, const unsigned short *impl, u32 flags);
__declspec(dllimport) int __stdcall BCryptCreateHash(void *alg, void **hash, u8 *obj, u32 objlen, u8 *secret, u32 slen, u32 flags);
__declspec(dllimport) int __stdcall BCryptHashData(void *hash, u8 *input, u32 len, u32 flags);
__declspec(dllimport) int __stdcall BCryptFinishHash(void *hash, u8 *out, u32 outlen, u32 flags);
__declspec(dllimport) int __stdcall BCryptDestroyHash(void *hash);
__declspec(dllimport) int __stdcall BCryptCloseAlgorithmProvider(void *alg, u32 flags);

typedef u64 SOCKET;
__declspec(dllimport) int __stdcall WSAStartup(u16 version, void *data);
__declspec(dllimport) SOCKET __stdcall socket(int af, int type, int proto);
__declspec(dllimport) int __stdcall bind(SOCKET s, const void *name, int namelen);
__declspec(dllimport) int __stdcall listen(SOCKET s, int backlog);
__declspec(dllimport) SOCKET __stdcall accept(SOCKET s, void *addr, int *addrlen);
__declspec(dllimport) int __stdcall getsockname(SOCKET s, void *name, int *namelen);
__declspec(dllimport) int __stdcall setsockopt(SOCKET s, int level, int name, const char *val, int len);
__declspec(dllimport) int __stdcall recv(SOCKET s, char *buf, int len, int flags);
__declspec(dllimport) int __stdcall send(SOCKET s, const char *buf, int len, int flags);
__declspec(dllimport) int __stdcall select(int nfds, void *rfds, void *wfds, void *efds, void *timeout);
__declspec(dllimport) int __stdcall shutdown(SOCKET s, int how);
__declspec(dllimport) int __stdcall closesocket(SOCKET s);

/* winsock fd_set (WSA fd_set: count + array) */
typedef struct { u32 fd_count; SOCKET fd_array[64]; } fd_set;
static void FD_ZERO(fd_set *s) { s->fd_count = 0; }
static void FD_SET(fd_set *s, SOCKET x) { if (s->fd_count < 64) s->fd_array[s->fd_count++] = x; }
static int fd_isset(const fd_set *s, SOCKET x) {
    for (u32 i = 0; i < s->fd_count; i++) if (s->fd_array[i] == x) return 1;
    return 0;
}

#define INVALID_SOCKET (~(SOCKET)0)
#define AF_INET 2
#define SOCK_STREAM 1
#define IPPROTO_TCP 6
#define SOL_SOCKET 0xFFFF
#define SO_RCVTIMEO 0x1006
#define SD_BOTH 2
#define BCRYPT_USE_SYSTEM_PREFERRED_RNG 2u
#define FILE_APPEND_DATA 0x0004u
#define GENERIC_READ 0x80000000u
#define GENERIC_WRITE 0x40000000u
#define FILE_SHARE_READ 0x00000001u
#define FILE_SHARE_WRITE 0x00000002u
#define FILE_SHARE_DELETE 0x00000004u
#define OPEN_EXISTING 3u
#define CREATE_ALWAYS 2u
#define FILE_ATTRIBUTE_NORMAL 0x80u
#define MOVEFILE_REPLACE_EXISTING 0x1u
#define INVALID_HANDLE_VALUE ((HANDLE)(long long)-1)
#define GET_MODULE_HANDLE_EX_FLAG_PIN 0x1u
#define MAX_LINE (1024u * 1024u)          /* advertised maxLineBytes */
#define QCAP 64
#define OUT_CAP 64
#define BRIDGE_BUILD "m2.20"
#define GAME_BUILD PIN_GAME_BUILD   /* generated by pins.py from signatures/ (sigcheck.CURRENT_BUILD) */
#define DEFAULT_DEADLINE_MS 30000u

#define GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS 0x4u
#define MODULE_PIN_FLAGS (GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS)

int _fltused;                             /* keep clang from emitting FP refs */

/* ---------------- freestanding memory primitives ---------------- */
void *memcpy(void *dst, const void *src, u64 n) {
    u8 *d = (u8 *)dst; const u8 *s = (const u8 *)src;
    while (n--) *d++ = *s++;
    return dst;
}
void *memset(void *dst, int v, u64 n) {
    u8 *d = (u8 *)dst;
    while (n--) *d++ = (u8)v;
    return dst;
}
void *memmove(void *dst, const void *src, u64 n) {
    u8 *d = (u8 *)dst; const u8 *s = (const u8 *)src;
    if (d == s || n == 0) return dst;
    if (d < s) { while (n--) *d++ = *s++; }
    else { d += n; s += n; while (n--) *--d = *--s; }
    return dst;
}

/* ---------------- string helpers ---------------- */
static u32 slen(const char *s) { u32 n = 0; while (s[n]) n++; return n; }
static int seq(const char *a, const char *b) { while (*a && *a == *b) { a++; b++; } return *a == *b; }
static int ct_eq(const char *a, const char *b) {   /* constant-time, length-checked */
    u32 la = slen(a), lb = slen(b);
    if (la != lb) return 0;
    u8 acc = 0;
    for (u32 i = 0; i < la; i++) acc |= (u8)(a[i] ^ b[i]);
    return acc == 0;
}
static void cpy(char *dst, const char *src, u32 cap) {
    u32 i = 0;
    while (src[i] && i + 1 < cap) { dst[i] = src[i]; i++; }
    dst[i] = 0;
}
/* ASCII-case-insensitive exact match: the §3.2 filters are case-insensitive, and since m2.18 so
   is SetKeyValue's key lookup (re-props.md Q1), which is why apply_keyvalues refuses a key whose
   only match is a different spelling - FindAttribute (and the `previous` pass) is exact. */
static int ci_seq(const char *a, const char *b) {
    for (;;) {
        char x = *a++, y = *b++;
        if (x >= 'A' && x <= 'Z') x += 32;
        if (y >= 'A' && y <= 'Z') y += 32;
        if (x != y) return 0;
        if (!x) return 1;
    }
}

/* ---------------- freestanding interlocked ops ----------------
   Wine's kernel32 does not implement the Interlocked* imports (a call aborts
   the process), so use the compiler's atomic builtins instead. */
static long il_cmpxchg(volatile long *dst, long v, long cmp) {
    __atomic_compare_exchange_n((long *)dst, &cmp, v, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST);
    return cmp;
}
static long il_xchg(volatile long *dst, long v) {
    return __atomic_exchange_n((long *)dst, v, __ATOMIC_SEQ_CST);
}
static long il_inc(volatile long *v) {
    return __atomic_add_fetch((long *)v, 1, __ATOMIC_SEQ_CST);
}

/* ---------------- log ---------------- */
static void logline(const char *s) {
    HANDLE h = CreateFileA("Z:\\app\\mcp_bridge.log", FILE_APPEND_DATA, FILE_SHARE_READ, 0, OPEN_EXISTING, 0, 0);
    if (h == INVALID_HANDLE_VALUE || !h)
        h = CreateFileA("Z:\\app\\mcp_bridge.log", FILE_APPEND_DATA, FILE_SHARE_READ, 0, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, 0);
    if (h == INVALID_HANDLE_VALUE || !h) return;
    u32 w = 0;
    WriteFile(h, s, slen(s), &w, 0);
    WriteFile(h, "\n", 1, &w, 0);
    CloseHandle(h);
}
static void lognum(const char *prefix, u64 v) {
    char buf[80]; u32 n = 0;
    while (prefix[n] && n < 56) { buf[n] = prefix[n]; n++; }
    char digits[24]; int d = 0;
    if (!v) digits[d++] = '0';
    while (v) { digits[d++] = (char)('0' + (v % 10)); v /= 10; }
    while (d > 0 && n < 70) buf[n++] = digits[--d];
    buf[n] = 0;
    logline(buf);
}

/* ============================ host-testable JSON scanners ====================================
 * Everything between the two `entity JSON region` markers is freestanding C with no game or
 * Windows dependency, so bridge/build.sh lifts this region verbatim out of this TU and compiles
 * the committed bridge/test_entity_json.c against it for the host (like json_text.h's
 * test_json_text.c): the unit test then proves the shipped text, not a copy of it. Keep the region
 * self-contained and keep both markers - the extraction fails the build loudly without them.
 * ============================================================================================= */
/* ---- entity JSON region begin ---- */
/* ---------------- string builder with overflow flag + JSON escaping ---------------- */
typedef struct { char *buf; u32 cap; u32 len; int ovf; } SB;
static void sb_init(SB *b, char *mem, u32 cap) { b->buf = mem; b->cap = cap; b->len = 0; b->ovf = 0; if (cap) mem[0] = 0; }
static void sb_putc(SB *b, char c) {
    if (b->len + 1 >= b->cap) { b->ovf = 1; return; }
    b->buf[b->len++] = c;
    b->buf[b->len] = 0;
}
static void sb_add(SB *b, const char *s) { while (*s) sb_putc(b, *s++); }
static void sb_addnum(SB *b, u64 v) {
    char digits[24]; int d = 0;
    if (!v) digits[d++] = '0';
    while (v) { digits[d++] = (char)('0' + (v % 10)); v /= 10; }
    while (d > 0) sb_putc(b, digits[--d]);
}
static void sb_addi(SB *b, long long v) {      /* signed (error codes are negative) */
    if (v < 0) { sb_putc(b, '-'); v = -v; }
    sb_addnum(b, (u64)v);
}

/* the byte length of the UTF-8 sequence at s (1..4, bounded by the `avail` bytes the caller can
   show), or 0 when it is not a valid one: bad lead byte, missing/short continuation, overlong
   form, a surrogate or beyond U+10FFFF. The byte values stay decimal because the m2.15 diff must
   add no 0x literal (bridge/test_entities.py --static): 194..223 is the two-byte lead range
   C2..DF, 224..239 the three-byte E0..EF, 240..244 the four-byte F0..F4, 128..191 the
   continuation range 80..BF, 192 the continuation mask C0, 63 the six-bit payload mask. */
static u32 utf8_seq(const char *s, u32 avail) {
    const u8 *p = (const u8 *)s;
    u32 n, cp, i;
    if (p[0] < 128) return 1;
    if (p[0] >= 194 && p[0] <= 223) { n = 2; cp = (u32)p[0] & 31u; }
    else if (p[0] >= 224 && p[0] <= 239) { n = 3; cp = (u32)p[0] & 15u; }
    else if (p[0] >= 240 && p[0] <= 244) { n = 4; cp = (u32)p[0] & 7u; }
    else return 0;
    if (n > avail) return 0;
    for (i = 1; i < n; i++) {
        if (((u32)p[i] & 192u) != 128u) return 0;
        cp = (cp << 6) | ((u32)p[i] & 63u);
    }
    if (n == 2 && cp < 128) return 0;                    /* overlong */
    if (n == 3 && cp < 2048) return 0;
    if (n == 4 && cp < 65536) return 0;
    if (cp >= 55296 && cp <= 57343) return 0;            /* D800..DFFF is not a code point */
    if (cp > 1114111) return 0;                          /* U+10FFFF */
    return n;
}

/* the longest prefix of the `n` bytes at out that does not end inside a multi-byte UTF-8
   sequence: the cut a truncating reader must make. A half sequence would make serde_json reject
   the whole frame (review opus #10), so probe_str trims every cut with this. */
static u32 utf8_prefix(const char *out, u32 n) {
    u32 k = n;
    while (k > 0 && ((u8)out[k - 1] & 192u) == 128u) k--;    /* walk back over continuations */
    if (k == 0) return 0;                                    /* nothing but continuation bytes */
    u32 avail = n - (k - 1);
    /* the sequence starting at out[k-1] is complete exactly when it uses every remaining byte */
    return utf8_seq(out + k - 1, avail) == avail ? n : k - 1;
}

/* JSON string escaping. A byte >= 128 is only emitted when it starts a valid UTF-8 sequence;
   anything else becomes U+FFFD, because one invalid or cut multibyte sequence makes serde_json
   reject the whole frame and hard-close the connection (review opus #10). */
static void sb_addjson(SB *b, const char *s) {
    sb_putc(b, '"');
    while (*s) {
        u8 c = (u8)*s;
        if (c == '"' || c == '\\') { sb_putc(b, '\\'); sb_putc(b, (char)c); s++; }
        else if (c == '\n') { sb_add(b, "\\n"); s++; }
        else if (c == '\r') { sb_add(b, "\\r"); s++; }
        else if (c == '\t') { sb_add(b, "\\t"); s++; }
        else if (c < 32) {
            static const char *hex = "0123456789abcdef";
            sb_add(b, "\\u00"); sb_putc(b, hex[c >> 4]); sb_putc(b, hex[c & 15]);
            s++;
        } else if (c < 128) { sb_putc(b, (char)c); s++; }
        else {
            u32 n = utf8_seq(s, 4);
            if (n) { for (u32 i = 0; i < n; i++) sb_putc(b, s[i]); s += n; }
            else {
                /* one U+FFFD per maximal subpart: the lead byte plus the continuation bytes that
                   could belong to its sequence, so a sequence cut at the end of the string is one
                   replacement, not one per byte */
                sb_add(b, "\xEF\xBF\xBD");
                u32 want = c >= 194 && c <= 223 ? 2 : c >= 224 && c <= 239 ? 3
                         : c >= 240 && c <= 244 ? 4 : 1;
                u32 got = 1;
                while (got < want && ((u8)s[got] & 192u) == 128u) got++;
                s += got;
            }
        }
    }
    sb_putc(b, '"');
}

/* ---------------- minimal JSON scanners (own protocol) ----------------
   json_find searches the whole text at any depth, strings included; it stays the finder for the
   pet.* / modeldoc.* / game.* arms. json_find_top matches a member of the object it is given and
   skips nested objects/arrays and string contents, which is what the entity op params need:
   serde_json writes the struct fields in key order, so `keyValues.origin` (a string) comes before
   a top-level `origin` (an array) and first-match json_find hands entity.create the wrong one
   (review opus #1: the entity landed at [0,0,0]). */
static const char *json_find(const char *json, const char *key) {
    for (const char *p = json; *p; p++) {
        if (*p != '"') continue;
        const char *q = p + 1;
        const char *k = key;
        while (*k && *q == *k) { q++; k++; }
        if (*k) continue;
        if (*q != '"') continue;
        q++;
        while (*q == ' ' || *q == '\t') q++;
        if (*q != ':') continue;
        q++;
        while (*q == ' ' || *q == '\t') q++;
        return q;
    }
    return 0;
}
/* the end (exclusive) of the balanced JSON object/array at p, or 0 when the text ends first. A
   brace or bracket inside a string does not count (review opus #2: `"message":"a}b"` used to end
   a batch op early and drop the rest of the ops). */
static const char *json_bal_end(const char *p) {
    if (*p != '{' && *p != '[') return 0;
    int depth = 0;
    for (; *p; p++) {
        if (*p == '"') {
            for (p++; *p && *p != '"'; p++) if (*p == '\\' && p[1]) p++;
            if (!*p) return 0;                           /* an unterminated string */
        } else if (*p == '{' || *p == '[') depth++;
        else if ((*p == '}' || *p == ']') && !--depth) return p + 1;
    }
    return 0;
}
/* the end (exclusive) of any JSON value at p: a string, a nested object/array, or a scalar lexeme */
static const char *json_value_end(const char *p) {
    const char *q = json_bal_end(p);
    if (q) return q;
    if (*p == '{' || *p == '[') return p;                /* unterminated: no progress */
    if (*p == '"') {
        for (p++; *p && *p != '"'; p++) if (*p == '\\' && p[1]) p++;
        return *p ? p + 1 : p;
    }
    while (*p && *p != ',' && *p != '}' && *p != ']') p++;
    return p;
}
/* the value of the TOP-LEVEL member `key` of the JSON object at `json`, 0 when it has none */
static const char *json_find_top(const char *json, const char *key) {
    if (!json) return 0;
    const char *p = json;
    while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r') p++;
    if (*p != '{') return 0;
    p++;
    for (;;) {
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r' || *p == ',') p++;
        if (*p != '"') return 0;                          /* '}' or malformed: no such member */
        const char *k = key, *q = p + 1;
        int match = 1;
        while (*k && *q && *q != '"' && *q == *k) { q++; k++; }
        if (*k || *q != '"') match = 0;                   /* keys are compared literally (none ours is escaped) */
        p = json_value_end(p);                            /* past the key string */
        while (*p == ' ' || *p == '\t') p++;
        if (*p != ':') return 0;
        p++;
        while (*p == ' ' || *p == '\t') p++;
        if (match) return p;
        p = json_value_end(p);                            /* past the value (string-aware) */
    }
}

/* the JSON string at p (the quote itself) copied to out with escapes collapsed */
static int json_string_at(const char *p, char *out, u32 max) {
    if (!p || *p != '"') return 0;
    p++;
    u32 n = 0;
    while (*p && *p != '"' && n + 1 < max) {
        if (*p == '\\' && p[1]) p++;
        out[n++] = *p++;
    }
    out[n] = 0;
    return 1;
}
static int json_string(const char *json, const char *key, char *out, u32 max) {
    return json_string_at(json_find(json, key), out, max);
}
static int json_string_top(const char *json, const char *key, char *out, u32 max) {
    return json_string_at(json_find_top(json, key), out, max);
}

/* one JSON number at *pp (sign, digits, fraction, exponent). serde_json writes |v| >= 1e16 and
   |v| < 1e-5 in exponent form (`1e20`, `-1.5e-7`), which the old digit-only parsers read as 1 or
   as 0 mid-token (review opus #8: a radius of 1e20 matched nothing, a create origin of 5.5e-17
   came back partly zero). Advances *pp past the token; 0 when p is not a number. */
static int json_num(const char **pp, double *out) {
    const char *p = *pp;
    int neg = 0;
    if (*p == '-') { neg = 1; p++; }
    if (*p < '0' || *p > '9') return 0;
    double v = 0;
    while (*p >= '0' && *p <= '9') { v = v * 10 + (*p - '0'); p++; }
    if (*p == '.') {
        p++;
        double f = 0.1;
        while (*p >= '0' && *p <= '9') { v += (*p - '0') * f; f *= 0.1; p++; }
    }
    if (*p == 'e' || *p == 'E') {
        const char *q = p + 1;
        int eneg = 0, ex = 0;
        if (*q == '+') q++;
        else if (*q == '-') { eneg = 1; q++; }
        if (*q >= '0' && *q <= '9') {                    /* a bare `e` stays a malformed token */
            while (*q >= '0' && *q <= '9') { if (ex < 1000) ex = ex * 10 + (*q - '0'); q++; }
            while (ex--) v = eneg ? v / 10.0 : v * 10.0;
            p = q;
        }
    }
    *out = neg ? -v : v;
    /* a lexeme with hundreds of digits (or a long exponent) accumulates to +-inf: refuse it
       instead of handing the caller an infinite origin/radius (review opus N3). The same test
       sb_addf uses, so this needs no math.h. */
    if (!(*out == *out) || *out - *out != 0.0) return 0;
    *pp = p;
    return 1;
}
/* a 32-bit value that is neither NaN nor +-inf. A double that is finite can still narrow to
   +-inf (1e39, or any literal above FLT_MAX), so the float results are checked too: an origin of
   1e39 would otherwise be stored as an infinite vector (review opus N3). */
static int f_finite(float v) { return v == v && v - v == 0.0f; }
/* [x,y,z] number array -> 3 floats; returns 1 on success. All three are parsed before any is
   stored, so a refused literal leaves the caller's vector exactly as it was - `fov`/`znear`/
   `zfar` are read with the return value ignored, and they must not receive the inf the check just
   rejected. */
static int json_float3_at(const char *p, float *out) {
    float f[3];
    if (!p || *p != '[') return 0;
    p++;
    for (int i = 0; i < 3; i++) {
        while (*p == ' ') p++;
        double v;
        if (!json_num(&p, &v)) return 0;
        f[i] = (float)v;
        if (!f_finite(f[i])) return 0;
        while (*p == ' ') p++;
        if (i < 2) { if (*p != ',') return 0; p++; }
    }
    for (int i = 0; i < 3; i++) out[i] = f[i];
    return 1;
}
static int json_float3(const char *json, const char *key, float *out) {
    return json_float3_at(json_find(json, key), out);
}
static int json_float3_top(const char *json, const char *key, float *out) {
    return json_float3_at(json_find_top(json, key), out);
}
/* single JSON number (int, decimal or exponent) */
static int json_float1_at(const char *p, float *out) {
    double v;
    if (!p || !json_num(&p, &v)) return 0;
    float f = (float)v;
    if (!f_finite(f)) return 0;                /* never store it: see json_float3_at */
    *out = f;
    return 1;
}
static int json_float1(const char *json, const char *key, float *out) {
    return json_float1_at(json_find(json, key), out);
}
static int json_float1_top(const char *json, const char *key, float *out) {
    return json_float1_at(json_find_top(json, key), out);
}

static void sb_addf(SB *b, float v) {
    if (v != v || v - v != 0.0f) { sb_add(b, "null"); return; }   /* NaN or +-inf is not a JSON number */
    if (v < 0) { sb_putc(b, '-'); v = -v; }
    u32 ip = (u32)v;
    u32 frac = (u32)((v - (float)ip) * 1000.0f + 0.5f);
    if (frac >= 1000) { ip++; frac = 0; }
    sb_addnum(b, ip);
    if (frac) {
        sb_putc(b, '.');
        sb_putc(b, (char)('0' + frac / 100));
        sb_putc(b, (char)('0' + (frac / 10) % 10));
        sb_putc(b, (char)('0' + frac % 10));
    }
}

/* the largest integer json_int_at accepts: INT32_MAX, so a nodeID/limit lexeme is either inside
   the callers' documented range or refused (decimal only - the region may not carry a 0x literal). */
#define JSON_INT_MAX 2147483647LL

static int json_int(const char *json, const char *key, long long *out) {
    const char *p = json_find(json, key);
    if (!p) return 0;
    int neg = 0;
    if (*p == '-') { neg = 1; p++; }
    if (*p < '0' || *p > '9') return 0;
    long long v = 0;
    while (*p >= '0' && *p <= '9') {
        /* this reader has no semantic cap: the values its callers read (epoch, width/height/format,
           frames, node.list's limit, protocol, deadlineMs) are all far below both bounds, so the
           only thing it must not do is wrap. The check is before the multiply, because the
           accumulation used to be signed overflow - UB, and a wrap in practice (review opus N3:
           a 20-digit nodeID became a different node). */
        int d = *p - '0';
        if (v > (9223372036854775807LL - d) / 10) return 0;
        v = v * 10 + d;
        p++;
    }
    *out = neg ? -v : v;
    return 1;
}
/* a plain decimal integer at p: no fraction, no exponent, no quotes. A lenient prefix parse would
   take `4294967326` or `1.5` for a nodeID (the caller's (int) cast then aliases another node -
   review opus #5 / ds #1), so the integer params go through this one. It is also bounded to
   INT32_MAX (NODEID_MAX) *before* every multiply: nothing wraps (review opus N3), and a value the
   caller would refuse as out of range is refused here instead of being handed over as a garbage
   int. No caller accepts a negative (the nodeID/limit checks are `>= 0`/`1..2000`), but the sign is
   parsed so `-1` is still a readable -32602 `out of range`, not a parse error. */
static int json_int_at(const char *p, long long *out) {
    if (!p) return 0;
    int neg = 0;
    if (*p == '-') { neg = 1; p++; }
    if (*p < '0' || *p > '9') return 0;
    long long v = 0;
    while (*p >= '0' && *p <= '9') {
        int d = *p - '0';
        if (v > (JSON_INT_MAX - d) / 10) return 0;          /* would exceed INT32_MAX */
        v = v * 10 + d;
        p++;
    }
    if (*p == '.' || *p == 'e' || *p == 'E') return 0;      /* a number lexeme, not an integer */
    *out = neg ? -v : v;
    return 1;
}
static int json_int_top(const char *json, const char *key, long long *out) {
    return json_int_at(json_find_top(json, key), out);
}

/* one JSON string element at p decoded into out (up to 4 UTF-8 bytes). Returns the input bytes
   consumed (0 at the closing quote/NUL, so every caller loop advances) and *outn the bytes
   written. Handles the short escapes, `\/` and `\uXXXX` with surrogate pairs (review opus #16:
   `\uXXXX`/`\b`/`\f` used to land in the value as literal `uXXXX`/`b`/`f`); an unknown escape keeps
   the escaped byte, exactly as the m2.14 code did. */
static u32 json_estr(const char *p, char out[4], u32 *outn) {
    u8 c = (u8)p[0];
    u32 cp, i;
    *outn = 0;
    if (!c || c == '"') return 0;
    if (c != '\\') { out[0] = (char)c; *outn = 1; return 1; }
    u8 e = (u8)p[1];
    if (!e) { out[0] = '\\'; *outn = 1; return 1; }          /* a trailing backslash: malformed frame */
    switch (e) {
        case '"': case '\\': case '/': out[0] = (char)e; *outn = 1; return 2;
        case 'b': out[0] = '\b'; *outn = 1; return 2;
        case 'f': out[0] = '\f'; *outn = 1; return 2;
        case 'n': out[0] = '\n'; *outn = 1; return 2;
        case 'r': out[0] = '\r'; *outn = 1; return 2;
        case 't': out[0] = '\t'; *outn = 1; return 2;
        case 'u': break;
        default: out[0] = (char)e; *outn = 1; return 2;       /* unknown escape: the escaped byte */
    }
    cp = 0;
    for (i = 0; i < 4; i++) {
        u8 h = (u8)p[2 + i];
        u32 d = h >= '0' && h <= '9' ? (u32)(h - '0')
               : h >= 'a' && h <= 'f' ? (u32)(h - 'a' + 10)
               : h >= 'A' && h <= 'F' ? (u32)(h - 'A' + 10) : 99u;
        if (d > 15) { out[0] = 'u'; *outn = 1; return 2; }    /* not \uXXXX: the old `u` */
        cp = cp * 16 + d;
    }
    u32 used = 6;
    if (cp >= 55296 && cp <= 56319 && p[6] == '\\' && p[7] == 'u') {
        u32 lo = 0, ok = 1;                                  /* a high surrogate: try its low half */
        for (i = 0; i < 4; i++) {
            u8 h = (u8)p[8 + i];
            u32 d = h >= '0' && h <= '9' ? (u32)(h - '0')
                   : h >= 'a' && h <= 'f' ? (u32)(h - 'a' + 10)
                   : h >= 'A' && h <= 'F' ? (u32)(h - 'A' + 10) : 99u;
            if (d > 15) { ok = 0; break; }
            lo = lo * 16 + d;
        }
        if (ok && lo >= 56320 && lo <= 57343) {
            cp = 65536 + ((cp - 55296) << 10) + (lo - 56320);
            used = 12;
        }
    }
    if (cp >= 55296 && cp <= 57343) cp = 65533;              /* a lone surrogate -> U+FFFD */
    if (cp > 1114111) cp = 65533;
    if (cp == 0) cp = 65533;                                 /* \u0000 would end the C string a name/value is read as */
    if (cp < 128) { out[0] = (char)cp; *outn = 1; }
    else if (cp < 2048) {
        out[0] = (char)(192 | (cp >> 6)); out[1] = (char)(128 | (cp & 63)); *outn = 2;
    } else if (cp < 65536) {
        out[0] = (char)(224 | (cp >> 12)); out[1] = (char)(128 | ((cp >> 6) & 63));
        out[2] = (char)(128 | (cp & 63)); *outn = 3;
    } else {
        out[0] = (char)(240 | (cp >> 18)); out[1] = (char)(128 | ((cp >> 12) & 63));
        out[2] = (char)(128 | ((cp >> 6) & 63)); out[3] = (char)(128 | (cp & 63)); *outn = 4;
    }
    return used;
}

/* the longest keyvalue string the bridge writes. Rust caps a keyvalue at 512 B (hammer-mcp
   MAX_VALUE_BYTES); the bridge takes <= 1024 and refuses anything longer instead of cutting it
   (review opus #7 / ds #3: a 512-byte value used to be stored as 511 bytes with applied:true). */
#define KV_VALUE_MAX 1024

/* the literal at p is exactly `lit` (a C string). The compare stops at p's NUL - it is what
   strncmp(p, lit, strlen(lit)) would do in the CRT this build does not link - so a frame that ends
   inside a literal is malformed instead of stepping past its terminator: the old scan recognised
   true/false/null from one or two bytes (`p[0] == 't' && p[1] == 'r'`, `p[0] == 'n'`) and then
   returned p + 4 / p + 5, so a raw frame ending in `"k":tr`, `"k":n` or `"k":fa` handed the caller
   a pointer past its NUL (review L6). */
static int lit_at(const char *p, const char *lit) {
    while (*lit) { if (*p != *lit) return 0; p++; lit++; }
    return 1;
}

/* iterate a JSON object of keyvalue pairs at depth 1. Returns the position after the value, or 0
   when done. Value kinds: 5 string, 4 bool, 2 int, 3 float.
   nval (m2.15 §3.4) receives the raw number token text exactly as it was written ("4", not
   "4.000000", `1e20` for an exponent) so a number applied to a string attribute keeps the
   caller's text; it is set to "" for every other kind. It also carries an out-of-range number
   token's text, so a String attribute can still store it (review L6).
   `over` (review opus #7/#8, ds #3, L6) is 1 when a string value did not fit svalcap and 2 when a
   number token is outside what the DME setters can hold; either way the token is consumed and the
   callers must refuse *that key* instead of writing a cut or wrapped value. The rest of the object
   still parses, so the keys after it are not lost. Every loop provably advances. */
static const char *kv_next(const char *p, char *name, u32 namesz, int *kind,
                           char *sval, u32 svalcap, long long *ival, double *fval,
                           char *nval, u32 nvalcap, int *over) {
    if (over) *over = 0;
    if (nval && nvalcap) nval[0] = 0;
    if (!p) return 0;
    while (*p == ' ' || *p == ',' || *p == '\n' || *p == '\t' || *p == '\r') p++;
    if (*p != '"') return 0;
    p++;
    u32 n = 0;
    while (*p && *p != '"') {
        char ub[4]; u32 used, on = 0;
        used = json_estr(p, ub, &on);
        if (!used) break;
        if (n + on + 1 <= namesz) { for (u32 i = 0; i < on; i++) name[n + i] = ub[i]; n += on; }
        p += used;                                           /* an over-long name is dropped, not cut */
    }
    name[n] = 0;
    if (*p != '"') return 0;
    p++;
    while (*p == ' ' || *p == ':') p++;
    if (*p == '"') {
        p++;
        u32 v = 0;
        int longv = 0;
        while (*p && *p != '"') {
            char ub[4]; u32 used, on = 0;
            used = json_estr(p, ub, &on);
            if (!used) break;
            if (v + on + 1 > svalcap) longv = 1;              /* keep scanning so later keys parse */
            else { for (u32 i = 0; i < on; i++) sval[v + i] = ub[i]; v += on; }
            p += used;
        }
        sval[v] = 0;
        if (*p == '"') p++;
        *kind = 5;
        if (over) *over = longv;
        return p;
    }
    /* the whole literal, never its first bytes: a truncated one is malformed and the scan stops at
       the frame's NUL instead of making the old 4/5-byte jump past it (`"k":tr`, `"k":n`, `"k":fa`;
       review L6) */
    if (lit_at(p, "true")) { *kind = 4; *ival = 1; return p + 4; }
    if (lit_at(p, "false")) { *kind = 4; *ival = 0; return p + 5; }
    if (lit_at(p, "null")) { *kind = 5; sval[0] = 0; return p + 4; }  /* null -> empty string */
    if (*p == '-' || (*p >= '0' && *p <= '9')) {
        const char *nstart = p;
        int neg = 0, isf = 0;
        double d = 0;
        if (*p == '-') { neg = 1; p++; }
        while (*p >= '0' && *p <= '9') { d = d * 10 + (*p - '0'); p++; }
        if (*p == '.') { isf = 1; p++; double f = 0.1; while (*p >= '0' && *p <= '9') { d += (*p - '0') * f; f *= 0.1; p++; } }
        if (*p == 'e' || *p == 'E') {
            const char *q = p + 1;
            int eneg = 0, ex = 0;
            if (*q == '+') q++;
            else if (*q == '-') { eneg = 1; q++; }
            if (*q >= '0' && *q <= '9') {                     /* an exponent makes it a float token */
                isf = 1;
                while (*q >= '0' && *q <= '9') { if (ex < 1000) ex = ex * 10 + (*q - '0'); q++; }
                while (ex--) d = eneg ? d / 10.0 : d * 10.0;
                p = q;
            }
        }
        if (neg) d = -d;
        /* the token text is kept for every number token, in-range or not: a String attribute takes
           the literal text (`1e39` stored as the text "1e39" - the m2.15 §3.4 `nval` path), so an
           out-of-range token on a String attribute is not an error (review L6). */
        if (nval && nvalcap) {
            u32 tl = (u32)(p - nstart);
            if (tl >= nvalcap) tl = nvalcap - 1;
            for (u32 i = 0; i < tl; i++) nval[i] = nstart[i];
            nval[tl] = 0;
        }
        /* a token the DME setters cannot hold is *refused for that key*, never silently dropped
           with every key after it and never a wrapped value: an integer past the long long range is
           UB on the cast below (and `(int)ival` then stores a different number), and a float past
           FLT_MAX would be written to a float attribute as +-inf. `*over = 2` (1 = over-long
           string, the `sval` path) consumes the token, keeps its text in `nval` and lets the apply
           pass decide per attribute type: a String attribute stores the text, an int/float/bool one
           refuses the key with a warning and the keys after it still parse (review L6). The double
           bounds are exactly +-2^63, so the integer cast below is in range. Same road as the
           json_int_at/json_num bounds above (review opus N3); MCP cannot reach it - the Rust layer
           sends every keyvalue as a JSON string. */
        if (isf) {
            *kind = 3;
            float fv = (float)d;
            if (!f_finite(fv)) { if (over) *over = 2; return p; }
            *fval = d;
        } else if (d >= 9223372036854775808.0 || d < -9223372036854775808.0) {
            *kind = 2;
            if (over) *over = 2;
            return p;
        } else {
            *kind = 2;
            *ival = (long long)d;
        }
        return p;
    }
    return 0;
}

/* edit.batch: copy the next {"method":...,"params":{...}} op object into opbuf; returns 0 at end.
   Braces/brackets and the closing quote inside a string do not count (review opus #2: a
   keyvalue containing `}` ended the op early and silently dropped every following op). */
static int batch_op_next(const char **pp, char *opbuf, u32 cap, char *pbuf, u32 pcap) {
    const char *p = *pp;
    while (*p == ' ' || *p == ',') p++;
    if (*p != '{') { *pp = p; return 0; }
    const char *q = json_bal_end(p);
    if (!q) { *pp = p + 1; return 0; }                       /* truncated: stop, never a cut op */
    u32 n = (u32)(q - p);
    if (n + 1 > cap) n = cap - 1;
    memcpy(opbuf, p, n);
    opbuf[n] = 0;
    const char *pp2 = json_find_top(opbuf, "params");        /* the op's own params member */
    pbuf[0] = 0;
    if (pp2 && *pp2 == '{') {
        const char *pe = json_bal_end(pp2);
        u32 m = pe ? (u32)(pe - pp2) : 0;
        if (m + 1 > pcap) m = pcap - 1;
        if (m) { memcpy(pbuf, pp2, m); pbuf[m] = 0; }
    }
    *pp = q;
    return 1;
}
/* edit.batch: does the ops array contain an `entity.create` op? The undo-group label depends on
   it (see the edit.batch arm): hammer-mcp's undo_last decrements its unsaved-edit counter for any
   popped "MCP: batch" group, which is right exactly when the batch created something - the pop
   reverts that create - but a batch of nothing but entity.setKeyValues ops reverts nothing when
   popped, so it has to carry the keyvalue label the counter leaves alone (review section 3). The
   scan is here (freestanding, text only) so the host test can pin the decision. `ops` is the array
   text; an empty or unreadable array has no create op, which is the safe direction: nothing was
   created. The splitter provably advances or returns 0, so the walk cannot spin. */
static int batch_has_create(const char *ops) {
    static char opbuf[8192], pbuf[8192];
    const char *p = ops + 1;
    for (;;) {
        const char *before = p;
        if (!batch_op_next(&p, opbuf, sizeof opbuf, pbuf, sizeof pbuf)) break;
        if (p == before) break;
        char method[64];
        method[0] = 0;
        if (!json_string(opbuf, "method", method, sizeof method)) continue;
        const char *want = "entity.create";
        const char *a = method;
        while (*a && *want == *a) { a++; want++; }
        if (*a == 0 && *want == 0) return 1;
    }
    return 0;
}
/* edit.batch (review W2 #7): is this node already recorded in the group the batch opened? A node
   the batch created has its CreateNodeByName record, and a node an earlier setKeyValues op of the
   same batch wrote was recorded then - Hammer records each node once per group, and a second
   RecordNodeTree's effect on the undo is unverified, so it is skipped. The old rule compared the
   handle against the 16-entry handle table, which a 17th create or a second setKeyValues on an
   existing node walked straight past. Text-free and small enough for the host test to pin it. */
static int batch_node_recorded(const int *ids, int count, int id) {
    for (int i = 0; i < count; i++) if (ids[i] == id) return 1;
    return 0;
}
/* edit.batch (review W2 #7): should this setKeyValues op RecordNodeTree? 0 when the group already
   holds the node (a node the batch created, or one an earlier op of the same batch wrote) - the
   caller then passes record=0 into op_set_keyvalues. A 1 answer appends the node to the batch's own
   list, so a second op on the same node takes the 0 branch. Past `cap` the answer is still 1
   (record: the pre-W2 behaviour, safe) but nothing is remembered - a missing record could lose an
   undo, an extra one is what the code did before. */
static int batch_record_decision(int *ids, int *count, int cap, int id) {
    if (id < 0 || batch_node_recorded(ids, *count, id)) return 0;
    if (*count < cap) ids[(*count)++] = id;
    return 1;
}
/* ---------------- m2.18 prop-edit param gates (node.setTransform / entity.addConnections /
   entity.removeConnections) ----------------
   Pure text and value validation, so the host unit test exercises this shipped text
   (bridge/test_entity_json.c); the ops call these BEFORE they open an undo group, which is what
   makes a refused call leave no empty group behind (the review-13 rule). The live-tree side (the
   node, the connection vector) is NOT here: it cannot compile off Windows. */
#define XFORM_ORIGIN_MAX 1048576        /* node.setTransform: |component| <= this (int so the
                                           refusal text is built from the same number) */
#define XFORM_SCALES_MAX 1000.0f       /* each scale must be in (0, XFORM_SCALES_MAX] */
#define CONN_MAX 64                    /* entity.addConnections: at most this many per call */
#define CONN_STR_MAX 255               /* bytes, per output/target/input/param string */
#define CONN_DELAY_MAX 3600.0f         /* seconds */
#define RM_INDEX_MAX 512               /* entity.removeConnections: at most this many indices (and the add/get cap) */

/* the region's own string compare (the bridge's seq() is defined outside the lifted region, which
   bridge/build.sh compiles stand-alone) */
static int prm_seq(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return *a == *b;
}
/* copy a validated NUL-terminated string of at most cap-1 bytes */
static void prm_copy(char *dst, u32 cap, const char *src) {
    u32 n = 0;
    while (src[n] && n + 1 < cap) n++;
    memcpy(dst, src, n);
    dst[n] = 0;
}

/* a [x,y,z] a caller means to write: finite and within +-XFORM_ORIGIN_MAX per component (the
   transform setters feed transform/bounds math; 1e30 is not a position anyone means) */
static int xform_origin_ok(const float v[3]) {
    for (int i = 0; i < 3; i++)
        if (!f_finite(v[i]) || v[i] > (float)XFORM_ORIGIN_MAX || v[i] < -(float)XFORM_ORIGIN_MAX) return 0;
    return 1;
}
/* each scale finite and in (0, XFORM_SCALES_MAX]: 0, negative and NaN are refused (Hammer's own
   "Clear Rotation and Scale" writes 1,1,1 - a 0 scale degenerates the transform), and an
   unbounded one overflows the bounds/mesh-edit math the setter runs */
static int xform_scales_ok(const float s[3]) {
    for (int i = 0; i < 3; i++)
        if (!f_finite(s[i]) || !(s[i] > 0.0f) || s[i] > XFORM_SCALES_MAX) return 0;
    return 1;
}
/* node.setTransform must carry at least one of origin/angles/scales (the whole point of the op) */
static int xform_any(int has_origin, int has_angles, int has_scales) {
    return (has_origin != 0) || (has_angles != 0) || (has_scales != 0);
}

/* one output/target/input/param string: at most CONN_STR_MAX bytes, no C0 or DEL control byte
   (a multi-line/control string in an I/O name is never what a caller means and reads as garbage
   in Hammer's output tab). `required` also refuses the empty string. */
static int conn_text_ok(const char *s, int required) {
    if (!s) return 0;
    u32 n = 0;
    for (const u8 *p = (const u8 *)s; *p; p++, n++) {
        if (n >= CONN_STR_MAX) return 0;
        if (*p < 32 || *p == 127) return 0;
    }
    if (required && !n) return 0;
    return 1;
}

/* the validated form of one entity.addConnections entry (defaults applied: param "",
   delay 0, timesToFire -1) */
typedef struct {
    char output[CONN_STR_MAX + 1];
    char target[CONN_STR_MAX + 1];
    char input[CONN_STR_MAX + 1];
    char param[CONN_STR_MAX + 1];
    float delay;
    int timesToFire;
} ConnSpec;

/* parse and validate entity.addConnections' `connections` array: 1..max entries of
   {output, target, input, param?, delay?, timesToFire?} (re-props.md Q3). Every rule is checked
   here, before any write; an unknown member is refused instead of dropped, because a typo would
   otherwise silently lose a field the caller believes was applied. 0 = refused, nothing to write. */
static int conn_array_parse(const char *vp, ConnSpec *out, int max, int *count) {
    *count = 0;
    if (!vp || *vp != '[') return 0;
    const char *p = vp + 1;
    for (;;) {
        while (*p == ' ' || *p == ',' || *p == '\n' || *p == '\t' || *p == '\r') p++;
        if (*p == ']') return *count > 0;
        if (*p != '{' || *count >= max) return 0;
        ConnSpec *c = &out[*count];
        int has_output = 0, has_target = 0, has_input = 0;
        c->output[0] = c->target[0] = c->input[0] = c->param[0] = 0;
        c->delay = 0.0f;
        c->timesToFire = -1;
        p++;
        for (;;) {
            char name[64], sval[CONN_STR_MAX + 1], nval[96];
            int kind = 0, over = 0;
            long long ival = 0;
            double fval = 0;
            const char *before = p;      /* kv_next provably advances */
            p = kv_next(p, name, sizeof name, &kind, sval, sizeof sval, &ival, &fval,
                        nval, sizeof nval, &over);
            if (!p || p == before) return 0;
            if (over) return 0;                            /* a cut string/number token: refused */
            /* the member is dispatched BEFORE the separator is looked at: the last member of an
               object is followed by '}', so a break-then-dispatch order would silently drop it */
            if (prm_seq(name, "output") && !has_output) {
                if (kind != 5 || !conn_text_ok(sval, 1)) return 0;
                prm_copy(c->output, sizeof c->output, sval);
                has_output = 1;
            } else if (prm_seq(name, "target") && !has_target) {
                if (kind != 5 || !conn_text_ok(sval, 1)) return 0;
                prm_copy(c->target, sizeof c->target, sval);
                has_target = 1;
            } else if (prm_seq(name, "input") && !has_input) {
                if (kind != 5 || !conn_text_ok(sval, 1)) return 0;
                prm_copy(c->input, sizeof c->input, sval);
                has_input = 1;
            } else if (prm_seq(name, "param")) {
                if (kind != 5 || !conn_text_ok(sval, 0)) return 0;
                prm_copy(c->param, sizeof c->param, sval);
            } else if (prm_seq(name, "delay")) {
                if (kind == 2) {                            /* an integer token: 0..3600 s */
                    if (ival < 0 || ival > (long long)CONN_DELAY_MAX) return 0;
                    c->delay = (float)ival;
                } else if (kind == 3) {                     /* a decimal token (also the exponent form) */
                    if (!(fval >= 0.0 && fval <= (double)CONN_DELAY_MAX)) return 0;
                    c->delay = (float)fval;
                } else return 0;
            } else if (prm_seq(name, "timesToFire")) {
                /* -1 = always, or a positive count; 0 is not a legal firing count */
                if (kind != 2 || ival < -1 || ival == 0 || ival > JSON_INT_MAX) return 0;
                c->timesToFire = (int)ival;
            } else return 0;                               /* unknown member */
            while (*p == ' ' || *p == '\n' || *p == '\t' || *p == '\r') p++;
            if (*p == ',') continue;                       /* the next member (kv_next skips it too) */
            if (*p == '}') { p++; break; }                 /* the entry ends */
            return 0;                                      /* malformed object */
        }
        if (!has_output || !has_target || !has_input) return 0;
        (*count)++;
    }
}

/* entity.removeConnections: exactly one of `indices` / `match` (both sides are resolved to index
   sets against the same live vector, so a request carrying both is ambiguous, not merged) */
static int rm_mode_ok(int has_indices, int has_match) {
    return (has_indices != 0) != (has_match != 0);
}
/* parse entity.removeConnections' `indices`: 1..max distinct non-negative ints, each within the
   i32 range (an index is cast to int; a larger lexeme would alias another entry - the nodeID
   rule). Duplicates are refused here instead of silently collapsing (the caller's set must be
   the set that gets removed and reported). */
static int rm_indices_parse(const char *p, int *out, int max, int *count) {
    *count = 0;
    if (!p || *p != '[') return 0;
    p++;
    for (;;) {
        long long v = 0;
        while (*p == ' ' || *p == '\n' || *p == '\t' || *p == '\r') p++;
        if (*p == ']') { p++; return *count > 0; }
        if (*count >= max || !json_int_at(p, &v) || v < 0 || v > JSON_INT_MAX) return 0;
        for (int i = 0; i < *count; i++) if (out[i] == (int)v) return 0;   /* duplicate */
        out[(*count)++] = (int)v;
        while (*p == '-' || (*p >= '0' && *p <= '9')) p++;                 /* past the token */
        while (*p == ' ' || *p == '\n' || *p == '\t' || *p == '\r') p++;
        if (*p == ',') { p++; continue; }
        if (*p != ']') return 0;
    }
}
/* descending order: RemoveConnection shifts the entries after the removed one, so a set of
   indices is only valid when it is walked from the highest down (re-props.md Q3) */
static void rm_sort_desc(int *v, int n) {
    for (int i = 1; i < n; i++) {
        int key = v[i], k = i - 1;
        while (k >= 0 && v[k] < key) { v[k + 1] = v[k]; k--; }
        v[k + 1] = key;
    }
}

/* the node.setTransform origin refusal, built from the gate itself (review W2 #5): the text used
   to say "<= 65536" after the gate had moved to 1048576, so a client that "corrected" its value
   from the message stayed wrong. Decimal only (the region has no hex literal). */
static void xform_origin_reason(char *dst, u32 cap) {
    SB b;
    sb_init(&b, dst, cap);
    sb_add(&b, "origin: each |component| <= "); sb_addi(&b, XFORM_ORIGIN_MAX);
}
/* ---- entity JSON region end ---- */

/* ---------------- state ---------------- */
static char g_token[65];
static u16  g_port;
static volatile u64 g_lastTick;            /* slot 13 (main thread) */
static volatile u64 g_inflightStart;       /* exec_request start (main thread) */
static volatile int g_started;
static volatile int g_shuttingDown;
static HANDLE g_ipcThread, g_writerThread;
static SOCKET g_listener = INVALID_SOCKET;
static SOCKET g_authedClient = INVALID_SOCKET;   /* set only after hello */
static u32 g_clientGen;                    /* generation of the authenticated client */
static u32 g_nextGen = 1;
static u64 g_startTick;
static void *g_passthrough;                /* slot 11 (diagnostics only) */
static void *g_connectFactory;             /* app-system factory from Connect (interfaces by name) */
static CHammerApp *g_hammer;               /* ToolSystem2_001 from hammer.dll's CreateInterface */

/* ---------------- method table (single source: capabilities + dispatch) ---------------- */
#define NEED_TF2 1u
#define NEED_HAMMER 2u
#define NEED_SCENE 4u
#define NEED_ENGINE 8u
#define NEED_RSV 16u
#define NEED_RENDER (NEED_SCENE | NEED_ENGINE | NEED_RSV)
#define NEED_CLIENT 32u
#define NEED_INPUT (NEED_CLIENT | NEED_ENGINE)
#define NEED_SERVER 64u
#define NEED_TIER0 128u
#define NEED_SCHEMA 256u          /* schemasystem.dll (runtime field offsets) */
#define NEED_SOUND 512u           /* soundsystem.dll (ISoundOpSystem hook) */
#define NEED_MODELDOC 1024u       /* modeldoc_editor.dll (.vmdl document lifecycle, m2.11) */
#define NEED_PET 2048u            /* pet.dll (.vpcf document lifecycle + operator registry, m2.12) */

typedef struct {
    const char *name;
    u32 needs;          /* required modules (verified) */
    u8 impl;            /* 1 = implemented, 0 = documented but blocked */
    u8 hidden;          /* never advertised (control.hello) */
    const char *blockedOn;
} Method;

static const Method k_methods[] = {
    { "control.hello",        0,                   1, 1, 0 },
    { "control.ping",         0,                   1, 0, 0 },
    { "control.status",       0,                   1, 0, 0 },
    { "control.capabilities", 0,                   1, 0, 0 },
    { "control.version",      0,                   1, 0, 0 },
    { "control.cancel",       0,                   1, 0, 0 },
    { "control.shutdown",     0,                   1, 0, 0 },
    { "doc.active",           NEED_HAMMER,         1, 0, 0 },
    { "view.render",          NEED_HAMMER, 1, 0, 0 },   /* task 13: offscreen Hammer render -> PNG */
    { "world.render",         NEED_RENDER,         1, 0, 0 },
    { "input.set",            NEED_INPUT,          1, 0, 0 },   /* virtual buttons + view angles (inputhook-scratch.md) */
    { "input.clear",          NEED_INPUT,          1, 0, 0 },
    { "input.status",         NEED_INPUT,          1, 0, 0 },
    { "input.uninstall",      NEED_INPUT,          1, 0, 0 },
    { "test.checkpoint",      NEED_TIER0,          1, 0, 0 },   /* ITestScriptMgr::CheckPoint (vtest runner) */
    { "test.active",          NEED_TIER0,          1, 0, 0 },
    { "console.exec",         NEED_ENGINE,         1, 0, 0 },   /* IInputService cbuf; -testscript sessions only */
    { "game.tick",            NEED_ENGINE,         1, 0, 0 },
    { "game.saveRestore",     NEED_SERVER,         1, 0, 0 },   /* unlock save/load (AllowSaveRestore -> true) */
    { "game.local",           NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, 1, 0, 0 },   /* local player: view, origin, velocity, active weapon (mcp_bridge_game.inc) */
    { "game.weapons",         NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, 1, 0, 0 },   /* inventory: clips, reserve, def index per weapon */
    { "game.radar",           NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, 1, 0, 0 },   /* all controllers/pawns, bomb, hostages, spotted flags */
    { "game.rules",           NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, 1, 0, 0 },   /* C_CSGameRules via cs_gamerules: warmup, freeze, round state (schema-resolved, nullable) */
    { "game.entities",        NEED_CLIENT | NEED_ENGINE | NEED_SCHEMA, 1, 0, 0 },   /* identities filtered by class (designer name) and/or name (targetname), max 256 */
    { "game.selectWeapon",    NEED_ENGINE,         1, 0, 0 },   /* slotN / inventory commands on the cbuf; weapon/defIndex/handle -> CCSGOInput MakeWeaponSelection (needs client+schema) */
    { "game.schemaProbe",     NEED_SCHEMA,         1, 0, 0 },   /* diagnostics: which module names have a schema scope */
    { "sound.status",         NEED_SOUND,          1, 0, 0 },   /* installs the ISoundOpSystem wrap on first use */
    { "sound.subscribe",      NEED_SOUND,          1, 0, 0 },   /* alias of sound.status (installs the hook) */
    { "sound.drain",          NEED_SOUND,          1, 0, 0 },   /* ring buffer of started sound events */
    { "sound.uninstall",      NEED_SOUND,          1, 0, 0 },
    { "node.list",            NEED_HAMMER, 1, 0, 0 },   /* live tree walk: nodeID/class/origin */
    { "node.delete",          NEED_HAMMER, 1, 0, 0 },   /* CMapDoc::DeleteNode by nodeID (undoable) */
    { "mesh.create",          NEED_HAMMER, 1, 0, 0 },   /* polygon soup -> welded CMapMesh (undoable) */
    { "mesh.setGeometry",     NEED_HAMMER, 1, 0, 0 },   /* replace a mesh's polygons (undoable) */
    { "mesh.setMaterial",     NEED_HAMMER, 1, 0, 0 },   /* all faces -> material (undoable) */
    { "doc.list",             NEED_HAMMER, 0, 0, "bridge-protocol.md §7 doc.list (not implemented in M2.2)" },
    { "map.save",             NEED_HAMMER, 1, 0, 0 },
    { "map.reload",           NEED_HAMMER, 1, 0, 0 },
    { "map.open",             NEED_HAMMER, 1, 0, 0 },   /* OpenFile(addon-content-relative maps/x.vmap, flags 1), gated: path shape + host file + binary-v9 DMX header + session-leak (m2.16) */
    { "map.run",              NEED_HAMMER, 0, 0, "hammer-save-compile.md §B (phase-3 task 5)" },
    { "undo.undo",            NEED_HAMMER, 1, 0, 0 },
    { "entity.create",        NEED_HAMMER, 1, 0, 0 },
    { "entity.setKeyValues",  NEED_HAMMER, 1, 0, 0 },
    { "node.setTransform",    NEED_HAMMER, 1, 0, 0 },   /* m2.18: SetScales/SetAngles/SetOrigin (slots 19/18/17, re-props.md Q2) */
    { "entity.addConnections", NEED_HAMMER, 1, 0, 0 },  /* m2.18: CMapEntityIONode::AddConnection (re-props.md Q3) */
    { "entity.removeConnections", NEED_HAMMER, 1, 0, 0 }, /* m2.18: RemoveConnection in descending index order */
    { "entity.tieMeshes",     NEED_HAMMER, 1, 0, 0 },   /* m2.19: tie meshes into a new/existing brush entity, one group (re-triggers.md Q1) */
    { "entity.untie",         NEED_HAMMER, 1, 0, 0 },   /* m2.19: CMapDoc::MoveToWorld (opens its own undo group) */
    { "edit.batch",           NEED_HAMMER, 1, 0, 0 },
    { "file.open_asset",      NEED_HAMMER, 0, 0, "superseded by map.open (m2.16): the VToolMessage asset route is not an open/reload primitive (file-edit-reload-compile.md §B)" },
    { "job.status",           0,                   0, 0, "bridge-protocol.md §7 (phase-3 task 5)" },
    /* documented-but-blocked rows (protocol §7): -32011 NotImplemented + blockedOn */
    { "undo.redo",            NEED_HAMMER, 1, 0, 0 },   /* m2.17: GetRedoHistory(doc) -> its vtable slot 2 (the pair partner's apply-and-pop) */
    { "entity.get",           NEED_HAMMER, 1, 0, 0 },   /* live entity read by nodeID (m2.15) */
    { "entity.find",          NEED_HAMMER, 1, 0, 0 },   /* live entity query by class/targetname/within (m2.15) */
    { "entity.delete",        NEED_HAMMER, 0, 0, "superseded by node.delete (m2.5, by nodeID)" },
    { "selection.get",        NEED_HAMMER, 0, 0, "bridge-protocol.md §7 selection.* (not isolated)" },
    { "selection.set",        NEED_HAMMER, 0, 0, "bridge-protocol.md §7 selection.* (not isolated)" },
    { "mesh.setFaceRect",     NEED_HAMMER, 0, 0, "superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)" },
    { "mesh.createFaceFromPoints", NEED_HAMMER, 0, 0, "superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)" },
    { "mesh.moveVertices",    NEED_HAMMER, 0, 0, "superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)" },
    { "mesh.extrudeFaces",    NEED_HAMMER, 0, 0, "superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)" },
    { "mesh.deleteFaces",     NEED_HAMMER, 0, 0, "superseded by mesh.setGeometry (polygon route, m2.5; MCP tools do the op)" },
    { "mesh.get",             NEED_HAMMER, 0, 0, "reads come from the saved .vmap (MCP get_mesh); node.list enumerates live nodes" },
    { "material.assign",      NEED_HAMMER, 0, 0, "superseded by mesh.setMaterial (m2.5)" },
    { "modeldoc.active",      NEED_MODELDOC, 1, 0, 0 },   /* ModelDoc first pass (m2.11): active .vmdl document */
    { "modeldoc.open",        NEED_MODELDOC, 1, 0, 0 },   /* ModelDoc: OpenFile(addon-content-relative path, flags 1) */
    { "modeldoc.reload",      NEED_MODELDOC, 1, 0, 0 },   /* ModelDoc: ReloadDocument (no prompt; discards in-editor changes) */
    { "modeldoc.save",        NEED_MODELDOC, 1, 0, 0 },   /* ModelDoc: SaveAndNotify(path,path,0,0) (the FileSave call) */
    { "modeldoc.compile",     NEED_MODELDOC, 1, 0, 0 },   /* ModelDoc: CModelDocEditorSession::Compile(session, 0) */
    { "modeldoc.nodeTypes",   NEED_MODELDOC, 1, 0, 0 },   /* ModelDoc: the registered node-type descriptor table */
    { "modeldoc.render",      NEED_MODELDOC, 1, 0, 0 },   /* ModelDoc vision pass (m2.14): main viewport render-to-memory (CToolScene::Render) -> PNG */
    /* particle editor first pass (m2.12): .vpcf. pet.load is the only method that can run before
       pet.dll exists (it loads it); everything else pins pet.dll, pet.schema the schema system too. */
    { "pet.load",             NEED_TF2,            1, 0, 0 },   /* LoadToolSystem(idx from GetToolIndexByName("pet")) */
    { "pet.active",           NEED_PET,            1, 0, 0 },   /* active .vpcf document: path/arrays/classes */
    { "pet.open",             NEED_PET,            1, 0, 0 },   /* OpenFile(addon-content-relative path, flags 1), gated: vpcf_check on the resolved host file */
    { "pet.reload",           NEED_PET,            1, 0, 0 },   /* ReloadDocument, gated: vpcf_check on GetPath first (malformed input wedges the main thread) */
    { "pet.save",             NEED_PET,            1, 0, 0 },   /* Save(path, path, 0, 0) (the FileSave call) */
    { "pet.functionTypes",    NEED_PET,            1, 0, 0 },   /* particles.dll IParticleSystemMgr: the operator registry */
    { "pet.schema",           NEED_PET | NEED_SCHEMA, 1, 0, 0 }, /* particles.dll type scope: class fields/bases/metadata */
    { "pet.text",             NEED_PET,            1, 0, 0 },   /* the live document serialized (SaveToFile to a private path) + dirty/undo flags (m2.13) */
    { "pet.apply",            NEED_PET | NEED_TIER0, 1, 0, 0 }, /* replace the live tree from KV3 text as ONE pet undo step (gated: vpcf_check) */
    { "pet.undo",             NEED_PET,            1, 0, 0 },   /* CPetDoc::Undo (pet's own history) */
    { "pet.redo",             NEED_PET,            1, 0, 0 },   /* CPetDoc::Redo */
    { "pet.render",           NEED_PET,            1, 0, 0 },   /* preview viewport render-to-memory (CToolScene::Render) -> PNG */
    { "job.cancel",           0,                   0, 0, "bridge-protocol.md §7 job.cancel (reserved)" },
};
#define METHOD_COUNT (sizeof k_methods / sizeof k_methods[0])

static const Method *find_method(const char *name) {
    for (u32 i = 0; i < METHOD_COUNT; i++) if (seq(k_methods[i].name, name)) return &k_methods[i];
    return 0;
}

/* ---------------- module identity (generated pins, fail closed) ---------------- */
enum { MS_NOT_LOADED = 0, MS_VERIFYING = 1, MS_VERIFIED = 2, MS_MISMATCH = 3 };
static volatile long g_modState[PIN_MODULE_COUNT];
static void *g_modBase[PIN_MODULE_COUNT];
static volatile long g_capsRevision = 1;
static volatile long g_pollThrottle;

static int mod_ok(int m) { return g_modState[m] == MS_VERIFIED && g_modBase[m] != 0; }
static u32 verified_mask(void) {
    return (mod_ok(PIN_TF2) ? NEED_TF2 : 0u) | (mod_ok(PIN_HAMMER) ? NEED_HAMMER : 0u) |
           (mod_ok(PIN_SCENESYSTEM) ? NEED_SCENE : 0u) | (mod_ok(PIN_ENGINE2) ? NEED_ENGINE : 0u) |
           (mod_ok(PIN_RSV) ? NEED_RSV : 0u) | (mod_ok(PIN_CLIENT) ? NEED_CLIENT : 0u) |
           (mod_ok(PIN_SERVER) ? NEED_SERVER : 0u) | (mod_ok(PIN_TIER0) ? NEED_TIER0 : 0u) |
           (mod_ok(PIN_SCHEMA) ? NEED_SCHEMA : 0u) | (mod_ok(PIN_SOUND) ? NEED_SOUND : 0u) |
           (mod_ok(PIN_MODELDOC) ? NEED_MODELDOC : 0u) | (mod_ok(PIN_PET) ? NEED_PET : 0u);
}
static const char *mod_state_name(int m) {
    switch (g_modState[m]) {
        case MS_VERIFYING: return "verifying";
        case MS_VERIFIED:  return "verified";
        case MS_MISMATCH:  return "mismatch";
        default:           return "not_loaded";
    }
}
static void caps_bump(void) { il_inc(&g_capsRevision); }

static int memeq(const u8 *a, const u8 *b, u32 n) {
    for (u32 i = 0; i < n; i++) if (a[i] != b[i]) return 0;
    return 1;
}

/* is the address readable? (diagnostics must never fault; VirtualQuery is not
   reliable under Wine, ReadProcessMemory is) */
static int mem_ok(const void *p) {
    u8 tmp = 0;
    u64 got = 0;
    if (!ReadProcessMemory((void *)(long long)-1, p, &tmp, 1, &got)) return 0;
    return got == 1;
}

/* capability object: one builder for control.hello and control.capabilities */
static void build_caps(SB *b) {
    u32 mask = verified_mask();
    static const char *hex = "0123456789abcdef";
    sb_add(b, "{\"methods\":[");
    int first = 1;
    for (u32 i = 0; i < METHOD_COUNT; i++) {
        const Method *m = &k_methods[i];
        if (!m->impl || m->hidden) continue;
        if ((mask & m->needs) != m->needs) continue;
        if (!first) sb_add(b, ",");
        first = 0;
        sb_addjson(b, m->name);
    }
    sb_add(b, "],\"features\":{\"tcp\":true,\"pipe\":false,\"consoleCommand\":false},\"limits\":{\"maxLineBytes\":");
    sb_addnum(b, MAX_LINE);
    sb_add(b, "},\"modules\":{");
    for (u32 i = 0; i < PIN_MODULE_COUNT; i++) {
        if (i) sb_add(b, ",");
        sb_addjson(b, k_pinModules[i].name);
        sb_add(b, ":{\"state\":\"");
        sb_add(b, mod_state_name((int)i));
        sb_add(b, "\",\"sha256\":\"");
        for (u32 k = 0; k < 8; k++) {
            sb_putc(b, hex[k_pinModules[i].sha256[k] >> 4]);
            sb_putc(b, hex[k_pinModules[i].sha256[k] & 15]);
        }
        sb_add(b, "\"}");
    }
    sb_add(b, "},\"revision\":");
    sb_addnum(b, (u64)g_capsRevision);
    sb_add(b, "}");
}

/* mapped-image PE facts (never relocated fields only) */
typedef struct { u32 ts, sizeOfImage, checkSum; u8 guid[16]; u32 age; int haveRsds; } PeInfo;

/* PE32+ / CodeView file-format constants (c): image layout defined by winnt.h, not by the game or
   the SDK - named here instead of kept as bare literals in pe_info. */
#define PE_E_LFANEW_OFF        0x3C    /* DOS 'MZ' header: offset of the PE header */
#define PE_SIG                 0x00004550u  /* the PE header starts with "PE\0\0" */
#define PE_TIME_DATE_STAMP_OFF 8       /* COFF header: TimeDateStamp (Machine/NumberOfSections precede) */
#define PE_OPTIONAL_HDR_OFF    24      /* optional header = PE sig (4) + COFF header (20) */
#define PE32PLUS_MAGIC         0x20Bu  /* optional header Magic: PE32+ */
#define PE32PLUS_SIZEOFIMAGE   56      /* optional header: SizeOfImage */
#define PE32PLUS_CHECKSUM      64      /* optional header: CheckSum */
#define PE32PLUS_NRVA_OFF      0x6C    /* optional header: NumberOfRvaAndSizes */
#define PE32PLUS_DATADIR_OFF   0x70    /* optional header: data-directory array */
#define PE_DATADIR_DEBUG       6       /* IMAGE_DIRECTORY_ENTRY_DEBUG */
#define PE_DATADIR_STRIDE      8       /* each dir entry: VirtualAddress + Size */
#define PE_DBGDIR_BYTES        28      /* sizeof(IMAGE_DEBUG_DIRECTORY) */
#define PE_DBGDIR_TYPE_OFF     12      /* IMAGE_DEBUG_DIRECTORY::Type */
#define PE_DBGDIR_TYPE_CODEVIEW 2u     /* IMAGE_DEBUG_TYPE_CODEVIEW */
#define PE_DBGDIR_ADDRRAW_OFF  20      /* IMAGE_DEBUG_DIRECTORY::AddressOfRawData */
#define CV_RSDS_AGE_OFF        20      /* RSDS record: "RSDS" (4) + GUID (16) + Age */

static int pe_info(void *base, PeInfo *pi) {
    const u8 *b = (const u8 *)base;
    if (b[0] != 'M' || b[1] != 'Z') return 0;
    u32 pe = *(const u32 *)(b + PE_E_LFANEW_OFF);
    if (*(const u32 *)(b + pe) != PE_SIG) return 0;              /* "PE\0\0" */
    if (*(const u16 *)(b + pe + PE_OPTIONAL_HDR_OFF) != PE32PLUS_MAGIC) return 0;   /* PE32+ */
    pi->ts = *(const u32 *)(b + pe + PE_TIME_DATE_STAMP_OFF);
    const u8 *opt = b + pe + PE_OPTIONAL_HDR_OFF;
    pi->sizeOfImage = *(const u32 *)(opt + PE32PLUS_SIZEOFIMAGE);
    pi->checkSum = *(const u32 *)(opt + PE32PLUS_CHECKSUM);
    pi->haveRsds = 0;
    u32 nrv = *(const u32 *)(opt + PE32PLUS_NRVA_OFF);
    if (nrv > PE_DATADIR_DEBUG) {
        u32 dbgRva = *(const u32 *)(opt + PE32PLUS_DATADIR_OFF + PE_DATADIR_DEBUG * PE_DATADIR_STRIDE);
        u32 dbgSize = *(const u32 *)(opt + PE32PLUS_DATADIR_OFF + PE_DATADIR_DEBUG * PE_DATADIR_STRIDE + 4);
        for (u32 off = 0; off + PE_DBGDIR_BYTES <= dbgSize; off += PE_DBGDIR_BYTES) {
            const u8 *d = b + dbgRva + off;
            if (*(const u32 *)(d + PE_DBGDIR_TYPE_OFF) != PE_DBGDIR_TYPE_CODEVIEW) continue;   /* CodeView */
            const u8 *r = b + *(const u32 *)(d + PE_DBGDIR_ADDRRAW_OFF);   /* AddressOfRawData (RVA) */
            if (r[0] == 'R' && r[1] == 'S' && r[2] == 'D' && r[3] == 'S') {
                for (u32 i = 0; i < 16; i++) pi->guid[i] = r[4 + i];
                pi->age = *(const u32 *)(r + CV_RSDS_AGE_OFF);
                pi->haveRsds = 1;
                break;
            }
        }
    }
    return 1;
}

static int sha256_file(const char *path, u8 out[32]) {
    static const unsigned short sha256Id[7] = { 'S','H','A','2','5','6',0 };
    void *alg = 0, *h = 0;
    if (BCryptOpenAlgorithmProvider(&alg, sha256Id, 0, 0) != 0) return 0;
    if (BCryptCreateHash(alg, &h, 0, 0, 0, 0, 0) != 0) { BCryptCloseAlgorithmProvider(alg, 0); return 0; }
    HANDLE f = CreateFileA(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                           0, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, 0);
    int ok = 0;
    if (f && f != INVALID_HANDLE_VALUE) {
        u8 *buf = (u8 *)HeapAlloc(GetProcessHeap(), 0, 1u << 18);
        if (buf) {
            ok = 1;
            for (;;) {
                u32 got = 0;
                if (!ReadFile(f, buf, 1u << 18, &got, 0)) { ok = 0; break; }
                if (!got) break;
                if (BCryptHashData(h, buf, got, 0) != 0) { ok = 0; break; }
            }
            HeapFree(GetProcessHeap(), 0, buf);
        }
        CloseHandle(f);
    }
    if (ok && BCryptFinishHash(h, out, 32, 0) != 0) ok = 0;
    BCryptDestroyHash(h);
    BCryptCloseAlgorithmProvider(alg, 0);
    return ok;
}

/* verify one module: header + file hash + byte pins + vtable slot pins */
static int verify_one(int m) {
    const PinModule *pm = &k_pinModules[m];
    logline("verify: module ");
    logline(pm->name);
    void *h = GetModuleHandleA(pm->name);
    int ok = h != 0;
    PeInfo pi;
    if (!ok) logline("verify: module not loaded any more");
    if (ok && !pe_info(h, &pi)) { logline("verify: PE parse failed"); ok = 0; }
    if (ok && pi.ts != pm->timeStamp) { logline("verify: TimeDateStamp mismatch"); ok = 0; }
    if (ok && pi.sizeOfImage != pm->sizeOfImage) { logline("verify: SizeOfImage mismatch"); ok = 0; }
    if (ok && pi.checkSum != pm->checkSum) { logline("verify: CheckSum mismatch"); ok = 0; }
    if (ok && (!pi.haveRsds || !memeq(pi.guid, pm->rsdsGuid, 16) || pi.age != pm->rsdsAge)) {
        logline("verify: RSDS GUID/age mismatch"); ok = 0;
    }
    if (ok) {
        char path[300];
        u32 n = GetModuleFileNameA(h, path, sizeof path);
        u8 sha[32];
        if (!n || n >= sizeof path) { logline("verify: module file name failed"); ok = 0; }
        else if (!sha256_file(path, sha)) { logline("verify: SHA-256 of file failed"); ok = 0; }
        else if (!memeq(sha, pm->sha256, 32)) { logline("verify: file SHA-256 mismatch"); ok = 0; }
    }
    if (ok) {
        for (u32 i = 0; i < sizeof k_pinBytes / sizeof k_pinBytes[0]; i++) {
            const PinBytes *pb = &k_pinBytes[i];
            if (pb->mod != m) continue;
            if (!memeq((const u8 *)h + pb->rva, pb->bytes, pb->len)) {
                logline("verify: byte pin mismatch: ");
                logline(pb->what);
                ok = 0;
                break;
            }
        }
    }
    if (ok) {
        for (u32 i = 0; i < sizeof k_pinSlots / sizeof k_pinSlots[0]; i++) {
            const PinSlot *ps = &k_pinSlots[i];
            if (ps->mod != m) continue;
            void *target = ((void **)((u8 *)h + ps->vtRva))[ps->slot];
            if (target != (void *)((u8 *)h + ps->targetRva)) {
                logline("verify: vtable slot mismatch: ");
                logline(ps->what);
                ok = 0;
                break;
            }
        }
    }
    if (ok) {
        /* pin by address so the image cannot be swapped after verification */
        u32 firstRva = 0;
        for (u32 i = 0; i < sizeof k_pinBytes / sizeof k_pinBytes[0]; i++)
            if (k_pinBytes[i].mod == m) { firstRva = k_pinBytes[i].rva; break; }
        void *mod = 0;
        if (!firstRva || !GetModuleHandleExW(MODULE_PIN_FLAGS, (const void *)((u8 *)h + firstRva), &mod)) {
            logline("verify: GetModuleHandleEx(PIN) failed"); ok = 0;
        }
    }
    if (ok) {
        g_modBase[m] = h;
        logline("verify: OK (module pinned)");
        return 1;
    }
    logline("verify: FAIL -> fail closed (PrerequisiteFailed)");
    return 0;
}

/* one worker verifies every loaded, not-yet-checked module in turn */
static volatile long g_verifierRunning;
static u32 __stdcall verify_all_thread(void *unused) {
    (void)unused;
    /* toolframework2 first: doc.active only needs it and its hash is 3x smaller.
       Every pin must be listed exactly once - a missing entry leaves a 0 = PIN_HAMMER
       duplicate and the module is never verified. */
    static const int order[PIN_MODULE_COUNT] = { PIN_TF2, PIN_TIER0, PIN_SCHEMA, PIN_SCENESYSTEM, PIN_ENGINE2, PIN_RSV, PIN_HAMMER, PIN_CLIENT, PIN_SERVER, PIN_SOUND, PIN_MODELDOC, PIN_PET };
    for (int oi = 0; oi < PIN_MODULE_COUNT; oi++) {
        int m = order[oi];
        if (g_modState[m] != MS_NOT_LOADED) continue;
        if (!GetModuleHandleA(k_pinModules[m].name)) continue;
        il_xchg(&g_modState[m], MS_VERIFYING);
        int ok = verify_one(m);
        il_xchg(&g_modState[m], ok ? MS_VERIFIED : MS_MISMATCH);
        caps_bump();
    }
    il_xchg(&g_verifierRunning, 0);
    return 0;
}
/* start verification once a module is present (main thread, throttled) */
static void modules_poll(void) {
    if ((++g_pollThrottle & 31) != 0) return;
    int need = 0;
    for (int m = 0; m < PIN_MODULE_COUNT; m++) {
        if (g_modState[m] == MS_NOT_LOADED && GetModuleHandleA(k_pinModules[m].name)) { need = 1; break; }
    }
    if (!need) return;
    if (il_cmpxchg(&g_verifierRunning, 1, 0) != 0) return;
    if (!CreateThread(0, 0, verify_all_thread, 0, 0, 0)) {
        logline("verify: CreateThread failed");
        il_xchg(&g_verifierRunning, 0);
    }
}

/* request queue (IPC thread -> main-thread drain) */
typedef struct {
    char id[128];
    char method[64];
    char *params;          /* heap copy of the params object, may be 0 */
    u64 receivedAt;
    u64 deadlineMs;        /* 0 = none */
    u32 gen;
} Req;
static Req g_q[QCAP];
static int g_qHead, g_qTail, g_qCount;
__declspec(align(16)) static u64 g_qcs[8];
__declspec(align(16)) static u64 g_qcv[2];

/* outbound queue (any thread -> writer thread); each record owns heap memory */
typedef struct { char *line; u32 gen; } Out;
static Out g_out[OUT_CAP];
static int g_oHead, g_oTail, g_oCount;
__declspec(align(16)) static u64 g_ocs[8];
__declspec(align(16)) static u64 g_ocv[2];

static int g_qinit;
static void q_init(void) {
    if (g_qinit) return;
    g_qinit = 1;
    InitializeCriticalSection(g_qcs);
    InitializeCriticalSection(g_ocs);
}

/* ---- request queue ---- */
static int q_push(const Req *r) {
    EnterCriticalSection(g_qcs);
    if (g_qCount >= QCAP) { LeaveCriticalSection(g_qcs); return 0; }
    g_q[g_qTail] = *r;
    g_qTail = (g_qTail + 1) % QCAP;
    g_qCount++;
    WakeConditionVariable(g_qcv);
    LeaveCriticalSection(g_qcs);
    return 1;
}
static int q_pop(Req *r) {
    int ok = 0;
    EnterCriticalSection(g_qcs);
    if (g_qCount > 0) {
        *r = g_q[g_qHead];
        g_qHead = (g_qHead + 1) % QCAP;
        g_qCount--;
        ok = 1;
    }
    LeaveCriticalSection(g_qcs);
    return ok;
}
static void q_purge_gen(u32 gen) {          /* drop queued requests of a dead connection */
    EnterCriticalSection(g_qcs);
    int n = g_qCount;
    for (int i = 0; i < n; i++) {
        Req r;
        /* pop head, keep if not our generation */
        r = g_q[g_qHead];
        g_qHead = (g_qHead + 1) % QCAP;
        g_qCount--;
        if (r.gen == gen) {
            if (r.params) HeapFree(GetProcessHeap(), 0, r.params);
        } else {
            g_q[g_qTail] = r;
            g_qTail = (g_qTail + 1) % QCAP;
            g_qCount++;
        }
    }
    LeaveCriticalSection(g_qcs);
}
static int q_depth(void) { EnterCriticalSection(g_qcs); int n = g_qCount; LeaveCriticalSection(g_qcs); return n; }
static int q_remove_id(const char *id) {    /* control.cancel: remove a queued request by id */
    int found = 0;
    EnterCriticalSection(g_qcs);
    int n = g_qCount;
    for (int i = 0; i < n; i++) {
        Req r = g_q[g_qHead];
        g_qHead = (g_qHead + 1) % QCAP;
        g_qCount--;
        if (!found && seq(r.id, id)) {
            found = 1;
            if (r.params) HeapFree(GetProcessHeap(), 0, r.params);
        } else {
            g_q[g_qTail] = r;
            g_qTail = (g_qTail + 1) % QCAP;
            g_qCount++;
        }
    }
    LeaveCriticalSection(g_qcs);
    return found;
}
/* ---- outbound queue ---- */
static int out_space(void) { EnterCriticalSection(g_ocs); int ok = g_oCount < OUT_CAP; LeaveCriticalSection(g_ocs); return ok; }
static int out_push(const char *line, u32 gen) {
    u32 len = slen(line);
    char *mem = (char *)HeapAlloc(GetProcessHeap(), 0, len + 1);
    if (!mem) return 0;
    cpy(mem, line, len + 1);
    EnterCriticalSection(g_ocs);
    if (g_oCount >= OUT_CAP) { LeaveCriticalSection(g_ocs); HeapFree(GetProcessHeap(), 0, mem); return 0; }
    g_out[g_oTail].line = mem;
    g_out[g_oTail].gen = gen;
    g_oTail = (g_oTail + 1) % OUT_CAP;
    g_oCount++;
    WakeConditionVariable(g_ocv);
    LeaveCriticalSection(g_ocs);
    return 1;
}
static int out_pop_blocking(Out *o, u32 timeoutMs) {
    EnterCriticalSection(g_ocs);
    while (g_oCount == 0 && !g_shuttingDown)
        SleepConditionVariableCS(g_ocv, g_ocs, timeoutMs);
    int ok = 0;
    if (g_oCount > 0) {
        *o = g_out[g_oHead];
        g_oHead = (g_oHead + 1) % OUT_CAP;
        g_oCount--;
        ok = 1;
    }
    LeaveCriticalSection(g_ocs);
    return ok;
}

/* ---- single writer: all output goes through here, generation-checked ---- */
static int send_all(SOCKET s, const char *buf, u32 len) {
    u32 off = 0;
    while (off < len) {
        int n = send(s, buf + off, (int)(len - off), 0);
        if (n <= 0) return 0;
        off += (u32)n;
    }
    return 1;
}
static u32 __stdcall writer_thread(void *param) {
    (void)param;
    logline("writer: thread start");
    for (;;) {
        Out o;
        if (!out_pop_blocking(&o, 100)) {
            if (g_shuttingDown) break;
            continue;
        }
        /* deliver only to the connection the record was produced for */
        if (g_authedClient != INVALID_SOCKET && o.gen == g_clientGen) {
            if (!send_all(g_authedClient, o.line, slen(o.line)))
                logline("writer: send failed");
        } else {
            lognum("writer: dropped record for stale generation ", o.gen);
        }
        HeapFree(GetProcessHeap(), 0, o.line);
    }
    logline("writer: thread exit");
    return 0;
}

/* ---------------- error responses ---------------- */
static void build_error(SB *b, int hasId, const char *id, int code, const char *msg) {
    sb_add(b, "{\"jsonrpc\":\"2.0\",\"id\":");
    if (hasId) sb_addjson(b, id); else sb_add(b, "null");
    sb_add(b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"error\":{\"code\":");
    sb_addi(b, code);
    sb_add(b, ",\"message\":");
    sb_addjson(b, msg);
    sb_add(b, "}}\n");
}
static void build_error_ext(SB *b, int hasId, const char *id, int code, const char *msg, const char *dataJson) {
    sb_add(b, "{\"jsonrpc\":\"2.0\",\"id\":");
    if (hasId) sb_addjson(b, id); else sb_add(b, "null");
    sb_add(b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"error\":{\"code\":");
    sb_addi(b, code);
    sb_add(b, ",\"message\":");
    sb_addjson(b, msg);
    if (dataJson) { sb_add(b, ",\"data\":"); sb_add(b, dataJson); }
    sb_add(b, "}}\n");
}

/* M2.2 (d): expire queued requests whose deadline passed, so a client gets a
   definite QueueTimeout while Hammer is stuck in a modal and slot 13 is not
   called. A request that already started executing is NOT swept (its outcome is
   unknown and the drain always sends its real result). */
static void q_sweep_expired(void) {
    for (;;) {
        Req r; int take = 0;
        EnterCriticalSection(g_qcs);
        if (g_qCount > 0) {
            Req *h = &g_q[g_qHead];
            if (h->deadlineMs && GetTickCount64() > h->receivedAt + h->deadlineMs) {
                r = *h;
                g_qHead = (g_qHead + 1) % QCAP;
                g_qCount--;
                take = 1;
            }
        }
        LeaveCriticalSection(g_qcs);
        if (!take) break;
        static char out[MAX_LINE];
        SB b; sb_init(&b, out, sizeof out);
        build_error(&b, 1, r.id, -32008, "QueueTimeout");
        out_push(out, r.gen);
        if (r.params) HeapFree(GetProcessHeap(), 0, r.params);
    }
}

/* ---------------- interfaces, exports and bound function tables (main thread) ----------------
   s2sdk.h describes every object the bridge touches. Objects are reached by interface name
   (Connect factory / a module's exported CreateInterface) or exported symbol wherever one exists;
   the remaining non-virtual internals are bound once from pinned RVAs. Every object is still
   identity-checked against its pinned vtable, and its pinned slots are re-checked in memory the
   first time it is resolved (vtable calls must not follow a patched slot). */
static HammerFns g_hfn;
static SceneFns g_sfn;
static EngineFns g_efn;
static int g_hfnBound, g_sfnBound, g_efnBound;

/* every pinned slot of this vtable still holds its pinned target */
static int vt_slots_ok(int m, u32 vtRva) {
    u8 *base = (u8 *)g_modBase[m];
    for (u32 i = 0; i < sizeof k_pinSlots / sizeof k_pinSlots[0]; i++) {
        const PinSlot *ps = &k_pinSlots[i];
        if (ps->mod != m || ps->vtRva != vtRva) continue;
        if (((void **)(base + vtRva))[ps->slot] != (void *)(base + ps->targetRva)) {
            logline("iface: vtable slot changed since verification: ");
            logline(ps->what);
            return 0;
        }
    }
    return 1;
}
/* obj is a live object of the pinned class (identity + slot contents) */
static int obj_is(void *obj, int m, u32 vtRva) {
    if (!obj || !mem_ok(obj)) return 0;
    if (*(void **)obj != (void *)((u8 *)g_modBase[m] + vtRva)) return 0;
    return vt_slots_ok(m, vtRva);
}

static void hammer_bind(void) {
    if (g_hfnBound || !mod_ok(PIN_HAMMER)) return;
    u8 *hb = (u8 *)g_modBase[PIN_HAMMER];
#define BIND(f, rva) *(void **)&g_hfn.f = (void *)(hb + (rva))
    BIND(GetUndoHistory, HFN_GET_UNDO_HISTORY);
    BIND(GetRedoHistory, HFN_GET_REDO_HISTORY);
    BIND(GetActiveHistory, HFN_GET_ACTIVE_HISTORY);
    BIND(AddOperation, HFN_ADD_OPERATION);
    BIND(RecordNodeTree, HFN_RECORD_NODE_TREE);
    BIND(RecordNode, HFN_RECORD_NODE);
    BIND(RecordNodesAndParents, HFN_RECORD_NODES_AND_PARENTS);
    BIND(DocMoveToWorld, HFN_DOC_MOVE_TO_WORLD);
    BIND(ActivateDoc, HFN_ACTIVATE_DOC);
    BIND(DocOrDependencyChanged, HFN_DOC_OR_DEPENDENCY_CHANGED);
    BIND(ChangedSinceSave, HFN_CHANGED_SINCE_SAVE);
    BIND(DocSetModified, HFN_DOC_SET_MODIFIED);
    BIND(FindTypeByName, HFN_FIND_TYPE_BY_NAME);
    BIND(CreateNodeByName, HFN_CREATE_NODE_BY_NAME);
    BIND(InsertNode, HFN_INSERT_NODE);
    BIND(DeleteNode, HFN_DELETE_NODE);
    BIND(NodeIsLocked, HFN_NODE_IS_LOCKED);
    BIND(GetEntityProperties, HFN_GET_ENTITY_PROPERTIES);
    BIND(FindAttribute, HFN_FIND_ATTRIBUTE);
    BIND(CreateAttribute, HFN_CREATE_ATTRIBUTE);
    BIND(SetValueString, HFN_SET_STRING);
    BIND(SetValueInt, HFN_SET_INT);
    BIND(SetValueFloat, HFN_SET_FLOAT);
    BIND(SetKeyValue, HFN_SET_KEY_VALUE);
    BIND(AddConnection, HFN_ADD_CONNECTION);
    BIND(RemoveConnection, HFN_REMOVE_CONNECTION);
    BIND(Kv3Init, HFN_KV3_INIT);
    BIND(Kv3Free, HFN_KV3_FREE);
    BIND(CreateMeshElement, HFN_CREATE_MESH_ELEMENT);
    BIND(MeshInitFromPolygons, HFN_MESH_INIT_FROM_POLYGONS);
    BIND(AssignMaterialToFaces, HFN_ASSIGN_MATERIAL_TO_FACES);
    BIND(CollectHandles, HFN_COLLECT_HANDLES);
    BIND(MeshDeleteFaces, HFN_MESH_DELETE_FACES);
    BIND(GetPreferred3DView, HFN_GET_PREFERRED_3D_VIEW);
    BIND(SessionUpdateViews, HFN_SESSION_UPDATE_VIEWS);
    BIND(RenderViewToMemory, HFN_RENDER_VIEW_TO_MEMORY);
    BIND(FrustumInit, HFN_FRUSTUM_INIT);
    BIND(FrustumUpdate, HFN_FRUSTUM_UPDATE);
    BIND(CameraGetPosition, HFN_CAMERA_GET_POSITION);
    BIND(CameraGetAngles, HFN_CAMERA_GET_ANGLES);
    BIND(ToggleToolsMaterials, HFN_TOGGLE_TOOLS_MATERIALS);
#undef BIND
    g_hfnBound = 1;
}

/* CHammerApp via hammer.dll's exported CreateInterface("ToolSystem2_001") - the same lazy
   singleton toolframework2 holds; identity-checked against the pinned static object */
static CHammerApp *resolve_hammer(void) {
    if (g_hammer) return g_hammer;
    if (!mod_ok(PIN_HAMMER)) { logline("doc: hammer not verified"); return 0; }
    u8 *hb = (u8 *)g_modBase[PIN_HAMMER];
    void *hm = GetModuleHandleA(k_pinModules[PIN_HAMMER].name);
    if (hm != (void *)hb) { logline("doc: hammer module handle changed"); return 0; }
    S2CreateInterfaceFn ci = (S2CreateInterfaceFn)GetProcAddress(hm, S2_SYM_CREATEINTERFACE);
    if (!ci) { logline("doc: hammer CreateInterface export missing"); return 0; }
    int ret = -1;
    CHammerApp *app = (CHammerApp *)ci(S2_TOOLSYSTEM2_VERSION, &ret);
    if (!app || ret != 0) { lognum("doc: ToolSystem2_001 unresolved, ret=", (u64)(long long)ret); return 0; }
    if ((void *)app != (void *)(hb + HG_HAMMERAPP_OBJ)) { logline("doc: hammer app identity mismatch"); return 0; }
    if (!obj_is(app, PIN_HAMMER, HV_HAMMERAPP_VT)) { logline("doc: hammer app vtable mismatch"); return 0; }
    hammer_bind();
    g_hammer = app;
    logline("doc: hammer app resolved (ToolSystem2_001 export)");
    return app;
}

/* Document chain with per-hop validation. reason = short diagnostic string. */
static CMapDoc *active_doc(const char **reason) {
    *reason = "ok";
    if (!mod_ok(PIN_HAMMER)) {
        *reason = (g_modState[PIN_HAMMER] == MS_MISMATCH) ? "hammer_mismatch"
                : (g_modState[PIN_HAMMER] == MS_VERIFYING) ? "hammer_verifying" : "hammer_not_loaded";
        return 0;
    }
    CHammerApp *app = resolve_hammer();
    if (!app) { *reason = "hammer_unavailable"; return 0; }
    CHammerEditorSession *session = CHammerApp_ActiveSession(app);
    if (!session) { *reason = "no_session"; return 0; }
    if (!obj_is(session, PIN_HAMMER, HV_SESSION_VT)) { *reason = "bad_session"; return 0; }
    CMapDocSession *docSession = session->m_pDocSession;
    if (!docSession) { *reason = "no_document"; return 0; }
    if (!obj_is(docSession, PIN_HAMMER, HV_DOCSESSION_VT)) { *reason = "bad_doc_session"; return 0; }
    if (CMapDocSession_Owner(docSession) != session) { *reason = "doc_session_backptr"; return 0; }
    CMapDoc *doc = CMapDocSession_Doc(docSession);          /* embedded: the address is the document */
    if (!obj_is(doc, PIN_HAMMER, HV_DOC_VT)) { *reason = "bad_doc"; return 0; }
    if (doc->m_pDocSession != docSession) { *reason = "doc_backptr"; return 0; }
    return doc;
}

/* ---------------- document fingerprint + fields (M2.2 cycle B) ---------------- */
/* Sampled on every slot-13 tick (main thread): a cheap structural fingerprint so
   doc.id/epoch change whenever the document identity changes - including a
   close/open that reuses the same heap address (ABA). Pointers alone are not
   enough: the root's DME element handle is an integer id, and the path
   CUtlString pointer changes when the file changes. */
typedef struct {
    void *docSession;
    void *root;
    void *pathPtr;
    u32   rootHandle;
    u32   docSeq;      /* monotonic document id */
    u32   epoch;       /* monotonic: any structural change */
} DocFp;
static DocFp g_fp;

static void doc_fingerprint(void) {
    void *ds = 0, *root = 0, *pp = 0;
    u32 rh = 0;
    if (mod_ok(PIN_HAMMER) && g_hammer) {
        CHammerEditorSession *session = CHammerApp_ActiveSession(g_hammer);
        if (session) {
            CMapDocSession *d = session->m_pDocSession;
            if (d) {
                CMapRootElement *r = CMapDocSession_Doc(d)->m_pRoot;
                ds = d;
                root = r;
                pp = CMapDocSession_Path(d);
                if (r) rh = r->m_hWorld;
            }
        }
    }
    if (ds != g_fp.docSession || root != g_fp.root || pp != g_fp.pathPtr || rh != g_fp.rootHandle) {
        if (ds != g_fp.docSession) g_fp.docSeq++;
        g_fp.docSession = ds;
        g_fp.root = root;
        g_fp.pathPtr = pp;
        g_fp.rootHandle = rh;
        g_fp.epoch++;
        lognum("doc: fingerprint change, epoch ", g_fp.epoch);
    }
}

/* full edit precondition set (phase-3 task 0b). Mutating methods must call this
   before their first mutation; doc.active only reports it. */
static int doc_editable(CMapDoc *doc, const char **why) {
    u8 *hb = (u8 *)g_modBase[PIN_HAMMER];
    if (!doc->m_pRoot) { *why = "no_root"; return 0; }
    if (!obj_is(doc->m_pRoot, PIN_HAMMER, HV_ROOT_VT)) { *why = "bad_root"; return 0; }
    if (!doc->m_bActive) { *why = "inactive"; return 0; }
    if (!doc->m_pSelection) { *why = "no_selection"; return 0; }
    if (!obj_is(doc->m_pSelection, PIN_HAMMER, HV_SELECTION_VT)) { *why = "bad_selection"; return 0; }
    if (CMapDoc_ScopeDepth(doc) != 0) {
        *why = "edit_scope_active";   /* prefab scope: the active history is the prefab's */
        return 0;
    }
    CMapDocHistory *hist = g_hfn.GetUndoHistory(doc);
    if (!hist) { *why = "no_history"; return 0; }
    if (!obj_is(hist, PIN_HAMMER, HV_HISTORY_VT)) { *why = "bad_history"; return 0; }
    CMapDocHistory *active = *(CMapDocHistory **)(hb + HG_ACTIVE_HISTORY);
    if (active != hist) { *why = "history_not_active"; return 0; }
    *why = "ok";
    return 1;
}

/* activate a loaded document (the UI path is CHammerApp::OnActiveEditorSessionChanged
   -> sub_180137370 -> this primitive; open_asset alone leaves the active history
   global null, which blocks every edit). Only called by mutating methods. */
static int __attribute__((unused)) doc_activate(CMapDoc *doc) {
    g_hfn.ActivateDoc(doc, 1);
    const char *why = "?";
    return doc_editable(doc, &why);
}

/* top undo group name + serial of ONE history: `depth` is the history's own group count, `serial`
   the serial of its top group (0 with no group). Named generically since m2.17 reads the redo
   history's pair through it as well (undo.redo reports the group it redid). */
static void history_top_info(CMapDocHistory *hist, char *name, u32 namesz, u32 *depth, u32 *serial) {
    name[0] = 0; *depth = 0; *serial = 0;
    if (!hist) return;
    u32 n = CMapDocHistory_Count(hist);
    *depth = n;
    if (!n) return;
    CMapUndoGroup *entry = CMapDocHistory_Groups(hist)[n - 1];
    if (!entry) return;
    *serial = entry->m_nSerial;
    u32 i = 0;
    while (i + 1 < namesz && i < sizeof entry->m_szName && entry->m_szName[i]) { name[i] = entry->m_szName[i]; i++; }
    name[i] = 0;
}

/* top undo group name + serial of the document's undo stack */
static void undo_group_info(CMapDoc *doc, char *name, u32 namesz, u32 *depth, u32 *serial) {
    history_top_info(g_hfn.GetUndoHistory(doc), name, namesz, depth, serial);
}

/* open a named undo group on the active history (every mutating method) */
static CMapDocHistory *undo_open(CMapDoc *doc, const char *name) {
    CMapDocHistory *hist = g_hfn.GetActiveHistory();
    g_hfn.AddOperation(hist, (CSelection *)doc->m_pSelection, name, 0);
    return hist;
}

/* ---------------- m2.17: the document state a write's own reply carries ----------------
   Every undoable write appends `,"doc":{id, topSerial, undoDepth}` to its result, read HERE, on
   the tools main thread, inside the same dispatch as the write. The three numbers are the ones
   `doc.active` reports, from the same source (the fingerprint's document id + the undo stack's top
   group), and reading them here is what closes review-5 L2: a follow-up `doc.active` request is a
   *later* drain iteration, so a user Ctrl+Z and a user edit could land between the write and that
   read and make a two-action sequence look like the write's own single one; nothing can land
   between the write and this read.

   hammer-mcp prefers this state over its own post-write `doc.active` read (and falls back to that
   read for an older bridge: the field is simply absent). A write whose op failed or that ran
   outside a document carries no `doc`. */
static void sb_add_doc_object(SB *b, CMapDoc *doc) {
    doc_fingerprint();                 /* the id must be the document's own, current one */
    char undoname[128];
    u32 depth = 0, serial = 0;
    undo_group_info(doc, undoname, sizeof undoname, &depth, &serial);
    sb_add(b, "{\"id\":\"d"); sb_addnum(b, g_fp.docSeq);
    sb_add(b, "\",\"topSerial\":"); sb_addnum(b, serial);
    sb_add(b, ",\"undoDepth\":"); sb_addnum(b, depth);
    sb_add(b, "}");
}

/* Append `,"doc":{...}` to a result object an op just finished writing: `out` must hold exactly
   one complete JSON object (the last byte a '}'), which is what every mutating op emits. Anything
   else is left untouched and logged - the reply then carries no `doc` and the caller reads the
   document state itself, which is the older bridge's behaviour, so a shape surprise can never turn
   into a wrong tail. */
static void sb_add_doc_tail(SB *out, CMapDoc *doc) {
    if (!doc || out->len == 0 || out->buf[out->len - 1] != '}') {
        logline("doc tail: the result is not a complete object; no doc state added");
        return;
    }
    out->len--;
    out->buf[out->len] = 0;
    sb_add(out, ",\"doc\":");
    sb_add_doc_object(out, doc);
    sb_add(out, "}");
}

/* ---------------- node handles (v1: epoch-scoped tokens) ----------------
   The protocol rule stays: a handle is only usable inside the same epoch and,
   for safety, callers must not store it across requests. There is still no
   liveness validation (Cluster A adds the doc+0x160 membership + nodeID check). */
#define HANDLE_CAP 32
typedef struct { u32 used; u32 docSeq; u32 epoch; void *node; char token[16]; } Handle;
static Handle g_handles[HANDLE_CAP];
static u32 g_handleNext = 1;

static const char *handle_new(void *node) {
    Handle *h = 0;
    for (u32 i = 0; i < HANDLE_CAP; i++) if (!g_handles[i].used) { h = &g_handles[i]; break; }
    if (!h) { h = &g_handles[g_handleNext % HANDLE_CAP]; }
    h->used = 1;
    h->docSeq = g_fp.docSeq;
    h->epoch = g_fp.epoch;
    h->node = node;
    h->token[0] = 'n';
    u32 v = g_handleNext++;
    if (!v) v = 1;
    char tmp[12]; u32 n = 0;
    while (v) { tmp[n++] = (char)('0' + v % 10); v /= 10; }
    u32 i = 1;
    while (n && i + 1 < sizeof h->token) h->token[i++] = tmp[--n];
    h->token[i] = 0;
    return h->token;
}

static void __attribute__((unused)) *handle_lookup(const char *tok, const char **why) {
    for (u32 i = 0; i < HANDLE_CAP; i++) {
        if (!g_handles[i].used || !seq(g_handles[i].token, tok)) continue;
        if (g_handles[i].docSeq != g_fp.docSeq || g_handles[i].epoch != g_fp.epoch) {
            *why = "epoch_changed";
            return 0;
        }
        return g_handles[i].node;
    }
    *why = "unknown_handle";
    return 0;
}

/* expectDoc {id:"dN", epoch} must match the current fingerprint */
static int expect_doc_ok(const char *params, const char **why) {
    *why = "ok";
    if (!params) { *why = "missing"; return 0; }
    const char *ed = json_find_top(params, "expectDoc");    /* top-level only (review opus #1) */
    if (!ed || *ed != '{') { *why = "missing"; return 0; }
    char idbuf[32];
    idbuf[0] = 0;
    long long ep = 0;
    if (!json_string(ed, "id", idbuf, sizeof idbuf)) { *why = "missing_id"; return 0; }
    if (!json_int(ed, "epoch", &ep)) { *why = "missing_epoch"; return 0; }
    u32 want = 0;
    for (const char *q = idbuf; *q; q++) if (*q >= '0' && *q <= '9') want = want * 10 + (u32)(*q - '0');
    if (want != g_fp.docSeq) { *why = "doc_id"; return 0; }
    if ((long long)g_fp.epoch != ep) { *why = "epoch"; return 0; }
    return 1;
}

/* hammer's CDataModel (statically linked; no factory interface): pinned global, virtual calls */
static IDataModel *data_model(void) {
    IDataModel *dm = *(IDataModel **)((u8 *)g_modBase[PIN_HAMMER] + HG_DATAMODEL);
    return obj_is(dm, PIN_HAMMER, HV_DATAMODEL_VT) ? dm : 0;
}
static S2_CUtlSymbolLarge dm_intern(const char *name) {
    IDataModel *dm = data_model();
    S2_CUtlSymbolLarge sym = { 0 };
    if (dm) dm->vt->GetSymbol(dm, &sym, name);
    return sym;
}

/* copy a game-owned C string with mem_ok guards and a page-boundary probe (strings can point at
   freed or reallocated storage). Returns 1 when the whole string fit, -1 when it was cut at
   cap-1 (out holds the prefix), 0 (empty out) when null or unreadable. A cut never ends inside a
   multi-byte UTF-8 sequence: a half sequence would make serde_json reject the whole frame
   (review opus #10).
   PROBE_PAGE_MASK is 4 KB - 1, so the readability probe re-runs at every page boundary (the
   existing md_str does the same; the mask stays decimal so the m2.15 diff adds no hex literal). */
#define PROBE_PAGE_MASK 4095u
static int probe_str(const char *s, char *out, u32 cap) {
    out[0] = 0;
    if (!s || !mem_ok(s) || cap < 2) return 0;
    u32 i = 0;
    for (; i + 1 < cap; i++) {
        if (i && (((u64)(size_t)(s + i)) & PROBE_PAGE_MASK) == 0 && !mem_ok(s + i)) { out[0] = 0; return 0; }
        out[i] = s[i];
        if (!out[i]) return 1;
    }
    /* the copy filled cap-1 bytes without a NUL: the next byte decides whether the string is
       exactly cap-1 bytes (it fits, and out[cap-1] terminates it) or longer (a cut) */
    if (mem_ok(s + cap - 1) && s[cap - 1] == 0) { out[cap - 1] = 0; return 1; }
    u32 n = utf8_prefix(out, cap - 1);   /* never cut inside a sequence (review opus #10) */
    out[n] = 0;
    return -1;
}

/* the char* a type-5 attribute's storage holds (a CUtlSymbolLarge: exactly what SetValueString /
   dm_intern store), 0 when the attribute or its storage is not readable */
static const char *attr_cstr(CDmAttribute *a) {
    if (!a || !a->value) return 0;
    const u8 *vp = (const u8 *)a->value;
    if (!mem_ok(vp) || !mem_ok(vp + 7)) return 0;
    const char *s = *(const char **)vp;
    if (!s) return "";                 /* an empty CUtlSymbolLarge is a null pointer (live: targetname "") */
    return mem_ok(s) ? s : 0;
}

/* ---------------- op implementations (shared by standalone methods and edit.batch) ---------------- */
/* data grew from 192 to 512 (FIX-B review-b M6): the AddChild-refusal data names the mesh plus
   the progress the group already made (tied ids, removedEmptyEntities) - see the builder there */
typedef struct { int code; const char *name; char data[512]; } OpErr;

static void op_err(OpErr *e, int code, const char *name, const char *reason) {
    e->code = code;
    e->name = name;
    SB db;
    sb_init(&db, e->data, sizeof e->data);
    sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason ? reason : "?"); sb_add(&db, "}");
}
/* op_err plus `"maybeExecuted":true`: for a failure that happened AFTER the undo group was opened
   and the earlier writes in it ran (m2.18's entity.removeConnections). A client must verify with a
   read instead of retrying - the same "maybe-executed" contract -32012 PartialFailure documents. */
static void op_err_maybe(OpErr *e, int code, const char *name, const char *reason) {
    e->code = code;
    e->name = name;
    SB db;
    sb_init(&db, e->data, sizeof e->data);
    sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason ? reason : "?");
    sb_add(&db, ",\"maybeExecuted\":true}");
}

/* the `warnings` entry of a refused case variant: it names the attribute that IS stored, so the
   caller can re-send the key with the entity's own spelling (the fix). Built in a 384-byte buffer
   (a 95-byte key name + the existing spelling + the explanation); the entry is dropped when the
   warnings array already overflowed. */
static void kv_warn_variant(SB *wn, const char *name, const char *variant) {
    char vb[96], msg[384];
    SB w;
    vb[0] = 0;
    if (variant && mem_ok(variant)) probe_str(variant, vb, sizeof vb);   /* page-guarded, bounded */
    sb_init(&w, msg, sizeof msg);
    sb_add(&w, name); sb_add(&w, ": refused, the entity spells this key \"");
    sb_add(&w, vb); sb_add(&w, "\" (SetKeyValue matches case-insensitively, `previous` is exact)");
    if (!wn->ovf) { if (wn->len) sb_add(wn, ","); sb_addjson(wn, msg); }
}
/* the name of the entity's attribute that matches `name` case-insensitively but NOT exactly -
   the attribute SetKeyValue would write while FindAttribute (and so the `previous` pass) does not
   see it. 0 when there is none (m2.18, re-props.md Q1: SetKeyValue matches case-insensitively,
   FindAttribute is exact, so a case variant is refused instead of applied half-seen). */
static const char *kv_case_variant(CDmElement *child, const char *name) {
    CDmAttribute *a = child ? child->attrs : 0;
    for (int guard = 0; a && guard < 4096; guard++) {
        if (!mem_ok(a) || !mem_ok((u8 *)a + sizeof *a - 1)) return 0;
        if (a->name && mem_ok(a->name) && !seq(a->name, name) && ci_seq(a->name, name)) return a->name;
        a = a->next;
    }
    return 0;
}
static void apply_keyvalues(CMapNode *node, CDmElement *child, const char *vp, SB *out, int with_previous);
static CMapNode *geo_entity_node(CMapDoc *doc, long long id, OpErr *e);   /* live-tree section (m2.15) */

/* the validated inputs of an entity.create: the class name (resolved in the node-factory registry by
   entity_create_target, whose lookup is what actually validates it - CreateNodeByName re-resolves
   it), the origin/angles vectors and (m2.18) the optional scales. The whole validation (class
   present and non-empty, origin/angles well-formed, scales in (0, 1000], class in the registry)
   happens in entity_create_target, so the standalone arm can run it BEFORE it opens an undo group
   (review L3: a refused create must not leave an empty "MCP: create entity" group for undo_last to
   pop - the same gap opus #13 fixed for entity.setKeyValues). Only the node creation itself can
   fail after this. */
typedef struct { char cls[64]; float origin[3]; float angles[3]; float scales[3]; int has_scales; } CreateTarget;

static int entity_create_target(const char *params, CreateTarget *t, OpErr *e) {
    u8 *hb = (u8 *)g_modBase[PIN_HAMMER];
    t->cls[0] = 0;
    t->origin[0] = t->origin[1] = t->origin[2] = 0;
    t->angles[0] = t->angles[1] = t->angles[2] = 0;
    t->scales[0] = t->scales[1] = t->scales[2] = 1.0f;
    t->has_scales = 0;
    if (!params || !json_string_top(params, "class", t->cls, sizeof t->cls) || !t->cls[0]) {
        op_err(e, -32602, "InvalidParams", "class");
        return 0;
    }
    /* top-level only: `keyValues.origin` is a string and precedes the real array in the serialized
       params, so the first-match json_find would place the entity at [0,0,0] (review opus #1) */
    float origin[3] = {0, 0, 0}, angles[3] = {0, 0, 0}, scales[3] = {1, 1, 1};
    int has_origin = json_float3_top(params, "origin", origin);
    int has_angles = json_float3_top(params, "angles", angles);
    int has_scales = json_float3_top(params, "scales", scales);
    if (json_find_top(params, "origin") && !has_origin) {
        op_err(e, -32602, "InvalidParams", "origin: [x,y,z] required");
        return 0;
    }
    if (json_find_top(params, "angles") && !has_angles) {
        op_err(e, -32602, "InvalidParams", "angles: [pitch,yaw,roll] required");
        return 0;
    }
    /* m2.18 (re-props.md Q2): CreateNodeByName takes only origin/angles, so a create with scales
       calls the entity SetScales vslot right after it (entity_create_apply). Refused here, before
       the undo group and before anything is created: a 0/negative/NaN scale degenerates the
       transform and an unbounded one overflows the bounds math. */
    if (json_find_top(params, "scales") && !has_scales) {
        op_err(e, -32602, "InvalidParams", "scales: [x,y,z] required");
        return 0;
    }
    if (has_scales && !xform_scales_ok(scales)) {
        op_err(e, -32602, "InvalidParams", "scales: each finite and in (0, 1000]");
        return 0;
    }
    void *reg = *(void **)(hb + HG_NODE_FACTORY_REGISTRY);
    void *entry = reg ? g_hfn.FindTypeByName(reg, t->cls) : 0;
    if (!entry) {
        op_err(e, -32006, "UnknownClass", "not in the CMapNodeFactory registry");
        return 0;
    }
    for (int i = 0; i < 3; i++) { t->origin[i] = origin[i]; t->angles[i] = angles[i]; }
    if (has_scales) { for (int i = 0; i < 3; i++) t->scales[i] = scales[i]; t->has_scales = 1; }
    return 1;
}

/* defined with the live-tree helpers further down; the create reply needs it here */
static int node_id_of(CMapNode *node, int *out);

static int entity_create_apply(CMapDoc *doc, const char *params, CreateTarget *t, SB *out, OpErr *e) {
    CMapNode *node = g_hfn.CreateNodeByName(doc, t->cls, t->origin, t->angles);
    if (!node) {
        op_err(e, -32603, "InternalError", "CreateNodeByName failed (see bridge log)");
        return 0;
    }
    /* m2.18: the create takes only origin/angles, so the scale write is the entity SetScales
       vslot (slot 19), in this same group - the creation record already covers it, so no extra
       RecordNodeTree (re-props.md Q2). SetScales also moves an entity's attached children and
       invalidates the transform/bounds, exactly like Hammer's own setters. */
    if (t->has_scales) node->vt->SetScales(node, t->scales);
    const char *tok = handle_new(node);
    logline("entity: created ");
    logline(t->cls);
    sb_add(out, "{\"handle\":"); sb_addjson(out, tok);
    /* the created node's nodeID (what node.list/entity.get address it by, node_id_of): the create
       reply used to carry a handle only, so every create needed a follow-up entity.find to learn
       the id. A node whose nodeID attribute is not readable yet carries no field (older readers
       ignore it). */
    int new_id = -1;
    if (node_id_of(node, &new_id)) { sb_add(out, ",\"nodeID\":"); sb_addi(out, new_id); }
    sb_add(out, ",\"class\":"); sb_addjson(out, t->cls);
    sb_add(out, ",\"origin\":[");
    sb_addf(out, t->origin[0]); sb_add(out, ",");
    sb_addf(out, t->origin[1]); sb_add(out, ",");
    sb_addf(out, t->origin[2]);
    sb_add(out, "],\"scales\":[");
    sb_addf(out, t->scales[0]); sb_add(out, ",");
    sb_addf(out, t->scales[1]); sb_add(out, ",");
    sb_addf(out, t->scales[2]);
    sb_add(out, "],\"undoLabel\":\"MCP: create entity\"");
    const char *kv = params ? json_find_top(params, "keyValues") : 0;
    if (kv && *kv == '{') {
        CDmElement *child = g_hfn.GetEntityProperties(node);
        if (child) {
            sb_add(out, ",\"keyValues\":");
            apply_keyvalues(node, child, kv, out, 0);
        }
    }
    sb_add(out, "}");
    return 1;
}

static int op_entity_create(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    CreateTarget t;
    if (!entity_create_target(params, &t, e)) return 0;
    return entity_create_apply(doc, params, &t, out, e);
}

/* one `previous` entry (m2.15 §3.3): the attribute's value as JSON before any write, or null when
   the entity does not have the attribute (and when its value is not readable - the attribute
   pointer is mem_ok'd before any field is read, review ds #4). A String value longer than
   KV_VALUE_MAX is null and *cut is set: the caller lists the name in `previousTruncated` instead
   of handing the caller a prefix that the documented revert would write back (review opus #6). */
static void kv_previous_json(SB *out, CDmAttribute *a, int *cut) {
    char buf[KV_VALUE_MAX + 1];
    if (!a || !mem_ok(a) || !mem_ok((u8 *)a + sizeof *a - 1)) { sb_add(out, "null"); return; }
    int atype = (int)(a->typeFlags & S2_ATTR_TYPE_MASK);
    if (atype == S2_AT_STRING) {
        const char *s = attr_cstr(a);
        int got = s ? probe_str(s, buf, sizeof buf) : 0;
        if (got == 1) sb_addjson(out, buf);
        else { sb_add(out, "null"); if (got < 0) *cut = 1; }
    } else if (atype == S2_AT_INT) {
        int *v = (int *)a->value;
        if (v && mem_ok(v) && mem_ok((u8 *)v + 3)) sb_addi(out, *v); else sb_add(out, "null");
    } else if (atype == S2_AT_FLOAT) {
        float *v = (float *)a->value;
        if (v && mem_ok(v) && mem_ok((u8 *)v + 3)) sb_addf(out, *v); else sb_add(out, "null");
    } else if (atype == S2_AT_BOOL) {
        int *v = (int *)a->value;
        if (v && mem_ok(v) && mem_ok((u8 *)v + 3)) sb_add(out, *v ? "true" : "false");
        else sb_add(out, "null");
    } else sb_add(out, "null");
}

/* apply a {"name":value,...} object onto an entity_properties child (m2.15 §3.4; m2.18 routes
   the String writes through Hammer's own setter).
   Every FGD keyvalue in a saved .vmap is a DME String attribute (vmap-format.md §4), while JSON
   carries numbers and bools, so: a String attribute (or a key the entity does not have yet, which
   Hammer creates as a String) takes the JSON token's text (a number keeps its literal text, a
   bool becomes "1"/"0") through `g_hfn.SetKeyValue`, Hammer's own CMapGameDataNode::SetKeyValue;
   an existing int/float/bool attribute keeps the typed setters; any other attribute type is not
   applied and is reported in `warnings`.
   SetKeyValue is what makes a prop model load and draw (re-props.md Q1: it is the only one of the
   two that reaches the node's OnKeyValueChanged handler, where CMapEntity rebuilds the helpers and
   updates the CModelHelper). It requires `node` and a non-null `child` (it dereferences the
   element unguarded - callers pass what GetEntityProperties returned, and refuse the call when it
   is null).
   A string value longer than KV_VALUE_MAX is refused (`applied:false` + a warning naming the
   limit) instead of being written cut, and the keys after it still parse (review opus #7, ds #3).
   A key whose only match is a case variant of another spelling is refused the same way: SetKeyValue
   matches keys case-insensitively while FindAttribute (and so the `previous` pass) is exact, so
   writing it would store a value `previous` never reported.
   with_previous (the setKeyValues result, §3.3) adds `previous` - read in a pass of its own
   BEFORE anything is written - plus `previousTruncated` (the names whose previous value was too
   long to return: their entry is null and must not be re-applied) and `valueUndo` (`"recorded"`
   since m2.18: the callers RecordNodeTree the node before the first write, so the group pops the
   values back - re-props.md Q1's live finding). */
static void apply_keyvalues(CMapNode *node, CDmElement *child, const char *vp, SB *out, int with_previous) {
    static char abuf[8192], wbuf[4096], pbuf[65536], tbuf[2048];
    SB ap, wn, pv, tn;
    sb_init(&ap, abuf, sizeof abuf);
    sb_init(&wn, wbuf, sizeof wbuf);
    sb_init(&pv, pbuf, sizeof pbuf);
    sb_init(&tn, tbuf, sizeof tbuf);
    char name[96], sval[KV_VALUE_MAX + 1], nval[96];
    int kind = 0;
    long long ival = 0;
    double fval = 0;
    const char *p;

    if (with_previous) {
        int first = 1;
        p = vp + 1;
        for (;;) {
            const char *before = p;              /* kv_next provably advances; never hang the drain */
            int over = 0;
            p = kv_next(p, name, sizeof name, &kind, sval, sizeof sval, &ival, &fval,
                        nval, sizeof nval, &over);
            if (!p) break;
            if (p == before) { logline("kv: parser stalled (previous pass)"); out->ovf = 1; return; }
            S2_CUtlSymbolLarge sym = dm_intern(name);
            CDmAttribute *a = sym.m_pString ? g_hfn.FindAttribute(child, &sym) : 0;
            int cut = 0;
            if (!first) sb_add(&pv, ",");
            first = 0;
            sb_addjson(&pv, name);
            sb_add(&pv, ":");
            kv_previous_json(&pv, a, &cut);
            if (cut) { if (tn.len) sb_add(&tn, ","); sb_addjson(&tn, name); }
        }
    }

    int first = 1;
    p = vp + 1;
    for (;;) {
        const char *before = p;                  /* kv_next provably advances; never hang the drain */
        int over = 0;
        p = kv_next(p, name, sizeof name, &kind, sval, sizeof sval, &ival, &fval,
                    nval, sizeof nval, &over);
        if (!p) break;
        if (p == before) { logline("kv: parser stalled"); out->ovf = 1; return; }
        S2_CUtlSymbolLarge sym = dm_intern(name);
        CDmAttribute *attr = sym.m_pString ? g_hfn.FindAttribute(child, &sym) : 0;
        int applied = 0;
        /* the attribute's type, read once for the `over == 2` refusal below; it stays -1 for a
           missing or unreadable attribute (the two branches after this read it the long way) so a
           NULL `attr` is never dereferenced here */
        int atype = (attr && mem_ok(attr) && mem_ok((u8 *)attr + sizeof *attr - 1))
                  ? (int)(attr->typeFlags & S2_ATTR_TYPE_MASK) : -1;
        if (over == 1) {
            /* never write a cut value: refuse the key and say why (review opus #7, ds #3) */
            char msg[192]; SB w; sb_init(&w, msg, sizeof msg);
            sb_add(&w, name); sb_add(&w, ": value longer than "); sb_addi(&w, KV_VALUE_MAX);
            sb_add(&w, " bytes");
            if (!wn.ovf) { if (wn.len) sb_add(&wn, ","); sb_addjson(&wn, msg); }
        } else if (attr && (!mem_ok(attr) || !mem_ok((u8 *)attr + sizeof *attr - 1))) {
            /* a dangling attribute pointer: report it instead of faulting on typeFlags (ds #4) */
            char msg[192]; SB w; sb_init(&w, msg, sizeof msg);
            sb_add(&w, name); sb_add(&w, ": attribute not readable");
            if (!wn.ovf) { if (wn.len) sb_add(&wn, ","); sb_addjson(&wn, msg); }
        } else if (over == 2 && (atype == S2_AT_INT || atype == S2_AT_BOOL || atype == S2_AT_FLOAT)) {
            /* the number token is outside what this attribute's setter can hold (a 20-digit
               integer, or a float past FLT_MAX): refuse that key with a warning instead of
               dropping it and every key after it, and never write a narrowed value (review L6).
               A String attribute - here or for a key the entity does not have yet - keeps the
               token text through the `nval` paths below.
               review-5 L5 (not fixed, documented, raw-client reachable): `nval` is a 96-byte
               frame buffer, and `kv_next` cuts a longer token's text to 95 bytes. A number token
               with more than 95 characters stored on a String attribute (existing or newly
               created) answers `applied:true` and stores the CUT text, no warning. This is the
               same shape the pre-L6 float change already had for long in-range float tokens,
               MCP cannot reach it (the Rust layer sends keyvalues as JSON strings), and the
               review's suggested `over`-flag fix is deliberately skipped: it would refuse
               `{"key": <huge>}` on a String attribute that today keeps a (cut) value, changing
               behaviour for no live caller. */
            char msg[192]; SB w; sb_init(&w, msg, sizeof msg);
            sb_add(&w, name); sb_add(&w, ": number out of range for this attribute");
            if (!wn.ovf) { if (wn.len) sb_add(&wn, ","); sb_addjson(&wn, msg); }
        } else if (attr) {
            if (atype == S2_AT_STRING) {
                /* m2.18 (re-props.md Q1): Hammer's own CMapGameDataNode::SetKeyValue, not the raw
                   attribute setter. Only SetKeyValue runs the changed-value path: it writes the
                   String attribute and then calls the node's OnKeyValueChanged handler - for
                   CMapEntity that rebuilds the helpers, updates the CModelHelper (loads and draws
                   the model, updates the bounds) and handles
                   rendercolor/renderamt/disableshadows/solid/visoccluder. The raw SetValueString
                   reached the attribute storage only, which is why a raw-written model stayed the
                   white placeholder box (Q1, live).
                   `text` is a plain C string: SetKeyValue interns it itself, so the old
                   dm_intern + null-symbol write dance is gone with the raw setter.
                   The case-variant check runs here too, not only on the missing-key path:
                   SetKeyValue writes the FIRST case-insensitive match, so on an entity that holds
                   two spellings of the key (only an older bridge could have created that - it
                   CreateAttribute'd the caller's exact spelling) it could write the other one
                   while `previous`, which is exact, reports this one.
                   review-5 §3, documented not fixed (raw-client visible): SetValueString's own
                   type gate is what made a non-String attribute a silent no-op - that is why the
                   typed attributes below keep their setters, and why SetKeyValue is only reached
                   for a String attribute or a missing key (where it creates a type-5 one). */
                const char *variant = kv_case_variant(child, name);
                if (variant) kv_warn_variant(&wn, name, variant);
                else {
                    const char *text = kind == S2_AT_STRING ? sval
                                     : kind == S2_AT_BOOL ? (ival ? "1" : "0") : nval;
                    g_hfn.SetKeyValue(node, name, text);
                    applied = 1;
                }
            } else if (atype == S2_AT_INT || atype == S2_AT_BOOL) {
                if (kind == S2_AT_INT || kind == S2_AT_BOOL) {
                    /* the attribute is an `int`: a value between INT32_MAX and 2^63 would be
                       truncated by the cast and a *different* number stored - refuse it instead
                       (review L6; in-range tokens are unaffected) */
                    if (ival > 2147483647LL || ival < -2147483648LL) {
                        char msg[192]; SB w; sb_init(&w, msg, sizeof msg);
                        sb_add(&w, name); sb_add(&w, ": number out of range for an int attribute");
                        if (!wn.ovf) { if (wn.len) sb_add(&wn, ","); sb_addjson(&wn, msg); }
                    } else {
                        int iv = (int)ival;
                        g_hfn.SetValueInt(attr, &iv);
                        applied = 1;
                    }
                } else logline("kv: type mismatch (attr vs json)");
            } else if (atype == S2_AT_FLOAT) {
                if (kind == S2_AT_FLOAT) {
                    float fv = (float)fval;
                    g_hfn.SetValueFloat(attr, &fv);
                    applied = 1;
                } else logline("kv: type mismatch (attr vs json)");
            } else {
                /* an attribute type the four DME setters cannot write: report it for real */
                char msg[192]; SB w; sb_init(&w, msg, sizeof msg);
                sb_add(&w, name); sb_add(&w, ": attribute type "); sb_addi(&w, atype);
                sb_add(&w, " not writable");
                if (!wn.ovf) { if (wn.len) sb_add(&wn, ","); sb_addjson(&wn, msg); }
            }
        } else if (sym.m_pString) {
            /* a key the entity does not have: SetKeyValue creates the attribute itself, always as
               a String (type 5, §3.4 - never int/float/bool from the JSON kind), and then runs the
               same changed-value path as above. The case-variant check comes first: an attribute
               that matches only case-insensitively would be written by SetKeyValue while the
               `previous` pass (FindAttribute, exact) never saw it. */
            const char *variant = kv_case_variant(child, name);
            if (variant) {
                kv_warn_variant(&wn, name, variant);
            } else {
                const char *text = kind == S2_AT_STRING ? sval
                                 : kind == S2_AT_BOOL ? (ival ? "1" : "0") : nval;
                g_hfn.SetKeyValue(node, name, text);
                applied = 1;
            }
        }
        if (!first) sb_add(&ap, ",");
        first = 0;
        sb_addjson(&ap, name);
        sb_add(&ap, ":");
        sb_add(&ap, applied ? "true" : "false");
    }

    if (ap.ovf || wn.ovf || pv.ovf || tn.ovf) out->ovf = 1;   /* never emit a cut JSON object */
    sb_add(out, "{\"applied\":{"); sb_add(out, abuf);
    sb_add(out, "},\"warnings\":["); sb_add(out, wbuf); sb_add(out, "]");
    if (with_previous) {
        sb_add(out, ",\"previous\":{"); sb_add(out, pbuf);
        sb_add(out, "},\"previousTruncated\":["); sb_add(out, tbuf);
        /* m2.18: "recorded" - the caller RecordNodeTree'd the node before the first write, so
           undo.undo restores the values too (that closes m2.15's "group-only" gap; re-props.md Q1
           [V live]). The field NAME stays `valueUndo`: an older reader simply sees a new value. */
        sb_add(out, "],\"valueUndo\":\"recorded\"");
    }
    sb_add(out, "}");
}

/* the resolved target of an entity.setKeyValues op. The whole validation (handle/nodeID form, the
   node, the lock, the values member, the child element) happens here, so the standalone arm can
   run it BEFORE it opens an undo group (review opus #13: a refused call must not leave an empty
   "MCP: set keyvalues" group for undo_last to pop).
   `node` is what apply_keyvalues hands to SetKeyValue (m2.18) and what set_keyvalues_apply
   RecordNodeTrees; `record` is 0 for a node this batch created itself (its creation is already
   recorded in the same group - re-props.md Q1, no extra RecordNodeTree). */
typedef struct { CMapNode *node; CDmElement *child; const char *vp; int record; } SetKvTarget;

static int set_keyvalues_target(CMapDoc *doc, const char *params, SetKvTarget *t, OpErr *e) {
    char htok[32];
    htok[0] = 0;
    long long nid = 0;
    int has_handle = params && json_string_top(params, "handle", htok, sizeof htok);
    int has_node = params && json_find_top(params, "nodeID") != 0;
    if (has_node && !json_int_top(params, "nodeID", &nid)) {
        op_err(e, -32602, "InvalidParams", "nodeID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    /* m2.15 §3.3: exactly one of handle/nodeID */
    if (has_handle == has_node) {
        op_err(e, -32602, "InvalidParams", has_handle ? "handle and nodeID are mutually exclusive"
                                                     : "handle or nodeID required");
        return 0;
    }
    CMapNode *node = 0;
    if (has_node) {
        node = geo_entity_node(doc, nid, e);        /* -32602 not an entity, -32022 NodeNotFound */
        if (!node) return 0;
        if (g_hfn.NodeIsLocked(node, 0)) {
            op_err(e, -32018, "DocumentNotEditable", "node is locked");
            return 0;
        }
    } else {
        const char *hwhy = "?";
        node = (CMapNode *)handle_lookup(htok, &hwhy);
        if (!node) {
            op_err(e, -32005, "StaleHandle", hwhy);
            return 0;
        }
    }
    const char *vp = json_find_top(params, "values");
    if (!vp || *vp != '{') {
        op_err(e, -32602, "InvalidParams", "values");
        return 0;
    }
    CDmElement *child = g_hfn.GetEntityProperties(node);
    if (!child) {
        op_err(e, -32603, "InternalError", "entity_properties missing");
        return 0;
    }
    t->node = node;
    t->child = child;
    t->vp = vp;
    t->record = 1;
    return 1;
}

static void set_keyvalues_apply(CMapDoc *doc, const SetKvTarget *t, SB *out) {
    /* m2.18 (re-props.md Q1): RecordNodeTree ONCE per node, after the caller opened the undo group
       and before the first write - that is what every Hammer edit path does (the asset spray, "Add
       New Output", "Update local offset") and what turns the group from "group-only" into a real
       value undo: undo.undo restores the attribute values (verified live in re-props.md Q1).
       Skipped only for a node the same edit.batch created (t->record == 0): CreateNodeByName
       already recorded it in this group. */
    if (t->record) g_hfn.RecordNodeTree(g_hfn.GetActiveHistory(), CMapNode_Undoable(t->node), 1);
    apply_keyvalues(t->node, t->child, t->vp, out, 1);
    /* the DME setters do not mark the map changed (live 2026-10-14: setKeyValues then map.save ->
       skippedClean, file unchanged); do what Hammer's own edit paths do after a change */
    g_hfn.DocSetModified(doc, doc);
}

/* `record` = 0 for a node created earlier in the same edit.batch (its create recorded it) */
static int op_set_keyvalues(CMapDoc *doc, const char *params, SB *out, OpErr *e, int record) {
    SetKvTarget t;
    if (!set_keyvalues_target(doc, params, &t, e)) return 0;
    t.record = record;
    set_keyvalues_apply(doc, &t, out);
    return 1;
}

/* the undo-group label of an edit.batch. hammer-mcp's undo_last decrements its unsaved-edit
   counter for any popped "MCP: batch" group, which is right exactly when the batch created
   something (the pop reverts that create); a batch of nothing but entity.setKeyValues ops reverts
   nothing when popped, so it takes the keyvalue label the counter leaves alone - otherwise the
   counter drops while the values stay changed (review section 3: "a clean fix would be for the
   bridge to label a batch with no create ops \"MCP: set keyvalues\""). The label describes the
   whole batch, so it is decided before the group is opened (batch_has_create, in the entity JSON
   region above so the host test pins the decision).
   KNOWN LIMIT (review L5, unchanged and deliberately so): the scan reads the ops *listed*, while
   the run stops at the first refusal, so a raw mixed batch can still be labelled "MCP: batch" and
   hold only a keyvalue write - `[setKeyValues, create-that-fails]`. The label can only be chosen
   while the group is opened, so fixing it needs a lazily labelled group (a Hammer-side rename),
   not a scan; hammer-mcp's own create_entities sends creates only and cannot reach it. Documented
   in protocol-diff-m2.15.md item 16 and carried in m2.16. */
static const char *batch_undo_label(const char *ops) {
    return batch_has_create(ops) ? "MCP: batch" : "MCP: set keyvalues";
}

/* edit.batch: "$N" in a setKeyValues handle resolves to the handle issued by op N */
static void batch_subst_handle(char *params, u32 cap, char handles[][16], int hcount) {
    const char *h = json_find_top(params, "handle");
    if (!h || *h != '"' || h[1] != '$') return;
    int n = 0;
    const char *q = h + 2;
    while (*q >= '0' && *q <= '9') { n = n * 10 + (*q - '0'); q++; }
    if (n >= hcount || !handles[n][0]) return;
    char tmp[1536];
    u32 prefix = (u32)(h - params);
    u32 o = 0;
    if (prefix + 2 + slen(handles[n]) + slen(q) + 4 >= sizeof tmp) return;
    memcpy(tmp, params, prefix);
    o = prefix;
    tmp[o++] = '"';
    for (u32 i = 0; handles[n][i]; i++) tmp[o++] = handles[n][i];
    tmp[o++] = '"';
    const char *rest = q;
    while (*rest && *rest != '"') rest++;
    if (*rest == '"') rest++;
    while (*rest && o + 1 < sizeof tmp) tmp[o++] = *rest++;
    tmp[o] = 0;
    cpy(params, tmp, cap);
}

/* copy the next {"method":...,"params":{...}} op object into opbuf; returns 0 at end */
/* ---------------- request execution (main thread) ---------------- */
/* ---------------- view.render (phase-3 task 13) ----------------
   Hammer's own offscreen render. CMapViewMgr::RenderViewToMemory
   (hammer!0x1801843D0) is the primitive behind the map thumbnail written on every
   save (sub_1803CC4F0, from CHammerEditorSession::SaveDocument - so map.save has
   already run it from slot 13) and the "Take Large Screenshot" dialog
   (sub_180221240/sub_180221630). It builds a temporary CMapView from the template
   view's settings, renders it synchronously (SceneSystem BeginRenderingViews ->
   FinishRenderingViews -> WaitForRenderingToComplete) and copies the colour target
   into the caller's buffer with IRenderDevice::ReadTexturePixels (vfn+0x250).
   The call below mirrors the thumbnail: UpdateViews(session), a CFrustum from
   Hammer's default template, format 0 (RGBA8888), LinearToSRGB on every byte.
   (The frustum size is the generated S2_SIZEOF_CFrustum, s2sdk_gen.h.) */
#define VR_MAX_SIDE 4096u
#define VR_MAX_PIXELS (4096u * 2048u)
#define VR_SENTINEL 0xA55A5AA5u

static CFrustum g_vrFrustum __attribute__((aligned(16)));
/* sRGB encode LUT: Hammer's LinearToSRGB (hammer!0x180BFDD80: 12.92x below 0.0031308, else
   1.055*x^(1/2.4)-0.055) at i/255, truncated - exactly what the thumbnail/screenshot loop does */
static const u8 k_srgbLut[256] = { 0, 12, 21, 28, 33, 38, 42, 46, 49, 52, 55, 58, 61, 63, 66, 68, 70, 73, 75, 77, 79, 81, 82, 84, 86, 88, 89, 91, 93, 94, 96, 97, 99, 100, 102, 103, 104, 106, 107, 109, 110, 111, 112, 114, 115, 116, 117, 118, 120, 121, 122, 123, 124, 125, 126, 127, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 151, 152, 153, 154, 155, 156, 157, 157, 158, 159, 160, 161, 161, 162, 163, 164, 165, 165, 166, 167, 168, 168, 169, 170, 171, 171, 172, 173, 174, 174, 175, 176, 176, 177, 178, 179, 179, 180, 181, 181, 182, 183, 183, 184, 185, 185, 186, 187, 187, 188, 189, 189, 190, 191, 191, 192, 193, 193, 194, 194, 195, 196, 196, 197, 197, 198, 199, 199, 200, 201, 201, 202, 202, 203, 204, 204, 205, 205, 206, 206, 207, 208, 208, 209, 209, 210, 210, 211, 212, 212, 213, 213, 214, 214, 215, 215, 216, 217, 217, 218, 218, 219, 219, 220, 220, 221, 221, 222, 222, 223, 223, 224, 224, 225, 226, 226, 227, 227, 228, 228, 229, 229, 230, 230, 231, 231, 232, 232, 233, 233, 234, 234, 235, 235, 236, 236, 237, 237, 237, 238, 238, 239, 239, 240, 240, 241, 241, 242, 242, 243, 243, 244, 244, 245, 245, 245, 246, 246, 247, 247, 248, 248, 249, 249, 250, 250, 251, 251, 251, 252, 252, 253, 253, 254, 254, 254 };
static u32 g_crcTable[256];
static int g_crcInit;
static u32 g_renderSeq;

/* json_float1/json_float1_top live in the host-testable JSON region above (json_num) */
static int json_bool(const char *json, const char *key, int *out) {
    const char *p = json_find(json, key);
    if (!p) return 0;
    if (p[0] == 't' && p[1] == 'r' && p[2] == 'u' && p[3] == 'e') { *out = 1; return 1; }
    if (p[0] == 'f' && p[1] == 'a' && p[2] == 'l' && p[3] == 's' && p[4] == 'e') { *out = 0; return 1; }
    return 0;
}

static void crc_init(void) {
    if (g_crcInit) return;
    for (u32 n = 0; n < 256; n++) {
        u32 c = n;
        for (int k = 0; k < 8; k++) c = (c & 1) ? 0xEDB88320u ^ (c >> 1) : c >> 1;
        g_crcTable[n] = c;
    }
    g_crcInit = 1;
}
static u32 crc_update(u32 c, const u8 *p, u64 n) {
    while (n--) c = g_crcTable[(c ^ *p++) & 0xFF] ^ (c >> 8);
    return c;
}
static void put_be32(u8 *p, u32 v) { p[0] = (u8)(v >> 24); p[1] = (u8)(v >> 16); p[2] = (u8)(v >> 8); p[3] = (u8)v; }

/* RGB8 PNG with stored (uncompressed) deflate blocks; the server recompresses if
   it inlines the image. raw = h rows of {filter 0, w*3 bytes}. */
static u8 *png_encode(const u8 *raw, u32 w, u32 h, u64 *outLen) {
    u64 rawLen = (u64)h * (1 + 3ull * w);
    u64 nblk = (rawLen + 65534) / 65535;
    u64 zlen = 2 + rawLen + 5 * nblk + 4;
    u64 total = 8 + 25 + (12 + zlen) + 12;
    u8 *png = (u8 *)HeapAlloc(GetProcessHeap(), 0, total);
    if (!png) return 0;
    static const u8 sig[8] = { 0x89, 'P', 'N', 'G', 0x0D, 0x0A, 0x1A, 0x0A };
    u8 *p = png;
    for (int i = 0; i < 8; i++) *p++ = sig[i];
    /* IHDR */
    put_be32(p, 13); p += 4;
    u8 *ct = p;
    p[0] = 'I'; p[1] = 'H'; p[2] = 'D'; p[3] = 'R'; p += 4;
    put_be32(p, w); p += 4;
    put_be32(p, h); p += 4;
    *p++ = 8; *p++ = 2; *p++ = 0; *p++ = 0; *p++ = 0;   /* 8-bit, RGB, deflate, filter 0, no interlace */
    put_be32(p, crc_update(0xFFFFFFFFu, ct, 17) ^ 0xFFFFFFFFu); p += 4;
    /* IDAT */
    if (zlen > 0x7FFFFFFFull) { HeapFree(GetProcessHeap(), 0, png); return 0; }
    put_be32(p, (u32)zlen); p += 4;
    ct = p;
    p[0] = 'I'; p[1] = 'D'; p[2] = 'A'; p[3] = 'T'; p += 4;
    *p++ = 0x78; *p++ = 0x01;                          /* zlib: deflate, 32K window, no dict */
    u32 s1 = 1, s2 = 0;
    u64 off = 0;
    while (off < rawLen) {
        u32 n = (u32)((rawLen - off) > 65535 ? 65535 : (rawLen - off));
        *p++ = (off + n >= rawLen) ? 1 : 0;             /* BFINAL, BTYPE=00 (stored) */
        *p++ = (u8)n; *p++ = (u8)(n >> 8);
        *p++ = (u8)~n; *p++ = (u8)(~n >> 8);
        const u8 *src = raw + off;
        for (u32 i = 0; i < n; i++) {
            u8 c = src[i];
            p[i] = c;
            s1 += c; if (s1 >= 65521) s1 -= 65521;
            s2 += s1; if (s2 >= 65521) s2 -= 65521;
        }
        p += n;
        off += n;
    }
    put_be32(p, (s2 << 16) | s1); p += 4;
    put_be32(p, crc_update(0xFFFFFFFFu, ct, 4 + zlen) ^ 0xFFFFFFFFu); p += 4;
    /* IEND */
    put_be32(p, 0); p += 4;
    ct = p;
    p[0] = 'I'; p[1] = 'E'; p[2] = 'N'; p[3] = 'D'; p += 4;
    put_be32(p, crc_update(0xFFFFFFFFu, ct, 4) ^ 0xFFFFFFFFu); p += 4;
    *outLen = (u64)(p - png);
    return png;
}

static int write_file_atomic(const char *path, const u8 *data, u64 len) {
    char tmp[300];
    SB t; sb_init(&t, tmp, sizeof tmp);
    sb_add(&t, path); sb_add(&t, ".tmp");
    if (t.ovf) return 0;
    HANDLE h = CreateFileA(tmp, GENERIC_WRITE, 0, 0, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, 0);
    if (h == INVALID_HANDLE_VALUE || !h) return 0;
    u64 done = 0;
    int ok = 1;
    while (done < len) {
        u32 chunk = (u32)((len - done) > 0x4000000ull ? 0x4000000ull : (len - done));
        u32 w = 0;
        if (!WriteFile(h, data + done, chunk, &w, 0) || w == 0) { ok = 0; break; }
        done += w;
    }
    CloseHandle(h);
    if (!ok) { DeleteFileA(tmp); return 0; }
    if (!MoveFileExA(tmp, path, MOVEFILE_REPLACE_EXISTING)) { DeleteFileA(tmp); return 0; }
    return 1;
}

/* file stem: [A-Za-z0-9._-], max 64, no leading dot; a trailing ".png" is dropped */
static int render_name(const char *params, char *stem, u32 cap) {
    char raw[96];
    raw[0] = 0;
    if (params && json_string(params, "name", raw, sizeof raw) && raw[0]) {
        u32 n = slen(raw);
        if (n > 4 && raw[n - 4] == '.' && (raw[n - 3] | 32) == 'p' && (raw[n - 2] | 32) == 'n' && (raw[n - 1] | 32) == 'g')
            raw[n - 4] = 0;
        if (raw[0] == '.' || !raw[0] || slen(raw) > 64) return 0;
        for (u32 i = 0; raw[i]; i++) {
            char c = raw[i];
            int ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '.' || c == '_' || c == '-';
            if (!ok) return 0;
        }
        cpy(stem, raw, cap);
        return 1;
    }
    SB s; sb_init(&s, stem, cap);
    sb_add(&s, "render_");
    sb_addnum(&s, GetTickCount64());
    sb_add(&s, "_");
    sb_addnum(&s, ++g_renderSeq);
    return 1;
}

static void sb_addf3(SB *b, const float *v) {
    sb_add(b, "[");
    sb_addf(b, v[0]); sb_add(b, ",");
    sb_addf(b, v[1]); sb_add(b, ",");
    sb_addf(b, v[2]);
    sb_add(b, "]");
}

/* the CFrustum fields Hammer's screenshot dialog sets after the default init */
static void frustum_set(CFrustum *f, const float *origin, const float *angles, float fov, float aspect,
                        float znear, float zfar) {
    f->origin.x = origin[0]; f->origin.y = origin[1]; f->origin.z = origin[2];
    f->angles.x = angles[0]; f->angles.y = angles[1]; f->angles.z = angles[2];   /* generated S2_QAngle: x/y/z (was pitch/yaw/roll) */
    f->fovX = fov;
    f->aspect = aspect;
    f->znear = znear; f->zfar = zfar;
    f->reserved28[0] = -1.0f; f->reserved28[1] = -1.0f;        /* as Hammer's default template */
    f->ndc[0] = -1.0f; f->ndc[1] = -1.0f; f->ndc[2] = 1.0f; f->ndc[3] = 1.0f;   /* full viewport */
}

static int op_view_render(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    u8 *hb = (u8 *)g_modBase[PIN_HAMMER];
    CHammerEditorSession *session = CMapDocSession_Owner(CMapDoc_Session(doc));
    CMapViewMgr *mgr = session ? session->m_pViewMgr : 0;
    if (!mgr || !obj_is(mgr, PIN_HAMMER, HV_MAPVIEWMGR_VT)) {
        op_err(e, -32019, "NoViewport", mgr ? "bad_view_manager" : "no_view_manager");
        return 0;
    }
    CMapView *view = g_hfn.GetPreferred3DView(mgr);
    if (!view) { op_err(e, -32019, "NoViewport", "no_3d_view"); return 0; }
    if (!obj_is(view, PIN_HAMMER, HV_MAPVIEW_VT)) { op_err(e, -32019, "NoViewport", "bad_view"); return 0; }

    /* parameters */
    long long lw = 640, lh = 360, lfmt = 0;
    if (params) { json_int(params, "width", &lw); json_int(params, "height", &lh); json_int(params, "format", &lfmt); }
    if (lw < 16 || lh < 16 || lw > VR_MAX_SIDE || lh > VR_MAX_SIDE || (u64)lw * (u64)lh > VR_MAX_PIXELS) {
        op_err(e, -32602, "InvalidParams", "width/height: 16..4096 each, at most 8388608 pixels");
        return 0;
    }
    if (lfmt != 0 && lfmt != 2) { op_err(e, -32602, "InvalidParams", "format: 0 (RGBA8888) or 2 (RGB888)"); return 0; }
    u32 w = (u32)lw, h = (u32)lh;

    /* live 3D camera = defaults (the camera object embeds a CFrustum at +0x50) */
    CCamera *cam = view->vt->GetCamera(view);
    const CFrustum *live = CCamera_Frustum(cam);
    float vOrigin[3], vAngles[3];
    g_hfn.CameraGetPosition(cam, vOrigin);
    g_hfn.CameraGetAngles(cam, vAngles);
    float origin[3] = { vOrigin[0], vOrigin[1], vOrigin[2] };
    float angles[3] = { vAngles[0], vAngles[1], vAngles[2] };
    float fov = (live->fovX > 1.0f && live->fovX < 179.0f) ? live->fovX : 90.0f;
    float znear = 1.0f, zfar = 9000.0f;               /* the thumbnail/screenshot values ... */
    if (live->znear > 0.0f && live->zfar > live->znear && live->zfar <= 1000000.0f) { znear = live->znear; zfar = live->zfar; }   /* ... unless the viewport has its own */
    /* parse into temporaries: json_float3 writes element by element and may fail midway */
    float tmp3[3];
    int hasOrigin = 0, hasAngles = 0;
    if (params && json_find(params, "origin")) {
        if (!json_float3(params, "origin", tmp3)) { op_err(e, -32602, "InvalidParams", "origin: [x,y,z]"); return 0; }
        origin[0] = tmp3[0]; origin[1] = tmp3[1]; origin[2] = tmp3[2]; hasOrigin = 1;
    }
    if (params && json_find(params, "angles")) {
        if (!json_float3(params, "angles", tmp3)) { op_err(e, -32602, "InvalidParams", "angles: [pitch,yaw,roll]"); return 0; }
        angles[0] = tmp3[0]; angles[1] = tmp3[1]; angles[2] = tmp3[2]; hasAngles = 1;
    }
    if (params) {
        json_float1(params, "fov", &fov);
        json_float1(params, "znear", &znear);
        json_float1(params, "zfar", &zfar);
    }
    if (!(fov > 1.0f && fov < 179.0f)) { op_err(e, -32602, "InvalidParams", "fov: (1, 179) degrees"); return 0; }
    if (!(znear > 0.0f && zfar > znear && zfar <= 1000000.0f)) { op_err(e, -32602, "InvalidParams", "znear/zfar"); return 0; }
    int srgb = (lfmt == 0);
    if (params) json_bool(params, "srgb", &srgb);
    /* hideToolMaterials: the viewport's "Show Tools Materials" toggle, flipped around the
       render through its own handler (the thumbnail's filter modes 2/3 and the
       "Show Helpers" toggle were runtime-tested and change nothing in this path) */
    int hideTools = 0;
    if (params) json_bool(params, "hideToolMaterials", &hideTools);
    float aspect = (float)w / (float)h;
    char stem[80], path[200];
    if (!render_name(params, stem, sizeof stem)) {
        op_err(e, -32602, "InvalidParams", "name: [A-Za-z0-9._-], 1..64 chars, no leading dot");
        return 0;
    }

    /* CFrustum: Hammer's default template, then the same fields the screenshot dialog sets */
    CFrustum *fr = &g_vrFrustum;
    g_hfn.FrustumInit(fr);
    frustum_set(fr, origin, angles, fov, aspect, znear, zfar);
    g_hfn.FrustumUpdate(fr);

    u32 bpp = (lfmt == 0) ? 4u : 3u;
    u64 npx = (u64)w * h;
    u8 *px = (u8 *)HeapAlloc(GetProcessHeap(), 0, npx * bpp);
    u8 *raw = (u8 *)HeapAlloc(GetProcessHeap(), 0, (u64)h * (1 + 3ull * w));
    if (!px || !raw) {
        if (px) HeapFree(GetProcessHeap(), 0, px);
        if (raw) HeapFree(GetProcessHeap(), 0, raw);
        op_err(e, -32020, "RenderFailed", "out of memory");
        return 0;
    }
    for (u64 i = 0; i < npx * bpp; i += 4) {          /* sentinel: detects a readback that never ran */
        u32 s = VR_SENTINEL;
        for (u32 k = 0; k < 4 && i + k < npx * bpp; k++) px[i + k] = (u8)(s >> (8 * k));
    }

    CHammerApp *app = g_hammer;                       /* resolve_hammer ran in active_doc */
    S2HammerEditorSettings *settings = (S2HammerEditorSettings *)(hb + HG_EDITOR_SETTINGS);
    int toolsToggled = 0;
    if (hideTools && app && settings->showToolsMaterials) {
        g_hfn.ToggleToolsMaterials(app);
        toolsToggled = 1;
    }
    u64 t0 = GetTickCount64();
    g_hfn.SessionUpdateViews(session);
    g_hfn.RenderViewToMemory(mgr, fr, (int)w, (int)h, (int)lfmt, px, view);
    u64 t1 = GetTickCount64();
    if (toolsToggled) g_hfn.ToggleToolsMaterials(app);

    /* convert + stats */
    u64 untouched = 0, lumaSum = 0, nonBlack = 0;
    u32 lumaMin = 255, lumaMax = 0;
    for (u32 y = 0; y < h; y++) {
        u8 *row = raw + (u64)y * (1 + 3ull * w);
        row[0] = 0;
        for (u32 x = 0; x < w; x++) {
            const u8 *s = px + ((u64)y * w + x) * bpp;
            u64 bi = ((u64)y * w + x) * bpp;
            u32 sent = 1;
            for (u32 k = 0; k < bpp; k++) {
                u64 gi = bi + k;
                if (s[k] != (u8)(VR_SENTINEL >> (8 * (gi & 3)))) { sent = 0; break; }
            }
            untouched += sent;
            u8 r = s[0], g = s[1], bl = s[2];
            if (srgb) { r = k_srgbLut[r]; g = k_srgbLut[g]; bl = k_srgbLut[bl]; }
            row[1 + 3 * x] = r; row[2 + 3 * x] = g; row[3 + 3 * x] = bl;
            u32 luma = (54u * r + 183u * g + 19u * bl) >> 8;
            lumaSum += luma;
            if (luma < lumaMin) lumaMin = luma;
            if (luma > lumaMax) lumaMax = luma;
            if (luma > 8) nonBlack++;
        }
    }
    HeapFree(GetProcessHeap(), 0, px);
    if (untouched == npx) {
        HeapFree(GetProcessHeap(), 0, raw);
        op_err(e, -32020, "RenderFailed", "readback did not run (no pixels written)");
        return 0;
    }
    crc_init();
    u32 pixCrc = crc_update(0xFFFFFFFFu, raw, (u64)h * (1 + 3ull * w)) ^ 0xFFFFFFFFu;
    u64 pngLen = 0;
    u8 *png = png_encode(raw, w, h, &pngLen);
    HeapFree(GetProcessHeap(), 0, raw);
    if (!png) { op_err(e, -32020, "RenderFailed", "png encode: out of memory"); return 0; }

    CreateDirectoryA("Z:\\app\\renders", 0);
    SB ps; sb_init(&ps, path, sizeof path);
    sb_add(&ps, "Z:\\app\\renders\\"); sb_add(&ps, stem); sb_add(&ps, ".png");
    int wrote = !ps.ovf && write_file_atomic(path, png, pngLen);
    HeapFree(GetProcessHeap(), 0, png);
    u64 t2 = GetTickCount64();
    if (!wrote) { op_err(e, -32020, "RenderFailed", "could not write Z:\\app\\renders"); return 0; }
    logline("view.render: wrote");
    logline(path);

    static const char *hex = "0123456789abcdef";
    char fname[96];
    SB fs; sb_init(&fs, fname, sizeof fname);
    sb_add(&fs, stem); sb_add(&fs, ".png");
    sb_add(out, "{\"path\":"); sb_addjson(out, path);
    sb_add(out, ",\"file\":"); sb_addjson(out, fname);
    sb_add(out, ",\"width\":"); sb_addnum(out, w);
    sb_add(out, ",\"height\":"); sb_addnum(out, h);
    sb_add(out, ",\"bytes\":"); sb_addnum(out, pngLen);
    sb_add(out, ",\"camera\":{\"origin\":"); sb_addf3(out, origin);
    sb_add(out, ",\"angles\":"); sb_addf3(out, angles);
    sb_add(out, ",\"fov\":"); sb_addf(out, fov);
    sb_add(out, ",\"aspect\":"); sb_addf(out, aspect);
    sb_add(out, ",\"znear\":"); sb_addf(out, znear);
    sb_add(out, ",\"zfar\":"); sb_addf(out, zfar);
    sb_add(out, ",\"source\":"); sb_addjson(out, (hasOrigin || hasAngles) ? (hasOrigin && hasAngles ? "explicit" : "mixed") : "viewport");
    sb_add(out, ",\"viewport\":{\"origin\":"); sb_addf3(out, vOrigin);
    sb_add(out, ",\"angles\":"); sb_addf3(out, vAngles);
    sb_add(out, ",\"fov\":"); sb_addf(out, live->fovX);
    sb_add(out, ",\"aspect\":"); sb_addf(out, live->aspect);
    sb_add(out, ",\"znear\":"); sb_addf(out, live->znear);
    sb_add(out, ",\"zfar\":"); sb_addf(out, live->zfar);
    sb_add(out, "}}");
    sb_add(out, ",\"view\":{\"type\":"); sb_addnum(out, (u32)view->m_nViewType);
    sb_add(out, ",\"renderMode\":"); sb_addnum(out, (u32)view->m_nRenderMode);
    sb_add(out, ",\"views\":"); sb_addnum(out, CMapViewMgr_ViewCount(mgr));
    sb_add(out, "}");
    sb_add(out, ",\"toolMaterials\":"); sb_add(out, (toolsToggled || !settings->showToolsMaterials) ? "\"hidden\"" : "\"shown\"");
    sb_add(out, ",\"format\":"); sb_addnum(out, (u64)lfmt);
    sb_add(out, ",\"srgb\":"); sb_add(out, srgb ? "true" : "false");
    sb_add(out, ",\"stats\":{\"untouchedPixels\":"); sb_addnum(out, untouched);
    sb_add(out, ",\"nonBlackPixels\":"); sb_addnum(out, nonBlack);
    sb_add(out, ",\"meanLuma\":"); sb_addf(out, (float)((double)lumaSum / (double)npx));
    sb_add(out, ",\"minLuma\":"); sb_addnum(out, lumaMin);
    sb_add(out, ",\"maxLuma\":"); sb_addnum(out, lumaMax);
    sb_add(out, ",\"crc32\":\"");
    for (int i = 7; i >= 0; i--) sb_putc(out, hex[(pixCrc >> (4 * i)) & 15]);
    sb_add(out, "\"}");
    sb_add(out, ",\"ms\":{\"render\":"); sb_addnum(out, t1 - t0);
    sb_add(out, ",\"encode\":"); sb_addnum(out, t2 - t1);
    sb_add(out, "}}");
    return 1;
}



/* ---------------- world.render: Hammer-less offscreen render of the running map ----------------
   The engine's own pieces (docs/flows/map-preview-freecam.md, worknotes/worldpreview-scratch.md):
   SceneSystem_002 Begin/AddView/Finish/Wait, the "Tools" (-> CSGO) rendering pipeline, the
   view's own scratch colour/depth targets at the requested size, RenderDevice003 ReadTexturePixels.
   The engine objects come from the Connect factory by interface name (s2sdk.h); every object's
   vtable identity and pinned slots are checked before it is used. The whole Begin..Wait bracket runs inside this one main-thread call:
   the engine must not start its own frame in between ("Begin has already been called"). */
#define WR_MAX_SIDE 8192u
#define WR_MAX_PIXELS (8192u * 4096u)
#define WR_COLOR_FMT S2_CONST_IMAGE_FORMAT_BGRA8888   /* colour scratch format (generated, s2sdk_gen.h) */
static union { S2RenderAttrs a; u8 slack[1024]; } g_wrAttrs __attribute__((aligned(16)));   /* engine block is 928 bytes */
static CFrustum g_wrFrustum __attribute__((aligned(16)));

/* engine interfaces by name (the Connect factory), identity- and slot-checked */
typedef struct {
    ISceneSystem *sceneSystem;
    ISceneUtils *sceneUtils;
    IRenderService *renderService;
    IRenderDevice *renderDevice;
} EngineIfaces;
static EngineIfaces g_eng;
static int g_engOk;

static void *factory_get(const char *name) {
    if (!g_connectFactory) return 0;
    int ret = -1;
    void *p = ((S2CreateInterfaceFn)g_connectFactory)(name, &ret);
    if (!p) { logline("iface: not found: "); logline(name); }
    return p;
}

static int engine_ifaces(const char **why) {
    if (g_engOk) return 1;
    *why = "engine interface identity check failed";
    ISceneSystem *ss = (ISceneSystem *)factory_get(S2_IFACE_SCENESYSTEM);
    ISceneUtils *su = (ISceneUtils *)factory_get(S2_IFACE_SCENEUTILS);
    IRenderService *rs = (IRenderService *)factory_get(S2_IFACE_RENDERSERVICE);
    IRenderDevice *rd = (IRenderDevice *)factory_get(S2_IFACE_RENDER_DEVICE);
    if (!ss || !su || !rs || !rd) { *why = "engine interface not found (Connect factory)"; return 0; }
    if (!obj_is(ss, PIN_SCENESYSTEM, SV_SCENESYSTEM_VT) || !obj_is(su, PIN_SCENESYSTEM, SV_SCENEUTILS_VT) ||
        !obj_is(rs, PIN_ENGINE2, EV_RENDERSERVICE_VT) || !obj_is(rd, PIN_RSV, RV_RENDERDEVICE_VT))
        return 0;
    /* objects created per call are checked when they appear; their vtables' slots now */
    if (!vt_slots_ok(PIN_SCENESYSTEM, SV_SCENEVIEW_VT) || !vt_slots_ok(PIN_SCENESYSTEM, SV_PIPELINE_ALIAS_VT)) return 0;
    g_eng.sceneSystem = ss; g_eng.sceneUtils = su; g_eng.renderService = rs; g_eng.renderDevice = rd;
    u8 *sbase = (u8 *)g_modBase[PIN_SCENESYSTEM], *ebase = (u8 *)g_modBase[PIN_ENGINE2];
    *(void **)&g_sfn.FrustumInit = sbase + SFN_FRUSTUM_INIT;
    *(void **)&g_sfn.FrustumUpdate = sbase + SFN_FRUSTUM_UPDATE;
    *(void **)&g_efn.AttrsCtor = ebase + EFN_ATTRS_CTOR;
    *(void **)&g_efn.AttrsDtor = ebase + EFN_ATTRS_DTOR;
    *(void **)&g_efn.AttrsSetHandle = ebase + EFN_ATTRS_SET_HANDLE;
    g_sfnBound = g_efnBound = 1;
    g_engOk = 1;
    logline("iface: engine interfaces resolved by name");
    return 1;
}

static int op_world_render(const char *params, SB *out, OpErr *e) {
    const char *why = "?";
    if (!engine_ifaces(&why)) { op_err(e, -32020, "RenderFailed", why); return 0; }
    ISceneSystem *ss = g_eng.sceneSystem;
    IRenderDevice *rd = g_eng.renderDevice;
    ISceneWorld *world = g_eng.renderService->vt->GetSceneWorld(g_eng.renderService);
    if (!world) { op_err(e, -32021, "NoGameWorld", "no map is loaded in the engine (map <name>)"); return 0; }
    if (!obj_is(world, PIN_SCENESYSTEM, SV_SCENEWORLD_VT)) { op_err(e, -32020, "RenderFailed", "scene world identity check failed"); return 0; }
    IRenderingPipeline *pipe = g_eng.sceneUtils->vt->FindRenderingPipeline(g_eng.sceneUtils, S2_CONST_TOKEN_TOOLS);
    if (!obj_is(pipe, PIN_SCENESYSTEM, SV_PIPELINE_ALIAS_VT)) { op_err(e, -32020, "RenderFailed", "Tools rendering pipeline not found"); return 0; }

    /* parameters: origin + angles required (there is no viewport camera to default to) */
    long long lw = 1920, lh = 1080;
    float origin[3], angles[3], fov = 90.0f, znear = 4.0f, zfar = 30000.0f;
    if (!params || !json_float3(params, "origin", origin)) { op_err(e, -32602, "InvalidParams", "origin: [x,y,z] required"); return 0; }
    if (!json_float3(params, "angles", angles)) { op_err(e, -32602, "InvalidParams", "angles: [pitch,yaw,roll] required"); return 0; }
    json_int(params, "width", &lw); json_int(params, "height", &lh);
    json_float1(params, "fov", &fov); json_float1(params, "znear", &znear); json_float1(params, "zfar", &zfar);
    if (lw < 16 || lh < 16 || lw > WR_MAX_SIDE || lh > WR_MAX_SIDE || (u64)lw * (u64)lh > WR_MAX_PIXELS) {
        op_err(e, -32602, "InvalidParams", "width/height: 16..8192 each, at most 33554432 pixels");
        return 0;
    }
    if (!(fov > 1.0f && fov < 179.0f)) { op_err(e, -32602, "InvalidParams", "fov: (1, 179) degrees, horizontal"); return 0; }
    if (!(znear > 0.0f && zfar > znear && zfar <= 1000000.0f)) { op_err(e, -32602, "InvalidParams", "znear/zfar"); return 0; }
    int srgb = 0;
    json_bool(params, "srgb", &srgb);
    char stem[80], path[200];
    if (!render_name(params, stem, sizeof stem)) {
        op_err(e, -32602, "InvalidParams", "name: [A-Za-z0-9._-], 1..64 chars, no leading dot");
        return 0;
    }
    u32 w = (u32)lw, h = (u32)lh;
    u64 npx = (u64)w * h;
    u8 *px = (u8 *)HeapAlloc(GetProcessHeap(), 0, npx * 4);
    u8 *raw = (u8 *)HeapAlloc(GetProcessHeap(), 0, (u64)h * (1 + 3ull * w));
    if (!px || !raw) {
        if (px) HeapFree(GetProcessHeap(), 0, px);
        if (raw) HeapFree(GetProcessHeap(), 0, raw);
        op_err(e, -32020, "RenderFailed", "out of memory");
        return 0;
    }
    for (u64 i = 0; i < npx; i++) ((u32 *)px)[i] = VR_SENTINEL;

    /* camera */
    CFrustum *fr = &g_wrFrustum;
    g_sfn.FrustumInit(fr);
    frustum_set(fr, origin, angles, fov, (float)w / (float)h, znear, zfar);
    g_sfn.FrustumUpdate(fr);

    /* per-view render attributes (colour/depth handles go in the handle table) */
    S2RenderAttrs *attrs = &g_wrAttrs.a;
    for (u32 i = 0; i < sizeof g_wrAttrs; i++) g_wrAttrs.slack[i] = 0;
    g_efn.AttrsCtor(attrs);

    S2_RenderViewport_t vp = { 1, 0, 0, (int)w, (int)h, 0.0f, 1.0f };   /* generated RenderViewport_t (28 B) */
    S2RenderTargetDesc rt = { (int)w, (int)h, 0, 0 };
    u8 vid[16] __attribute__((aligned(16))) = { 0 };   /* CreateViewId leaves bytes 12-15 untouched */
    const char *names[2] = { "scratchcolor", "scratchdepth" };
    u8 dq = rd->vt->IsUsingReverseDepth(rd);
    int fmts[2] = { WR_COLOR_FMT, dq ? S2_CONST_IMAGE_FORMAT_D32FS8 : S2_CONST_IMAGE_FORMAT_D24S8 };   /* formats (generated, s2sdk_gen.h) */
    u32 bind[2] = { pipe->vt->ColorBindFlags(pipe), pipe->vt->DepthBindFlags(pipe) };
    u32 tflags = pipe->vt->ScratchTargetFlags(pipe) | 1u;
    long long hcol = -1, hdep = -1;
    long long *outs[2] = { &hcol, &hdep };
    const char *fail = 0;
    void *tex = 0;
    int read = 0;

    u64 t0 = GetTickCount64();
    logline("world.render: begin");
    ss->vt->WaitForRenderingToComplete(ss);
    ss->vt->BeginRenderingViews(ss, rd, 1);
    ss->vt->CreateViewId(ss, vid, 0xFFFFFFFFu);
    ISceneView *view = ss->vt->AddView(ss, "mcp world.render", vid, fr, 0, world, &vp, 0,
                                       65536u /* nPriority (1 << 16): the default view priority the sun/CSM pipeline passes - a value the bridge chooses, not a flag set */);
    if (!obj_is(view, PIN_SCENESYSTEM, SV_SCENEVIEW_VT)) fail = "AddView failed";
    if (!fail) {
        logline("world.render: view");
        /* SIZED_SCRATCH lets CreateScratchTargets allocate new targets of the requested size instead of
           only reusing the screen-sized pool; TOOLS3D matches Hammer's 3D views (PreRenderSetup: 18) */
        view->vt->AddFlags(view, 16u /* TOOLS3D (1 << 4): bridge-owned literal, no sigs entry - Hammer only ever passes TOOLS3D | SIZED_SCRATCH (18) */ | S2_VIEWFLAG_SIZED_SCRATCH);
        view->vt->CreateScratchTargets(view, 2, names, fmts, bind, outs, w, h, 0, tflags);
        if (hcol == -1 || hdep == -1) fail = "scratch target allocation failed";
    }
    if (!fail) {
        logline("world.render: layers");
        g_efn.AttrsSetHandle(attrs->handleTable, S2_CONST_TOKEN_OUTPUT_COLOR, hcol);
        g_efn.AttrsSetHandle(attrs->handleTable, S2_CONST_TOKEN_OUTPUT_DEPTH, hdep);
        view->vt->BeginLayerGroup(view, "Main");
        pipe->vt->AddLayersToView(pipe, view, &vp, &rt, attrs);
        view->vt->EndLayerGroup(view);
        S2_RenderViewport_t vpOut;   /* slot 3 writes a whole 28-byte viewport here (was a 4-byte int: stack overwrite) */
        pipe->vt->GetLayerViewport(pipe, 0, &vp, &vpOut);
        view->vt->MarkRenderTargetForReadback(view, hcol);
    }
    logline("world.render: finish");
    ss->vt->FinishRenderingViews(ss, FLT_MAX);   /* FLT_MAX = "use the current platform time" (flTime is read from xmm1) */
    ss->vt->WaitForRenderingToComplete(ss);
    u64 t1 = GetTickCount64();
    if (!fail) {
        view->vt->GetRenderTargetTexture(view, &tex, hcol);
        if (!tex) fail = "colour target has no texture";
    }
    if (!fail) {
        logline("world.render: readback");
        S2Rect rect = { 0, 0, (int)w, (int)h };
        read = (u8)rd->vt->ReadTexturePixels(rd, tex, &rect, 0, 0, &rect, px, S2_CONST_IMAGE_FORMAT_RGBA8888, (int)(w * 4));
        if (!read) fail = "ReadTexturePixels failed";
    }
    g_efn.AttrsDtor(attrs);
    u64 t2 = GetTickCount64();
    if (fail) {
        HeapFree(GetProcessHeap(), 0, px); HeapFree(GetProcessHeap(), 0, raw);
        logline("world.render: failed");
        op_err(e, -32020, "RenderFailed", fail);
        return 0;
    }

    u64 untouched = 0, lumaSum = 0, nonBlack = 0;
    u32 lumaMin = 255, lumaMax = 0;
    for (u32 y = 0; y < h; y++) {
        u8 *row = raw + (u64)y * (1 + 3ull * w);
        row[0] = 0;
        for (u32 x = 0; x < w; x++) {
            u64 i = (u64)y * w + x;
            if (((u32 *)px)[i] == VR_SENTINEL) untouched++;
            const u8 *s = px + i * 4;
            u8 r = s[0], g = s[1], bl = s[2];
            if (srgb) { r = k_srgbLut[r]; g = k_srgbLut[g]; bl = k_srgbLut[bl]; }
            row[1 + 3 * x] = r; row[2 + 3 * x] = g; row[3 + 3 * x] = bl;
            u32 luma = (54u * r + 183u * g + 19u * bl) >> 8;
            lumaSum += luma;
            if (luma < lumaMin) lumaMin = luma;
            if (luma > lumaMax) lumaMax = luma;
            if (luma > 8) nonBlack++;
        }
    }
    HeapFree(GetProcessHeap(), 0, px);
    if (untouched == npx) {
        HeapFree(GetProcessHeap(), 0, raw);
        op_err(e, -32020, "RenderFailed", "readback did not run (no pixels written)");
        return 0;
    }
    crc_init();
    u32 pixCrc = crc_update(0xFFFFFFFFu, raw, (u64)h * (1 + 3ull * w)) ^ 0xFFFFFFFFu;
    u64 pngLen = 0;
    u8 *png = png_encode(raw, w, h, &pngLen);
    HeapFree(GetProcessHeap(), 0, raw);
    if (!png) { op_err(e, -32020, "RenderFailed", "png encode: out of memory"); return 0; }
    CreateDirectoryA("Z:\\app\\renders", 0);
    SB ps; sb_init(&ps, path, sizeof path);
    sb_add(&ps, "Z:\\app\\renders\\"); sb_add(&ps, stem); sb_add(&ps, ".png");
    int wrote = !ps.ovf && write_file_atomic(path, png, pngLen);
    HeapFree(GetProcessHeap(), 0, png);
    u64 t3 = GetTickCount64();
    if (!wrote) { op_err(e, -32020, "RenderFailed", "could not write Z:\\app\\renders"); return 0; }
    logline("world.render: wrote");
    logline(path);

    static const char *hex = "0123456789abcdef";
    char fname[96];
    SB fs; sb_init(&fs, fname, sizeof fname);
    sb_add(&fs, stem); sb_add(&fs, ".png");
    sb_add(out, "{\"path\":"); sb_addjson(out, path);
    sb_add(out, ",\"file\":"); sb_addjson(out, fname);
    sb_add(out, ",\"width\":"); sb_addnum(out, w);
    sb_add(out, ",\"height\":"); sb_addnum(out, h);
    sb_add(out, ",\"bytes\":"); sb_addnum(out, pngLen);
    sb_add(out, ",\"camera\":{\"origin\":"); sb_addf3(out, origin);
    sb_add(out, ",\"angles\":"); sb_addf3(out, angles);
    sb_add(out, ",\"fov\":"); sb_addf(out, fov);
    sb_add(out, ",\"znear\":"); sb_addf(out, znear);
    sb_add(out, ",\"zfar\":"); sb_addf(out, zfar);
    sb_add(out, "},\"pipeline\":\"Tools->CSGO\",\"srgb\":"); sb_add(out, srgb ? "true" : "false");
    sb_add(out, ",\"stats\":{\"untouchedPixels\":"); sb_addnum(out, untouched);
    sb_add(out, ",\"nonBlackPixels\":"); sb_addnum(out, nonBlack);
    sb_add(out, ",\"meanLuma\":"); sb_addf(out, (float)((double)lumaSum / (double)npx));
    sb_add(out, ",\"minLuma\":"); sb_addnum(out, lumaMin);
    sb_add(out, ",\"maxLuma\":"); sb_addnum(out, lumaMax);
    sb_add(out, ",\"crc32\":\"");
    for (int i = 7; i >= 0; i--) sb_putc(out, hex[(pixCrc >> (4 * i)) & 15]);
    sb_add(out, "\"}");
    sb_add(out, ",\"ms\":{\"render\":"); sb_addnum(out, t1 - t0);
    sb_add(out, ",\"readback\":"); sb_addnum(out, t2 - t1);
    sb_add(out, ",\"encode\":"); sb_addnum(out, t3 - t2);
    sb_add(out, "}}");
    return 1;
}


/* ---------------- geometry: mesh.* / node.* (Hammer's polygon-tool recipe) ----------------
   docs/flows/hammer-mesh-edit.md, worknotes/geometry-scratch.md. New meshes are built exactly like
   Hammer's Polygon tool: CreateElement<CMapMesh> -> CMapMesh::InitializeFromPolygonGroup (CreateFace
   per polygon, MergeVerticesWithinDistance, texture init, pivot to bounds centre) -> InsertNode ->
   RecordNode. Existing meshes: RecordNodeTree before the change (the DeleteFaces tool pattern).
   Delete: CMapDoc::DeleteNode (records its own undo). Nodes are addressed by their nodeID attribute
   (the same id the .vmap reader reports), resolved by walking the live tree on every call. */
#define GEO_MAX_VERTS 65536
#define GEO_MAX_FACES 65536
#define GEO_MAX_IDX (1u << 20)
#define GEO_MAX_MATS 64
#define GEO_MAX_NODES 200000
#define GEO_UV_MODE 2                    /* what Hammer's Polygon tool passes */

typedef struct { float x, y, z; } GVec;
typedef struct { int n; int pad; GVec *pts; u64 zero; } GPolyRec;          /* 24 bytes */
typedef struct { int count; int pad; GPolyRec *recs; } GPolyGroup;
typedef struct {
    int nv, nf, nidx;
    GVec *v;
    int *fstart, *fcount, *idx;
    int *fmat;                         /* per face material index, -1 = default */
    int nmat;
    char mats[GEO_MAX_MATS][260];
    char defmat[260];
} GGeom;

static void *geo_alloc(u64 n) { return HeapAlloc(GetProcessHeap(), 8 /* HEAP_ZERO_MEMORY */, n ? n : 1); }
static void geo_free(void *p) { if (p) HeapFree(GetProcessHeap(), 0, p); }
static void geom_free(GGeom *g) {
    geo_free(g->v); geo_free(g->fstart); geo_free(g->fcount); geo_free(g->idx); geo_free(g->fmat);
    geo_free(g);
}

/* JSON number with optional fraction/exponent */
static const char *jnum(const char *p, double *out) {
    while (*p == ' ' || *p == '\n' || *p == '\t' || *p == '\r') p++;
    int neg = 0;
    if (*p == '-') { neg = 1; p++; }
    if (*p < '0' || *p > '9') return 0;
    double v = 0;
    while (*p >= '0' && *p <= '9') { v = v * 10 + (*p - '0'); p++; }
    if (*p == '.') { p++; double f = 0.1; while (*p >= '0' && *p <= '9') { v += (*p - '0') * f; f *= 0.1; p++; } }
    if (*p == 'e' || *p == 'E') {
        p++;
        int eneg = 0, ex = 0;
        if (*p == '+') p++; else if (*p == '-') { eneg = 1; p++; }
        if (*p < '0' || *p > '9') return 0;
        while (*p >= '0' && *p <= '9') { ex = ex * 10 + (*p - '0'); p++; if (ex > 300) return 0; }
        while (ex--) v = eneg ? v / 10.0 : v * 10.0;
    }
    *out = neg ? -v : v;
    return p;
}
static const char *jws(const char *p) { while (*p == ' ' || *p == '\n' || *p == '\t' || *p == '\r') p++; return p; }
/* JSON string at p (must start with "), copied to out (max cap-1), no escapes beyond \" \\ \/ */
static const char *jstr(const char *p, char *out, u32 cap) {
    p = jws(p);
    if (*p != '"') return 0;
    p++;
    u32 n = 0;
    while (*p && *p != '"') {
        char c = *p++;
        if (c == '\\') { c = *p++; if (c != '"' && c != '\\' && c != '/') return 0; }
        if (n + 1 >= cap) return 0;
        out[n++] = c;
    }
    if (*p != '"') return 0;
    out[n] = 0;
    return p + 1;
}

static int geo_mat_index(GGeom *g, const char *m) {
    for (int i = 0; i < g->nmat; i++) if (seq(g->mats[i], m)) return i;
    if (g->nmat >= GEO_MAX_MATS) return -2;
    cpy(g->mats[g->nmat], m, sizeof g->mats[0]);
    return g->nmat++;
}

/* {vertices:[[x,y,z]...], faces:[[i,j,k,...]...], material:"...", faceMaterials:["..."|null ...]} */
static GGeom *geom_parse(const char *params, const char **why) {
    GGeom *g = (GGeom *)geo_alloc(sizeof(GGeom));
    if (!g) { *why = "out of memory"; return 0; }
    g->v = (GVec *)geo_alloc(sizeof(GVec) * GEO_MAX_VERTS);
    g->fstart = (int *)geo_alloc(sizeof(int) * GEO_MAX_FACES);
    g->fcount = (int *)geo_alloc(sizeof(int) * GEO_MAX_FACES);
    g->fmat = (int *)geo_alloc(sizeof(int) * GEO_MAX_FACES);
    g->idx = (int *)geo_alloc(sizeof(int) * GEO_MAX_IDX);
    if (!g->v || !g->fstart || !g->fcount || !g->fmat || !g->idx) { *why = "out of memory"; geom_free(g); return 0; }
    const char *p = json_find(params, "vertices");
    if (!p || *p != '[') { *why = "vertices: [[x,y,z],...] required"; geom_free(g); return 0; }
    p = jws(p + 1);
    while (*p == '[') {
        if (g->nv >= GEO_MAX_VERTS) { *why = "too many vertices (max 65536)"; geom_free(g); return 0; }
        double c[3];
        p++;
        for (int k = 0; k < 3; k++) {
            p = jnum(p, &c[k]);
            if (!p || !(c[k] > -1e6 && c[k] < 1e6)) { *why = "vertices: each must be [x,y,z] with |coord| < 1e6"; geom_free(g); return 0; }
            p = jws(p);
            if (k < 2) { if (*p != ',') { *why = "vertices: each must be [x,y,z]"; geom_free(g); return 0; } p++; }
        }
        if (*p != ']') { *why = "vertices: each must be [x,y,z]"; geom_free(g); return 0; }
        g->v[g->nv].x = (float)c[0]; g->v[g->nv].y = (float)c[1]; g->v[g->nv].z = (float)c[2];
        g->nv++;
        p = jws(p + 1);
        if (*p == ',') p = jws(p + 1);
    }
    if (*p != ']' || g->nv < 3) { *why = "vertices: at least 3 [x,y,z]"; geom_free(g); return 0; }
    p = json_find(params, "faces");
    if (!p || *p != '[') { *why = "faces: [[i,j,k,...],...] required"; geom_free(g); return 0; }
    p = jws(p + 1);
    while (*p == '[') {
        if (g->nf >= GEO_MAX_FACES) { *why = "too many faces (max 65536)"; geom_free(g); return 0; }
        g->fstart[g->nf] = g->nidx;
        p = jws(p + 1);
        while (*p != ']') {
            double d;
            p = jnum(p, &d);
            if (!p || d < 0 || d >= g->nv || d != (double)(int)d) { *why = "faces: vertex indices must be integers in range"; geom_free(g); return 0; }
            if ((u32)g->nidx >= GEO_MAX_IDX) { *why = "too many face indices"; geom_free(g); return 0; }
            g->idx[g->nidx++] = (int)d;
            p = jws(p);
            if (*p == ',') p = jws(p + 1);
            else if (*p != ']') { *why = "faces: malformed"; geom_free(g); return 0; }
        }
        g->fcount[g->nf] = g->nidx - g->fstart[g->nf];
        if (g->fcount[g->nf] < 3) { *why = "faces: each needs at least 3 vertices"; geom_free(g); return 0; }
        g->fmat[g->nf] = -1;
        g->nf++;
        p = jws(p + 1);
        if (*p == ',') p = jws(p + 1);
    }
    if (*p != ']' || g->nf < 1) { *why = "faces: at least one face"; geom_free(g); return 0; }
    if (!json_string(params, "material", g->defmat, sizeof g->defmat) || !g->defmat[0]) {
        *why = "material: a material path, e.g. materials/dev/reflectivity_30.vmat"; geom_free(g); return 0;
    }
    p = json_find(params, "faceMaterials");
    if (p && *p == '[') {
        p = jws(p + 1);
        int fi = 0;
        while (*p != ']') {
            if (fi >= g->nf) { *why = "faceMaterials: longer than faces"; geom_free(g); return 0; }
            if (*p == 'n') { if (p[1] != 'u' || p[2] != 'l' || p[3] != 'l') { *why = "faceMaterials: strings or null"; geom_free(g); return 0; } p += 4; }
            else {
                char m[260];
                p = jstr(p, m, sizeof m);
                if (!p) { *why = "faceMaterials: strings or null"; geom_free(g); return 0; }
                if (m[0] && !seq(m, g->defmat)) {
                    int mi = geo_mat_index(g, m);
                    if (mi < 0) { *why = "faceMaterials: at most 64 distinct materials"; geom_free(g); return 0; }
                    g->fmat[fi] = mi;
                }
            }
            fi++;
            p = jws(p);
            if (*p == ',') p = jws(p + 1);
            else if (*p != ']') { *why = "faceMaterials: malformed"; geom_free(g); return 0; }
        }
    }
    return g;
}

/* ---- live tree ---- */
static CDmElement *dm_element(u32 handle) {
    IDataModel *dm = data_model();
    return dm ? dm->vt->GetElement(dm, handle) : 0;
}
static int is_map_node(CDmElement *e) {
    if (!e || !mem_ok(e)) return 0;
    S2_CUtlSymbolLarge sym;
    sym.m_pString = *(const char **)((u8 *)g_modBase[PIN_HAMMER] + HG_MAPNODE_TYPE_SYM);
    return e->vt->IsA(e, &sym) != 0;            /* polymorphic (per-class override), as RegisterNode */
}
static CDmAttribute *elem_attr(CDmElement *elem, const char *name) {
    CDmAttribute *a = elem->attrs;
    for (int guard = 0; a && guard < 4096; guard++) {
        if (!mem_ok(a) || !mem_ok((u8 *)a + sizeof *a - 1)) return 0;
        if (a->name && mem_ok(a->name) && seq(a->name, name)) return a;
        a = a->next;
    }
    return 0;
}
static int node_id_of(CMapNode *node, int *out) {
    CDmAttribute *a = elem_attr((CDmElement *)node, "nodeID");
    if (!a || (a->typeFlags & S2_ATTR_TYPE_MASK) != S2_AT_INT) return 0;
    int *v = (int *)a->value;
    if (!v || !mem_ok(v)) return 0;
    *out = *v;
    return 1;
}
static int node_origin(CMapNode *node, float *out) {
    if (!mem_ok(&node->m_Origin) || !mem_ok(&node->m_Origin.z)) return 0;
    out[0] = node->m_Origin.x; out[1] = node->m_Origin.y; out[2] = node->m_Origin.z;
    return 1;
}
/* the other two transform fields (m2.18, re-props.md Q2; the offsets are the HOFF_NODE_* pins
   s2sdk.h asserts), read with the same guards; a failed read leaves `out` untouched and the
   caller emits null */
static int node_angles(CMapNode *node, float *out) {
    if (!mem_ok(&node->m_Angles) || !mem_ok(&node->m_Angles.z)) return 0;
    out[0] = node->m_Angles.x; out[1] = node->m_Angles.y; out[2] = node->m_Angles.z;
    return 1;
}
static int node_scales(CMapNode *node, float *out) {
    if (!mem_ok(&node->m_Scales) || !mem_ok(&node->m_Scales.z)) return 0;
    out[0] = node->m_Scales.x; out[1] = node->m_Scales.y; out[2] = node->m_Scales.z;
    return 1;
}
static const char *node_class(CMapNode *n) {
    if (obj_is(n, PIN_HAMMER, HV_MAPMESH_VT)) return "mesh";
    if (obj_is(n, PIN_HAMMER, HV_MAPENTITY_VT)) return "entity";
    if (obj_is(n, PIN_HAMMER, HV_MAPGROUP_VT)) return "group";
    if (obj_is(n, PIN_HAMMER, HV_MAPPREFAB_VT)) return "prefab";
    if (obj_is(n, PIN_HAMMER, HV_MAPINSTANCE_VT)) return "instance";
    if (obj_is(n, PIN_HAMMER, HV_MAPWORLD_VT)) return "world";
    return "other";
}
static int is_mesh(CMapNode *n) { return obj_is(n, PIN_HAMMER, HV_MAPMESH_VT); }

/* ---- m2.19 helpers (re-triggers.md Q1: the tie/untie recipes) ---- */

/* the stack CUtlVector<CMapNode*> shape RecordNodesAndParents and MoveToWorld read: count at +0,
   data at +8 (MoveToWorld reads nothing else). flags carries S2_UTLVEC_EXTERNAL (the upstream
   EXTERNAL_CONST_BUFFER_MARKER, 1<<30 - s2sdk.h's alias): "caller-owned, never realloc/free".
   Both callees only read count and data today (RE [V bin]), but with flags 0 a future build that
   appended to (or purged) the vector would realloc/free a stack pointer (review-b L1). */
typedef struct { int count; int pad; CMapNode **data; int alloc; int flags; } NodeVec;

/* the FGD class type (FgdClass_t::m_nType) of a CMapEntity's gamedata node: 5 == a solid (brush) class, 4 == a point
   class ([V live] 2026-10-16: trigger_once/multiple, func_bomb_target/buyzone,
   post_processing_volume -> 5; info_player_counterterrorist -> 4). 0 = unreadable or the class is
   unknown to the FGD - every read is mem_ok-guarded, so a corrupt pointer is "not a brush
   entity", never a fault. */
static int gd_type(CMapNode *ent) {
    u8 *n = (u8 *)ent;
    if (!mem_ok(n + S2_OFF_CMapGameDataNode_m_pGameDataClass) || !mem_ok(n + S2_OFF_CMapGameDataNode_m_pGameDataClass + 7)) return 0;
    u8 *gd = *(u8 **)(n + S2_OFF_CMapGameDataNode_m_pGameDataClass);
    if (!gd || !mem_ok(gd)) return 0;
    if (!mem_ok(gd + S2_OFF_FgdClass_t_m_nType) || !mem_ok(gd + S2_OFF_FgdClass_t_m_nType + 3)) return 0;
    return *(int *)(gd + S2_OFF_FgdClass_t_m_nType);
}

/* the node's parent (S2_CMapNode::m_pParent): null when the field
   or the pointer is unreadable - the tie gate then treats the mesh as under the world */
static CMapNode *node_parent(CMapNode *node) {
    if (!mem_ok(&node->m_pParent)) return 0;
    CMapNode *p = node->m_pParent;
    return (p && mem_ok(p)) ? p : 0;
}

/* the guarded child count: -1 when the field is unreadable (never guess a count) */
static int node_child_count(CMapNode *node) {
    if (!mem_ok(&node->m_Children.m_Size) || !mem_ok((u8 *)&node->m_Children.m_Size + 3)) return -1;
    return node->m_Children.m_Size;
}

typedef struct { int wantId; CMapNode *found; int visited; SB *out; int listed; int limit; const char *cls; int truncated; } GWalk;
static void geo_walk(GWalk *w, CMapNode *node, int depth) {
    if (depth > 64 || w->visited > GEO_MAX_NODES || w->found) return;
    if (!mem_ok((u8 *)&node->m_Children.m_pMemory + 8)) return;
    int n = node->m_Children.m_Size;
    u32 *hs = CMapNode_ChildHandles(node);
    if (n <= 0 || n > 1000000 || !hs || !mem_ok(hs) || !mem_ok(hs + n - 1)) return;
    for (int i = 0; i < n && !w->found; i++) {
        CDmElement *ce = dm_element(hs[i]);
        if (!is_map_node(ce)) continue;
        CMapNode *c = (CMapNode *)ce;
        w->visited++;
        int id = -1;
        node_id_of(c, &id);
        if (w->wantId >= 0) {
            if (id == w->wantId) { w->found = c; return; }
        } else if (w->out) {
            const char *cl = node_class(c);
            if (!w->cls || seq(w->cls, "all") || seq(w->cls, cl)) {
                if (w->listed >= w->limit) w->truncated = 1;
                else {
                    float o[3] = { 0, 0, 0 };
                    node_origin(c, o);
                    if (w->listed) sb_add(w->out, ",");
                    sb_add(w->out, "{\"nodeID\":"); sb_addi(w->out, id);
                    sb_add(w->out, ",\"class\":\""); sb_add(w->out, cl);
                    sb_add(w->out, "\",\"origin\":"); sb_addf3(w->out, o);
                    if (cl[0] == 'm' && cl[1] == 'e') {
                        CHammerPolyMesh *hm = CMapMesh_PolyMesh((CMapMesh *)c);
                        sb_add(w->out, ",\"vertexSlots\":"); sb_addi(w->out, hm->vertices.count);
                        sb_add(w->out, ",\"faceSlots\":"); sb_addi(w->out, hm->faces.count);
                    }
                    sb_add(w->out, "}");
                    w->listed++;
                }
            }
        }
        geo_walk(w, c, depth + 1);
    }
}
static CMapNode *geo_world(CMapDoc *doc) {
    CMapNode *wn = doc->m_pWorld;
    return (wn && mem_ok(wn)) ? wn : 0;
}
static CMapNode *geo_find(CMapDoc *doc, int id) {
    CMapNode *wn = geo_world(doc);
    if (!wn) return 0;
    GWalk w; w.wantId = id; w.found = 0; w.visited = 0; w.out = 0; w.listed = 0; w.limit = 0; w.cls = 0; w.truncated = 0;
    geo_walk(&w, wn, 0);
    return w.found;
}

/* node ids are i32 on the wire (the Rust schemas say maximum 2147483647). Anything bigger would
   alias another node through the (int) cast (review opus #5 / ds #1: nodeID 4294967326 read and
   WROTE node 30), so it is InvalidParams like a negative id. Decimal on purpose: the m2.15 diff
   adds no 0x literal (bridge/test_entities.py --static). */
#define NODEID_MAX 2147483647

/* nodeID -> a checked entity node (m2.15): -32602 for an id outside 0..NODEID_MAX or for a
   non-entity node (world/mesh/group/prefab/instance), -32022 when the live document has no such
   nodeID. Shared by entity.get and the entity.setKeyValues nodeID form. */
static CMapNode *geo_entity_node(CMapDoc *doc, long long id, OpErr *e) {
    if (id < 0 || id > NODEID_MAX) {
        op_err(e, -32602, "InvalidParams", "nodeID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    CMapNode *n = geo_find(doc, (int)id);
    if (!n) { op_err(e, -32022, "NodeNotFound", "no node with this nodeID in the live document"); return 0; }
    if (!obj_is(n, PIN_HAMMER, HV_MAPENTITY_VT)) { op_err(e, -32602, "InvalidParams", "not an entity"); return 0; }
    return n;
}

/* ---- entity.get / entity.find (m2.15 §3.1/§3.2) ---- */
#define ENTITY_MAX_ATTRS 512
#define ENTITY_STR_CAP 4096
#define ENTITY_FIND_MAX 2000
#define ENTITY_FIND_DEFAULT 500

static int entity_type_supported(int atype) {
    return atype == S2_AT_STRING || atype == S2_AT_INT || atype == S2_AT_FLOAT || atype == S2_AT_BOOL;
}

/* one keyValues value, read through the same guards as elem_attr: a String's storage holds a
   CUtlSymbolLarge (the char* dm_intern/SetValueString write), 2/3/4 are the typed setters' value
   storages; every other type is null (the name also lands in `unsupported`). A String longer than
   ENTITY_STR_CAP is null and *cut is set: the caller lists the name in `truncatedValues`, so no
   caller ever re-applies a prefix as if it were the value (review opus #6/#9). */
static void entity_value_json(CDmAttribute *a, SB *out, int *cut) {
    static char sbuf[ENTITY_STR_CAP];
    int atype = (int)(a->typeFlags & S2_ATTR_TYPE_MASK);
    if (atype == S2_AT_STRING) {
        const char *s = attr_cstr(a);
        int got = s ? probe_str(s, sbuf, sizeof sbuf) : 0;
        if (got == 1) sb_addjson(out, sbuf);
        else { sb_add(out, "null"); if (got < 0) *cut = 1; }
    } else if (atype == S2_AT_INT) {
        int *v = (int *)a->value;
        if (v && mem_ok(v) && mem_ok((u8 *)v + 3)) sb_addi(out, *v); else sb_add(out, "null");
    } else if (atype == S2_AT_FLOAT) {
        float *v = (float *)a->value;
        if (v && mem_ok(v) && mem_ok((u8 *)v + 3)) sb_addf(out, *v); else sb_add(out, "null");
    } else if (atype == S2_AT_BOOL) {
        int *v = (int *)a->value;
        if (v && mem_ok(v) && mem_ok((u8 *)v + 3)) sb_add(out, *v ? "true" : "false");
        else sb_add(out, "null");
    } else sb_add(out, "null");
}

/* the child's `name` attribute as text; out is "" and the result 0 when it has none */
static int child_string(CDmElement *child, const char *name, char *out, u32 cap) {
    out[0] = 0;
    S2_CUtlSymbolLarge sym = dm_intern(name);
    if (!sym.m_pString) return 0;
    CDmAttribute *a = g_hfn.FindAttribute(child, &sym);
    const char *s = a ? attr_cstr(a) : 0;
    return (s && probe_str(s, out, cap)) ? 1 : 0;
}

/* ---- entity I/O: the live connection vector (m2.18, re-props.md Q3) ----
   The connections a running Hammer holds are this CUtlVector on the node
   (S2_CMapEntityIONode::m_Connections: m_Size, m_pMemory), NOT the DME `connectionsData`
   attribute: that one is materialized from the vector at save and consumed (then emptied) at
   load, so writing it live would be overwritten by the next save. One element is the 80-byte
   S2ConnectionSrc layout (s2sdk.h), which is also what AddConnection copies.
   Two bounds, deliberately different:
     - CONN_VEC_SANE is the largest count worth *reading at all*: RemoveConnection's id is
       `index << 16`, so an index >= 65536 cannot be addressed and a bigger count is a corrupt
       count, not a long list (fail closed: the op refuses, entity.get answers null);
     - RM_INDEX_MAX (512) is an *output* bound: a reply lists at most 512 entries (entity.get adds
       `connectionsTruncated:true` when more exist) and entity.addConnections refuses an add that
       would cross it *before* it opens a group, so the bridge can never build a state it cannot
       read back or repair. */
#define CONN_VEC_SANE 65535
static int conn_vec(CMapNode *node, int *count, u8 **data) {
    S2_CMapEntityIONode *io = (S2_CMapEntityIONode *)node;
    *count = 0; *data = 0;
    if (!mem_ok(&io->m_Connections.m_Size) || !mem_ok((u8 *)&io->m_Connections.m_Size + 3)) return 0;
    int c = io->m_Connections.m_Size;
    if (c < 0 || c > CONN_VEC_SANE) return 0;                /* never walk a corrupt count */
    if (!mem_ok(&io->m_Connections.m_pMemory) || !mem_ok((u8 *)&io->m_Connections.m_pMemory + 7)) return 0;
    u8 *d = (u8 *)io->m_Connections.m_pMemory;
    if (c > 0 && !d) return 0;
    *count = c; *data = d;
    return 1;
}
static const S2ConnectionSrc *conn_at(const u8 *data, int i) {
    return (const S2ConnectionSrc *)(const void *)(data + sizeof(S2ConnectionSrc) * (u64)i);
}
static int conn_entry_ok(const u8 *data, int i) {
    const u8 *e = (const u8 *)conn_at(data, i);
    return mem_ok(e) && mem_ok(e + sizeof(S2ConnectionSrc) - 1);
}
/* one connection string member (a CUtlString, i.e. its char*). Returns 1 when the value did not
   fit the buffer (the emitted text is the valid-UTF-8 prefix), 0 when it was copied whole; a null
   or unreadable pointer is 0 and emits "" (absent, which is not the same as cut). A cut is
   reported per entry (`truncated`) so no client re-adds a shortened name by accident. */
static int sb_add_conn_str(SB *out, const char *s) {
    char buf[256];
    int got = (s && mem_ok(s)) ? probe_str(s, buf, sizeof buf) : 0;
    if (got != 0) sb_addjson(out, buf);
    else sb_addjson(out, "");
    return got < 0;
}
/* the fields of one connection, named exactly as the .vmap reader names the saved
   DmeConnectionData (outputName/targetType/targetName/inputName/overrideParam/delay/timesToFire),
   so the live shape and the file shape compare field by field */
static void sb_add_conn_fields(SB *out, const S2ConnectionSrc *c) {
    int cut = 0;
    sb_add(out, "\"outputName\":"); cut |= sb_add_conn_str(out, c->output);
    sb_add(out, ",\"targetType\":"); sb_addi(out, c->targetType);
    sb_add(out, ",\"targetName\":"); cut |= sb_add_conn_str(out, c->target);
    sb_add(out, ",\"inputName\":"); cut |= sb_add_conn_str(out, c->input);
    sb_add(out, ",\"overrideParam\":"); cut |= sb_add_conn_str(out, c->param);
    sb_add(out, ",\"delay\":"); sb_addf(out, c->delay);
    sb_add(out, ",\"timesToFire\":"); sb_addi(out, c->timesToFire);
    if (cut) sb_add(out, ",\"truncated\":true");
}
/* the live vector as JSON. `null` for a node that is not a CMapEntity (nothing there to report)
   and for an unreadable vector (a read failure is not "no connections"); a readable one with no
   entries is `[]`. The walk stops at the first unreadable element rather than guessing.
   Returns the vector's real count when the 512-entry output cap cut the list (the caller reports
   `connectionsTruncated` + `connectionsTotal` - never a silent cut), else 0. */
static int conn_list_json(CMapNode *node, SB *out) {
    if (!obj_is(node, PIN_HAMMER, HV_MAPENTITY_VT)) { sb_add(out, "null"); return 0; }
    int count = 0; u8 *data = 0;
    if (!conn_vec(node, &count, &data)) { sb_add(out, "null"); return 0; }
    sb_add(out, "[");
    int n = 0;
    for (int i = 0; i < count && n < RM_INDEX_MAX; i++) {
        if (!conn_entry_ok(data, i)) break;
        if (n) sb_add(out, ",");
        sb_add(out, "{");
        sb_add_conn_fields(out, conn_at(data, i));
        sb_add(out, "}");
        n++;
    }
    sb_add(out, "]");
    return (n == RM_INDEX_MAX && count > n) ? count : 0;
}
/* one connection string member for the removeConnections `match` filter. Returns 1 only when a
   whole, non-empty value was copied: a cut string (probe_str -1) and an unreadable one are "does
   not match" - a live name that did not fit can never equal a filter the 255-byte gate accepted,
   so treating it as a match would remove a connection the caller did not name */
static int conn_copy_str(char *dst, u32 cap, const char *s) {
    dst[0] = 0;
    if (!s || !mem_ok(s)) return 0;
    return probe_str(s, dst, cap) == 1;
}

/* the children of one node, ONE level (m2.19, plan-triggers.md §5): [{"nodeID":id,"class":"mesh"},
   ...] - the same class strings node.list emits. `null` when the child array is unreadable (a read
   failure is not "no children"), `[]` when the node has none; non-map-node handles are skipped
   like geo_walk skips them. At most CHILDREN_LIST_MAX entries are listed; the return value is the
   real map-node count when that cap cut the list (the caller reports childrenTruncated +
   childrenTotal, the same shape the connections vector lists under), else 0. */
#define CHILDREN_LIST_MAX 1024
static int entity_children_json(CMapNode *node, SB *out) {
    int n = node_child_count(node);
    u32 *hs = 0;
    if (n < 0 || n > 1000000) { sb_add(out, "null"); return 0; }   /* a corrupt count is not a list - the range check runs BEFORE any array probe (review-b L10: the old order mem_ok(hs + n - 1)'d a wild address first) */
    if (n > 0) {
        if (!mem_ok(&node->m_Children.m_pMemory)) { sb_add(out, "null"); return 0; }
        hs = CMapNode_ChildHandles(node);
        if (!hs || !mem_ok(hs) || !mem_ok(hs + n - 1)) { sb_add(out, "null"); return 0; }
    }
    sb_add(out, "[");
    int listed = 0, total = 0;
    for (int i = 0; i < n; i++) {
        CDmElement *ce = dm_element(hs[i]);
        if (!ce || !mem_ok(ce) || !is_map_node(ce)) continue;
        CMapNode *c = (CMapNode *)ce;
        total++;
        if (listed >= CHILDREN_LIST_MAX) continue;
        int id = -1;
        node_id_of(c, &id);
        if (listed) sb_add(out, ",");
        sb_add(out, "{\"nodeID\":");
        if (id >= 0) sb_addi(out, id); else sb_add(out, "null");   /* review-b L6: an unreadable id is null, never -1 (a reader would address node -1) */
        sb_add(out, ",\"class\":\""); sb_add(out, node_class(c)); sb_add(out, "\"}");
        listed++;
    }
    sb_add(out, "]");
    return (listed == CHILDREN_LIST_MAX && total > listed) ? total : 0;
}

static int op_entity_get(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    long long id = -1;
    if (!params || !json_find_top(params, "nodeID")) {
        op_err(e, -32602, "InvalidParams", "nodeID required");
        return 0;
    }
    if (!json_int_top(params, "nodeID", &id)) {   /* no fraction/exponent (review opus #5 / ds #1) */
        op_err(e, -32602, "InvalidParams", "nodeID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    CMapNode *node = geo_entity_node(doc, id, e);
    if (!node) return 0;
    CDmElement *child = g_hfn.GetEntityProperties(node);
    if (!child || !mem_ok(child)) { op_err(e, -32603, "InternalError", "entity_properties missing"); return 0; }

    static char cbuf[1024], tbuf[1024];
    child_string(child, "classname", cbuf, sizeof cbuf);
    child_string(child, "targetname", tbuf, sizeof tbuf);
    float o[3] = { 0, 0, 0 }, an[3] = { 0, 0, 0 }, sc[3] = { 0, 0, 0 };
    node_origin(node, o);
    int has_angles = node_angles(node, an);
    /* m2.18: the third transform field (re-props.md Q2). It is a C++ field, not a DME attribute,
       so `keyValues` never carried it and a client had no read-only way to see an entity's
       scales. Same guards as origin/angles: an unreadable field is null. */
    int has_scales = node_scales(node, sc);

    sb_add(out, "{\"nodeID\":"); sb_addi(out, id);
    sb_add(out, ",\"classname\":"); sb_addjson(out, cbuf);
    sb_add(out, ",\"targetname\":"); sb_addjson(out, tbuf);
    sb_add(out, ",\"origin\":"); sb_addf3(out, o);
    sb_add(out, ",\"angles\":");
    if (has_angles) sb_addf3(out, an); else sb_add(out, "null");
    sb_add(out, ",\"scales\":");
    if (has_scales) sb_addf3(out, sc); else sb_add(out, "null");
    sb_add(out, ",\"keyValues\":{");

    /* walk the child's attributes with elem_attr's guards; `id` is synthetic (the node's own id) */
    static char ubuf[1024], vbuf[1024];
    SB us, vs;
    sb_init(&us, ubuf, sizeof ubuf);
    sb_init(&vs, vbuf, sizeof vbuf);
    CDmAttribute *a = child->attrs;
    int kvcount = 0, first = 1, truncated = 0, unsup = 0, vfirst = 1;
    for (int guard = 0; a && guard < 4096; guard++) {
        if (!mem_ok(a) || !mem_ok((u8 *)a + sizeof *a - 1)) break;
        static char nb[512];
        const char *nm = (a->name && mem_ok(a->name) && probe_str(a->name, nb, sizeof nb)) ? nb : 0;
        if (nm && !seq(nm, "id") && !seq(nm, "name")) {   /* DME element id/name, not keyvalues (the .vmap reader drops them too) */
            if (kvcount >= ENTITY_MAX_ATTRS) { truncated = 1; break; }
            if (!first) sb_add(out, ",");
            first = 0;
            sb_addjson(out, nm);
            sb_add(out, ":");
            int atype = (int)(a->typeFlags & S2_ATTR_TYPE_MASK);
            if (!entity_type_supported(atype) && unsup < 64) {
                if (unsup) sb_add(&us, ",");
                sb_addjson(&us, nm);
                unsup++;
            }
            int cut = 0;
            entity_value_json(a, out, &cut);
            if (cut) { if (!vfirst) sb_add(&vs, ","); vfirst = 0; sb_addjson(&vs, nm); }
            kvcount++;
        }
        a = a->next;
    }
    sb_add(out, "},\"keyValueCount\":"); sb_addi(out, kvcount);
    sb_add(out, ",\"unsupported\":["); sb_add(out, ubuf); sb_add(out, "]");
    /* the names of the values whose text did not fit: their keyValues entry is null and must not
       be treated as the value (review opus #6/#9). `truncated` stays the attribute-walk cap. */
    sb_add(out, ",\"truncatedValues\":["); sb_add(out, vbuf); sb_add(out, "]");
    if (truncated) sb_add(out, ",\"truncated\":true");
    /* m2.18: the LIVE I/O vector (re-props.md Q3) - it used to be a hard-coded null. Same field
       names as the .vmap reader's connectionsData view, so the live reply and the saved file can
       be compared entry by entry. */
    sb_add(out, ",\"connections\":");
    int connTotal = conn_list_json(node, out);
    if (connTotal > 0) {
        sb_add(out, ",\"connectionsTruncated\":true,\"connectionsTotal\":"); sb_addi(out, connTotal);
    }
    /* m2.19: the children, one level (re-triggers.md Q3 / plan-triggers.md §5) - the read the
       trigger tools need (the meshes of a brush entity). `null` = the array is unreadable, never
       "no children". */
    sb_add(out, ",\"children\":");
    int childrenTotal = entity_children_json(node, out);
    if (childrenTotal > 0) {
        sb_add(out, ",\"childrenTruncated\":true,\"childrenTotal\":"); sb_addi(out, childrenTotal);
    }
    sb_add(out, ",\"source\":\"live\"}");
    /* both name lists are part of the reply: their overflow must fail the whole op instead of
       emitting cut JSON (review ds #5) */
    if (us.ovf || vs.ovf) out->ovf = 1;
    return 1;
}

/* ---- entity.addConnections (m2.18, re-props.md Q3) ----
   Everything is validated before the undo group opens: 1..64 entries, output/target/input present
   and non-empty, every string at most 255 bytes and control-free, delay finite in [0, 3600],
   timesToFire -1 (always) or >= 1 (0 is not a legal count). One group "MCP: add connections" with
   one RecordNodeTree, then per entry the exact "Add New Output" src recipe: an 80-byte
   S2ConnectionSrc with targetType 7 and a KV3-initialized paramMap (HFN_KV3_INIT / HFN_KV3_FREE
   around the call), AddConnection, and the appended index read back out of *outId (index << 16).
   The reply's `added` entries are read back from the live vector, so a client sees what the
   document actually holds now. */
static int op_entity_add_connections(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    long long id = -1;
    if (!params || !json_find_top(params, "nodeID")) {
        op_err(e, -32602, "InvalidParams", "nodeID required");
        return 0;
    }
    if (!json_int_top(params, "nodeID", &id) || id < 0 || id > NODEID_MAX) {
        op_err(e, -32602, "InvalidParams", "nodeID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    static ConnSpec specs[CONN_MAX];
    static u8 spec_landed[CONN_MAX];   /* per wire entry: 1 = the vector confirmed it */
    static int spec_idx[CONN_MAX];     /* the live index the call reported, -1 when it reported none */
    int n = 0;
    const char *cp = json_find_top(params, "connections");
    if (!cp || *cp != '[') {
        op_err(e, -32602, "InvalidParams", "connections: [{output, target, input, param?, delay?, timesToFire?}] required");
        return 0;
    }
    if (!conn_array_parse(cp, specs, CONN_MAX, &n)) {
        /* short on purpose: `data` is a 192-byte OpErr buffer and a longer reason would be cut */
        op_err(e, -32602, "InvalidParams",
               "connections: 1..64 entries of {output, target, input, param?, delay?, timesToFire?}");
        return 0;
    }
    CMapNode *node = geo_entity_node(doc, id, e);      /* -32602 not an entity, -32022 not found */
    if (!node) return 0;
    if (g_hfn.NodeIsLocked(node, 0)) {
        op_err(e, -32018, "DocumentNotEditable", "node is locked");
        return 0;
    }
    /* the entity's current vector: needed for the cap refusal below AND as the baseline the
       per-entry read-back is checked against (a call that appended anything must move the count
       by exactly one). Both are before undo_open: a refusal leaves no group behind. */
    int count0 = 0; u8 *data0 = 0;
    if (!conn_vec(node, &count0, &data0)) {
        op_err(e, -32603, "InternalError", "connection vector unreadable");
        return 0;
    }
    if (count0 + n > RM_INDEX_MAX) {
        /* never build a vector this bridge could not read back or repair: entity.get lists at
           most RM_INDEX_MAX entries and removeConnections carries at most that many indices */
        op_err(e, -32602, "InvalidParams",
               "connections: this entity has room for fewer than the requested entries (max 512 live)");
        return 0;
    }
    CMapDocHistory *hist = undo_open(doc, "MCP: add connections");
    g_hfn.RecordNodeTree(hist, CMapNode_Undoable(node), 1);
    /* worst-case reply per entry: four 255-byte names, each char a 2-byte JSON escape, plus the
       index and the numbers (~2.2 KB) - 64 entries must fit, or the op would answer -32603 AFTER
       the connections were added (the overflow check runs in geo_dispatch, past the writes) */
    static char abuf[CONN_MAX * 2304];
    SB ab; sb_init(&ab, abuf, sizeof abuf);
    int added = 0;
    for (int i = 0; i < n; i++) {
        S2ConnectionSrc src;
        for (u32 k = 0; k < sizeof src; k++) ((u8 *)&src)[k] = 0;
        /* src.source (the first member) stays null: it is not a saved attribute (load fills it
           with the owner's targetname), so nothing a caller could verify rides on it */
        src.output = specs[i].output;
        src.target = specs[i].target;
        src.input = specs[i].input;
        src.param = specs[i].param;
        src.targetType = 7;                 /* the UI default and every saved Valve connection */
        g_hfn.Kv3Init(&src.paramMap, 1, 8); /* the copy reads it: must be an initialized KV3 */
        src.delay = specs[i].delay;
        src.timesToFire = specs[i].timesToFire;
        u32 cid = 4294967295u;              /* decimal: the m2.18 diff adds no 0x literal */
        g_hfn.AddConnection(node, &cid, &src);
        g_hfn.Kv3Free(&src.paramMap);       /* AddConnection copied it (strlen + SetDirect + KV3 copy) */
        /* the read-back: *outId must be `index << 16` (low word 0) AND the count must have grown
           by exactly one, so a wrong call shape cannot make the bridge report a different entry
           as the added one */
        int count1 = 0; u8 *data1 = 0;
        int landed = (cid != 4294967295u) && ((cid & 65535u) == 0u);
        int idx = landed ? (int)(cid >> 16) : -1;
        int readable = conn_vec(node, &count1, &data1);
        int ok = landed && readable && count1 == count0 + 1 && idx == count0
              && conn_entry_ok(data1, idx);
        if (i) sb_add(&ab, ",");
        spec_landed[i] = (u8)(ok ? 1 : 0);
        spec_idx[i] = landed ? idx : -1;
        if (ok) {
            sb_add(&ab, "{\"index\":"); sb_addi(&ab, idx); sb_add(&ab, ",");
            sb_add_conn_fields(&ab, conn_at(data1, idx));
            sb_add(&ab, "}");
            added++;
        } else {
            sb_add(&ab, "{\"readback\":false");
            if (landed) { sb_add(&ab, ",\"index\":"); sb_addi(&ab, idx); }
            sb_add(&ab, "}");
        }
        /* resync the baseline from the vector itself: a call that did not append must not shift
           the next entry's expected index */
        if (readable) count0 = count1;
    }
    if (ab.ovf) out->ovf = 1;
    g_hfn.DocSetModified(doc, doc);
    sb_add(out, "{\"nodeID\":"); sb_addi(out, id);
    sb_add(out, ",\"added\":["); sb_add(out, abuf); sb_add(out, "]");
    /* count == len(added): the entries the vector confirmed, never the calls attempted. An entry
       with "readback":false was handed to AddConnection but not verified - treat the call as
       maybe-executed (read entity.get) instead of retrying it.
       `landed` is per WIRE ENTRY (same order and count as the request's `connections`, so
       added[i]/landed[i] are the same entry): {index: <live index or null>, landed: true|false}.
       A confirmed entry's index equals its added[] index; a maybe-executed one names whatever
       index the call reported (or null when the call reported none). Additive field (W2 #3). */
    sb_add(out, ",\"count\":"); sb_addi(out, added);
    sb_add(out, ",\"landed\":[");
    for (int i = 0; i < n; i++) {
        if (i) sb_add(out, ",");
        sb_add(out, "{\"index\":");
        if (spec_idx[i] >= 0) sb_addi(out, spec_idx[i]); else sb_add(out, "null");
        sb_add(out, ",\"landed\":"); sb_add(out, spec_landed[i] ? "true" : "false");
        sb_add(out, "}");
    }
    sb_add(out, "]");
    sb_add(out, ",\"undoLabel\":\"MCP: add connections\"}");
    return 1;
}

/* ---- entity.removeConnections (m2.18, re-props.md Q3) ----
   Exactly one of `indices` / `match` resolves to a set of live indices BEFORE the undo group
   opens: an out-of-range or duplicate index, a bad filter and a filter that matches nothing are
   all refused with nothing opened (the last one as -32026 ConnectionNotFound). The removals then
   run in DESCENDING index order, because RemoveConnection shifts the entries after the removed
   one. A 0 return after the group is open is an InternalError whose data says the earlier
   removals may already have run (verify, do not retry). */
static int op_entity_remove_connections(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    long long id = -1;
    if (!params || !json_find_top(params, "nodeID")) {
        op_err(e, -32602, "InvalidParams", "nodeID required");
        return 0;
    }
    if (!json_int_top(params, "nodeID", &id) || id < 0 || id > NODEID_MAX) {
        op_err(e, -32602, "InvalidParams", "nodeID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    const char *ip = json_find_top(params, "indices");
    const char *mp = json_find_top(params, "match");
    if (!rm_mode_ok(ip != 0, mp != 0)) {
        op_err(e, -32602, "InvalidParams", "exactly one of indices / match is required");
        return 0;
    }
    static int idx[RM_INDEX_MAX];
    int n = 0;
    char mo[256], mt[256], mi[256];
    mo[0] = mt[0] = mi[0] = 0;
    int has_mo = 0, has_mt = 0, has_mi = 0;
    if (ip) {
        if (!rm_indices_parse(ip, idx, RM_INDEX_MAX, &n)) {
            op_err(e, -32602, "InvalidParams",
                   "indices: 1..512 distinct non-negative ints up to 2147483647");
            return 0;
        }
    } else {
        if (*mp != '{') { op_err(e, -32602, "InvalidParams", "match: {output?, target?, input?} required"); return 0; }
        /* the filter goes through the same escape-aware reader the connections array uses
           (kv_next): json_string_top would decode `\n` to `n` and silently cut a value past 255
           bytes - a filter that removes a connection must be the caller's exact bytes */
        const char *p = mp + 1;
        for (;;) {
            char name[64], sval[CONN_STR_MAX + 1], nval[96];
            int kind = 0, over = 0;
            long long ival = 0;
            double fval = 0;
            const char *before = p;      /* kv_next provably advances */
            p = kv_next(p, name, sizeof name, &kind, sval, sizeof sval, &ival, &fval,
                        nval, sizeof nval, &over);
            if (!p || p == before) { op_err(e, -32602, "InvalidParams", "match: malformed"); return 0; }
            if (over || kind != 5 || !conn_text_ok(sval, 1)) {
                op_err(e, -32602, "InvalidParams",
                       "match: output/target/input are 1..255 bytes without control characters");
                return 0;
            }
            if (prm_seq(name, "output") && !has_mo) { cpy(mo, sval, sizeof mo); has_mo = 1; }
            else if (prm_seq(name, "target") && !has_mt) { cpy(mt, sval, sizeof mt); has_mt = 1; }
            else if (prm_seq(name, "input") && !has_mi) { cpy(mi, sval, sizeof mi); has_mi = 1; }
            else { op_err(e, -32602, "InvalidParams", "match: only output, target and input"); return 0; }
            while (*p == ' ' || *p == '\n' || *p == '\t' || *p == '\r') p++;
            if (*p == ',') continue;
            if (*p == '}') break;
            op_err(e, -32602, "InvalidParams", "match: malformed");
            return 0;
        }
        if (!has_mo && !has_mt && !has_mi) {
            op_err(e, -32602, "InvalidParams", "match: at least one of output, target, input is required");
            return 0;
        }
    }
    CMapNode *node = geo_entity_node(doc, id, e);
    if (!node) return 0;
    if (g_hfn.NodeIsLocked(node, 0)) {
        op_err(e, -32018, "DocumentNotEditable", "node is locked");
        return 0;
    }
    int count = 0; u8 *data = 0;
    if (!conn_vec(node, &count, &data)) {
        op_err(e, -32603, "InternalError", "connection vector unreadable");
        return 0;
    }
    if (ip) {
        for (int i = 0; i < n; i++) {
            if (idx[i] >= count) {
                char why[128]; SB w; sb_init(&w, why, sizeof why);
                sb_add(&w, "indices: "); sb_addi(&w, idx[i]); sb_add(&w, " is out of range (the entity has ");
                sb_addi(&w, count); sb_add(&w, " connection(s))");
                op_err(e, -32602, "InvalidParams", why);
                return 0;
            }
        }
    } else {
        n = 0;
        for (int i = 0; i < count; i++) {
            if (!conn_entry_ok(data, i)) continue;      /* an unreadable entry never matches */
            const S2ConnectionSrc *c = conn_at(data, i);
            char so[256], st[256], si[256];
            /* a field that did not copy whole (cut/unreadable) can never equal a 1..255-byte
               filter: it is "not this one", never a prefix match */
            int ok_o = conn_copy_str(so, sizeof so, c->output);
            int ok_t = conn_copy_str(st, sizeof st, c->target);
            int ok_i = conn_copy_str(si, sizeof si, c->input);
            if ((has_mo && (!ok_o || !seq(so, mo))) || (has_mt && (!ok_t || !seq(st, mt)))
                || (has_mi && (!ok_i || !seq(si, mi)))) continue;
            if (n >= RM_INDEX_MAX) {
                /* more than a request may remove: refuse, nothing opened */
                op_err(e, -32602, "InvalidParams", "match: more than 512 connections match");
                return 0;
            }
            idx[n++] = i;
        }
        if (!n) {
            op_err(e, -32026, "ConnectionNotFound", "no live connection matches the match filter");
            return 0;
        }
    }
    rm_sort_desc(idx, n);                               /* highest first: RemoveConnection shifts */
    CMapDocHistory *hist = undo_open(doc, "MCP: remove connections");
    g_hfn.RecordNodeTree(hist, CMapNode_Undoable(node), 1);
    for (int i = 0; i < n; i++) {
        if (!g_hfn.RemoveConnection(node, (u32)idx[i] << 16)) {
            op_err_maybe(e, -32603, "InternalError",
                         "RemoveConnection refused an index; the removals before it in this group "
                         "already ran - verify with entity.get, do not retry");
            return 0;
        }
    }
    g_hfn.DocSetModified(doc, doc);
    /* the reply lists the removed indices ascending (the order they were walked in is the
       descending one above; a client compares sets, not order) */
    sb_add(out, "{\"nodeID\":"); sb_addi(out, id);
    sb_add(out, ",\"removed\":[");
    for (int i = n - 1; i >= 0; i--) { if (i != n - 1) sb_add(out, ","); sb_addi(out, idx[i]); }
    sb_add(out, "],\"count\":"); sb_addi(out, n);
    /* `count` is the number REMOVED (it always was, despite the name) and is kept for
       compatibility; `remaining` is the vector size after the group - read count was taken
       before it opened. A client that wants "how many are left" must read `remaining`
       (review W2 #1: reading `count` as the remainder under-reports by 2x). */
    sb_add(out, ",\"remaining\":"); sb_addi(out, count - n);
    sb_add(out, ",\"undoLabel\":\"MCP: remove connections\"}");
    return 1;
}

/* ---- entity.tieMeshes (m2.19, re-triggers.md Q1 + plan-triggers.md §5) ----
   The Hammer-faithful tie, reimplemented (CMapDoc::TieToEntity itself re-collects the UI
   selection and would hit a modal on an empty one - the bridge cannot call it with arbitrary
   nodes): one undo group "MCP: tie meshes to entity"; the entity is created with CreateNodeByName
   at the FIRST mesh's origin/angles (Hammer's rule, [V live] 2026-10-16: not the bounds centre)
   or the entityID's existing brush entity is reused; keyValues go through the entity.create
   SetKeyValue path; then RecordNodesAndParents - which must run BEFORE the AddChild calls, since
   it records the old parents (non-recursive) so the undo restores both child lists - and one
   AddChild (vtable slot 57) per mesh. AddChild never transforms: node transforms are world-space,
   so a tied mesh keeps its world geometry ([V live]). An old brush entity the tie empties is
   deleted in the same group (Hammer's RemoveNode(old, 3) rule) and reported in
   removedEmptyEntities. Every refusal below happens BEFORE undo_open (the review-13 rule) except
   the two documented in-band failures: the defensive gdType check (the class gate lives in the
   Rust FGD, but the bridge re-checks after the create - the created node is deleted in the same
   group, leaving an empty group behind, undoDepth + 1) and an AddChild refusal after earlier ties
   in the group already ran (maybeExecuted, like removeConnections). */
#define TIE_MAX_MESHES 64

/* op_err whose data names the node the refusal is about (plan-triggers.md §5: NodeNotFound
   {nodeID}, "not a mesh" {nodeID}, "already tied" {nodeID, parentNodeID} - the Rust tools map
   their NotAMesh / MeshAlreadyTied / NodeNotFound errors from these fields) */
static void tie_err_node(OpErr *e, int code, const char *name, const char *reason,
                         int nodeID, int parentNodeID) {
    e->code = code;
    e->name = name;
    SB db;
    sb_init(&db, e->data, sizeof e->data);
    sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason ? reason : "?");
    sb_add(&db, ",\"nodeID\":"); sb_addi(&db, nodeID);
    if (parentNodeID >= 0) { sb_add(&db, ",\"parentNodeID\":"); sb_addi(&db, parentNodeID); }
    sb_add(&db, "}");
}

/* the error for a failure that happened AFTER the tie's undo group opened but left it a NO-OP
   (FIX-B live L3 / review-b M3): the group was pushed (undoDepth +1, one undo.undo with this
   label pops it) but nothing of the caller's intent executed - the created node was deleted
   again (point class) or never created (CreateNodeByName failed). maybeExecuted is deliberately
   ABSENT: the Rust side must count the refusal, not mark an edit (live_common marks an edit only
   on maybeExecuted), while undoGroupOpened/undoLabel tell it one undo restores the depth. */
static void tie_err_undo_group(OpErr *e, int code, const char *name, const char *reason,
                               const char *label) {
    e->code = code;
    e->name = name;
    SB db;
    sb_init(&db, e->data, sizeof e->data);
    sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason ? reason : "?");
    sb_add(&db, ",\"undoGroupOpened\":true,\"undoLabel\":"); sb_addjson(&db, label ? label : "?");
    sb_add(&db, "}");
}

/* the id-list cap inside the AddChild-refusal data (review-b M6): e->data is a fixed 512-byte
   buffer, so the progress lists stop at 10 entries each - the full reply of a successful tie
   reports every id, this only has to name enough for the caller to see the group was not empty */
#define TIE_ERR_LIST_CAP 10

/* review-b M5: is it safe to reparent `node` (a mesh, or the entityID target)? AddChild moves
   nodes between containers with no file check, and geo_find DOES descend into CMapPrefab/
   CMapInstance children, so a nodeIDs entry picked from node.list can name a node owned by a
   referenced sub-map; moving it into a host-map entity would desync the prefab's document and
   the host map (Hammer's UI never does this - instance contents are not individually
   selectable). No existing bridge helper tells prefab ownership (entity.create /
   node.setTransform / mesh.* never walk ancestors), so this is the parent walk the WP
   prescribes: up via S2_OFF_CMapNode_m_pParent (mem_ok each step, bounded at 64 like geo_walk), every
   ancestor must be the document's world root (compared by pointer, so a sub-map's own
   CMapWorld does not pass), a CMapGroup, or a CMapEntity. A parent that reads as null stops the
   walk as OK - the same under-world status quo the tie gate already gives such a node; a
   prefab/instance ON the chain is the positive danger this refuses. */
static int tie_ancestors_ok(CMapDoc *doc, CMapNode *node) {
    CMapNode *n = node;
    CMapNode *world = geo_world(doc);
    for (int d = 0; d < 64; d++) {
        CMapNode *par = node_parent(n);
        if (!par) return 1;
        if (par == world) return 1;
        if (obj_is(par, PIN_HAMMER, HV_MAPGROUP_VT) || obj_is(par, PIN_HAMMER, HV_MAPENTITY_VT)) {
            n = par;
            continue;
        }
        return 0;                            /* prefab / instance / anything else: refuse */
    }
    return 0;                                /* deeper than the walk bound: refuse, not guess */
}

/* the dry pass over a tie's keyValues object, BEFORE the undo group opens (review-b L2/L3):
   apply_keyvalues would silently skip the keys after a malformed value ({"a":} stops kv_next
   early and the tie still succeeds), and a classname/origin/angles key would run SetKeyValue
   AFTER the solid-class gate and turn the new entity into a point class or move it off the
   first mesh's origin (SetKeyValue matches case-insensitively, so the compare is too). Returns
   1 when the object is clean, 0 with e set when refused. */
static int tie_kv_prescan(const char *vp, OpErr *e) {
    static char name[96], sval[KV_VALUE_MAX + 1], nval[96];
    int kind = 0;
    long long ival = 0;
    double fval = 0;
    const char *p = vp + 1;
    for (int guard = 0; guard < 4096; guard++) {
        const char *before = p;              /* kv_next provably advances; never hang the drain */
        int over = 0;
        p = kv_next(p, name, sizeof name, &kind, sval, sizeof sval, &ival, &fval,
                    nval, sizeof nval, &over);
        if (!p) {
            /* kv_next returns 0 right after skipping separators, so the stop char is the first
               non-separator from `before`: '}' ends the object cleanly, anything else is
               malformed and the apply pass would drop the rest of the keys silently */
            const char *q = before;
            while (*q == ' ' || *q == ',' || *q == '\n' || *q == '\t' || *q == '\r') q++;
            if (*q == '}') return 1;
            op_err(e, -32602, "InvalidParams", "keyValues: malformed object (the parse does not reach '}')");
            return 0;
        }
        if (ci_seq(name, "classname") || ci_seq(name, "origin") || ci_seq(name, "angles")) {
            e->code = -32602;
            e->name = "InvalidParams";
            SB db;
            sb_init(&db, e->data, sizeof e->data);
            /* the key goes in its own field: sb_addjson quotes it, which inside the reason
               string would break the JSON (seen live, FIX-B round 2) */
            sb_add(&db, "{\"reason\":\"keyValues: classname/origin/angles are refused on a tie "
                        "(they would reclass or move the new entity)\",\"key\":");
            sb_addjson(&db, name);
            sb_add(&db, "}");
            return 0;
        }
        /* over (an over-long string, an out-of-range number) stays the apply pass's per-key
           warning, exactly as on entity.create - the dry pass only refuses what would corrupt
           the tie itself */
    }
    op_err(e, -32602, "InvalidParams", "keyValues: malformed object");
    return 0;
}

/* the tie reply body (defined after the op: the full path and the all-alreadyTied no-op share it) */
static void tie_reply_body(SB *out, CMapNode *ent, CMapNode **meshes, int n,
                           const int *at_ids, int at_n, const int *rm_ids, int rm_n,
                           int created, const char *kvbuf, int noop);

static int op_entity_tie_meshes(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    /* exactly one of class / entityID (plan-triggers.md §5); the class gate itself (must be a
       @SolidClass) lives in the Rust FGD - the bridge re-checks after the create (below) */
    char cls[128];
    cls[0] = 0;
    long long entityID = -1;
    int has_class = params && json_find_top(params, "class");
    int has_entity = params && json_find_top(params, "entityID");
    if (has_class) {
        if (!json_string_top(params, "class", cls, sizeof cls) || !cls[0]) {
            op_err(e, -32602, "InvalidParams", "class: a non-empty string is required");
            return 0;
        }
        /* json_string_at cuts silently, so anything past 63 bytes must be refused before it can
           masquerade as a shorter class (the cap the wire contract names); cls[128] holds every
           string a caller could probe with up to the refusal */
        if (cls[63]) {
            op_err(e, -32602, "InvalidParams", "class: at most 63 bytes");
            return 0;
        }
    }
    if (has_entity &&
        (!json_int_top(params, "entityID", &entityID) || entityID < 0 || entityID > NODEID_MAX)) {
        op_err(e, -32602, "InvalidParams", "entityID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    if (has_class == has_entity) {
        op_err(e, -32602, "InvalidParams", "exactly one of class / entityID is required");
        return 0;
    }
    /* keyValues: the entity.create shape, and only with class (a retie target keeps its own) */
    const char *kv = 0;
    if (params) {
        kv = json_find_top(params, "keyValues");
        if (kv && (!has_class || *kv != '{')) {
            op_err(e, -32602, "InvalidParams", "keyValues: an object, and only with class");
            return 0;
        }
    }
    int allow_retie = 0;
    const char *arp = params ? json_find_top(params, "allowRetie") : 0;
    if (arp) {
        /* top-level only and exactly true/false (json_bool would take the first match anywhere) */
        if (lit_at(arp, "true")) allow_retie = 1;
        else if (!lit_at(arp, "false")) {
            op_err(e, -32602, "InvalidParams", "allowRetie: true or false required");
            return 0;
        }
    }
    if (kv) {
        /* FIX-B (review-b L2/L3): the whole object is validated BEFORE the group opens - a
           malformed one would otherwise be applied half-silently, and a classname/origin/angles
           key would run SetKeyValue after the solid-class gate below */
        if (!tie_kv_prescan(kv, e)) return 0;
    }
    /* nodeIDs: 1..64 distinct ints (the same reader node.delete uses; an id outside the i32
       range would alias another node through the (int) cast) */
    const char *np = params ? json_find_top(params, "nodeIDs") : 0;
    if (!np || *np != '[') {
        op_err(e, -32602, "InvalidParams", "nodeIDs: [int,...] required");
        return 0;
    }
    static int ids[TIE_MAX_MESHES];
    int n = 0;
    const char *p = jws(np + 1);
    while (*p != ']') {
        double d;
        p = jnum(p, &d);
        if (!p || d < 0 || d > 2147483647.0 || d != (double)(int)d) {
            op_err(e, -32602, "InvalidParams", "nodeIDs: non-negative ints up to 2147483647");
            return 0;
        }
        if (n >= TIE_MAX_MESHES) {
            op_err(e, -32602, "InvalidParams", "nodeIDs: at most 64");
            return 0;
        }
        ids[n++] = (int)d;
        p = jws(p);
        if (*p == ',') p = jws(p + 1);
        else if (*p != ']') { op_err(e, -32602, "InvalidParams", "nodeIDs: malformed"); return 0; }
    }
    if (!n) {
        op_err(e, -32602, "InvalidParams", "nodeIDs: at least one nodeID is required");
        return 0;
    }
    for (int i = 0; i < n; i++)
        for (int j = i + 1; j < n; j++)
            if (ids[i] == ids[j]) {
                op_err(e, -32602, "InvalidParams", "nodeIDs: duplicate nodeID");
                return 0;
            }
    /* the target entity (entityID form): a live CMapEntity whose FGD class is a solid class
       ([V live]: gamedata type 5), not locked - a point entity or a mesh id is refused before
       anything is opened */
    CMapNode *existing = 0;
    if (has_entity) {
        existing = geo_find(doc, (int)entityID);
        if (!existing) {
            op_err(e, -32022, "NodeNotFound", "no node with this entityID in the live document");
            return 0;
        }
        if (!obj_is(existing, PIN_HAMMER, HV_MAPENTITY_VT) || gd_type(existing) != 5) {
            op_err(e, -32602, "InvalidParams", "not a brush entity");
            return 0;
        }
        if (g_hfn.NodeIsLocked(existing, 0)) {
            op_err(e, -32018, "DocumentNotEditable", "node is locked");
            return 0;
        }
        /* review-b M5: a target inside a prefab/instance would pull the meshes into the
           sub-map's document - the same ancestor walk the meshes get below */
        if (!tie_ancestors_ok(doc, existing)) {
            tie_err_node(e, -32602, "InvalidParams", "entity is inside a prefab/instance",
                         (int)entityID, -1);
            return 0;
        }
    } else {
        /* the class must resolve in the node factory before the group opens (the same lookup
            entity.create validates with; CreateNodeByName re-resolves it). Same error as
            entity.create (review-b M4: the mock and the Rust tool map -32006 UnknownClass; a
            plain -32602 was mock/bridge drift). */
        void *reg = *(void **)((u8 *)g_modBase[PIN_HAMMER] + HG_NODE_FACTORY_REGISTRY);
        if (!reg || !g_hfn.FindTypeByName(reg, cls)) {
            op_err(e, -32006, "UnknownClass", "not in the CMapNodeFactory registry");
            return 0;
        }
    }
    /* every mesh: found, a CMapMesh (refuse groups/entities/props - Hammer's flatten and
       "Tie Static Objects" paths are out of scope), not locked, and not already tied to
       ANOTHER entity unless allowRetie (a mesh under the target itself is skipped and
       reported as alreadyTied, exactly what Hammer skips). A mesh under a group is fine:
       AddChild moves it between the layer containers. */
    static CMapNode *meshes[TIE_MAX_MESHES];
    static u8 skip[TIE_MAX_MESHES];
    static int at_ids[TIE_MAX_MESHES];
    int at_n = 0;
    for (int i = 0; i < n; i++) {
        CMapNode *nd = geo_find(doc, ids[i]);
        if (!nd) {
            tie_err_node(e, -32022, "NodeNotFound", "no node with this nodeID in the live document", ids[i], -1);
            return 0;
        }
        if (!is_mesh(nd)) {
            tie_err_node(e, -32602, "InvalidParams", "not a mesh", ids[i], -1);
            return 0;
        }
        /* review-b M5: a mesh inside a prefab/instance must never be reparented into a
           host-map entity (the ancestor walk above) */
        if (!tie_ancestors_ok(doc, nd)) {
            tie_err_node(e, -32602, "InvalidParams", "mesh is inside a prefab/instance",
                         ids[i], -1);
            return 0;
        }
        if (g_hfn.NodeIsLocked(nd, 0)) {
            op_err(e, -32018, "DocumentNotEditable", "node is locked");
            return 0;
        }
        meshes[i] = nd;
        skip[i] = 0;
        CMapNode *par = node_parent(nd);
        if (par && obj_is(par, PIN_HAMMER, HV_MAPENTITY_VT)) {
            if (existing && par == existing) {
                skip[i] = 1;
                at_ids[at_n++] = ids[i];
                continue;
            }
            if (!allow_retie) {
                int pid = -1;
                node_id_of(par, &pid);
                tie_err_node(e, -32602, "InvalidParams", "already tied", ids[i], pid);
                return 0;
            }
            /* review-b M2: a retie EMPTIES the old parent and DeleteNodes it - node.delete
               refuses locked nodes, so the retie must not bypass the lock on the entity it
               destroys. The error names the OLD parent ({nodeID: oldParent}). */
            if (g_hfn.NodeIsLocked(par, 0)) {
                int pid = -1;
                node_id_of(par, &pid);
                tie_err_node(e, -32018, "DocumentNotEditable", "node is locked", pid, -1);
                return 0;
            }
        }
    }
    if (!existing) {
        /* review-b L5: an unreadable origin/angles used to create the entity at [0,0,0] with a
           normal-looking reply - refuse before the group opens instead */
        float o0[3], a0[3];
        if (!node_origin(meshes[0], o0) || !node_angles(meshes[0], a0)) {
            op_err(e, -32603, "InternalError", "the first mesh's origin/angles are unreadable");
            return 0;
        }
    }
    /* FIX-B live L1 (review-b L4): a request whose EVERY mesh is already under the target
       entity has nothing to reparent - reply success WITHOUT opening a group (the old code
       pushed an empty "MCP: tie meshes to entity" group and marked the doc modified, so one
       undo_last popped nothing visible). at_n counts only entityID-form skips, so at_n == n
       implies the entityID form; the doc tail geo_dispatch appends reports the unchanged
       depth/serial. */
    if (at_n == n) {
        tie_reply_body(out, existing, meshes, n, at_ids, at_n, 0, 0, 0, 0, 1);
        return 1;
    }
    /* ---- the group: nothing above opened anything; everything below is undoable ---- */
    CMapDocHistory *hist = undo_open(doc, "MCP: tie meshes to entity");
    CMapNode *vec[TIE_MAX_MESHES + 1];
    int vc = 0;
    CMapNode *ent;
    int created = 0;
    if (existing) {
        ent = existing;
        vec[vc++] = ent;                     /* Hammer records the selected entity too */
    } else {
        float o[3] = { 0, 0, 0 }, a[3] = { 0, 0, 0 };
        node_origin(meshes[0], o);           /* Hammer: the FIRST node's origin/angles */
        node_angles(meshes[0], a);
        ent = g_hfn.CreateNodeByName(doc, cls, o, a);   /* records its own creation in this group */
        if (!ent) {
            /* FIX-B live L3 (review-b M3): the group IS open (undo_open ran above) - the error
               says so instead of a plain op_err, so the client knows one undo restores the
               depth. Nothing executed, so no maybeExecuted. */
            tie_err_undo_group(e, -32603, "InternalError", "CreateNodeByName failed (see bridge log)",
                               "MCP: tie meshes to entity");
            return 0;
        }
        created = 1;
        /* the defensive solid-class check: the FGD gate is Rust-side, so a point class that was
           sent anyway is caught here - the created node is deleted in the same group (the group
           is then a no-op: undoDepth + 1, document content unchanged). The error carries
           undoGroupOpened/undoLabel (FIX-B live L3, review-b M3) so the Rust side counts the
           refusal and the caller knows one undo pops the empty group. */
        if (gd_type(ent) != 5) {
            g_hfn.DeleteNode(doc, ent);
            tie_err_undo_group(e, -32602, "InvalidParams", "not a solid (brush) class",
                               "MCP: tie meshes to entity");
            return 0;
        }
    }
    static char kvbuf[8192];
    SB kb;
    int kv_ovf = 0;
    kvbuf[0] = 0;
    /* keyValues through the entity.create SetKeyValue path (no extra RecordNodeTree: the
       created node's CreateNodeByName record covers this group - re-props.md Q1). The echo is
       additive in the reply so a refused key is never silent; an echo that would not fit is
       dropped loudly (the write itself ran - the keys are on the entity) instead of emitting a
       cut JSON fragment into the reply. */
    if (kv && created) {
        CDmElement *child = g_hfn.GetEntityProperties(ent);
        if (child && mem_ok(child)) {
            sb_init(&kb, kvbuf, sizeof kvbuf);
            apply_keyvalues(ent, child, kv, &kb, 0);
            kv_ovf = kb.ovf;
            if (kv_ovf) logline("tie: keyvalues echo too large; the writes ran, the echo is dropped");
        } else {
            logline("tie: entity_properties missing; keyvalues not written");
        }
    }
    for (int i = 0; i < n; i++) vec[vc++] = meshes[i];
    NodeVec v = { vc, 0, vec, vc, S2_UTLVEC_EXTERNAL };
    /* BEFORE the AddChild calls (Hammer order): records each node and, when it has a parent,
       that parent non-recursively - this is what makes the reparent undoable */
    g_hfn.RecordNodesAndParents(hist, &v, 0);
    static int rm_ids[TIE_MAX_MESHES];
    static int tied_ids[TIE_MAX_MESHES];
    int rm_n = 0, tied_n = 0;
    for (int i = 0; i < n; i++) {
        if (skip[i]) continue;               /* already under the target: Hammer skips it */
        CMapNode *old = node_parent(meshes[i]);
        if (old == ent) continue;
        u8 added = ent->vt->AddChild(ent, meshes[i]);
        CMapNode *now = node_parent(meshes[i]);
        if (!added && now != ent) {          /* the call refused AND the read agrees it did not land */
            /* review-b M6: the ties before this one already ran and an old entity may already
               be deleted, so the document WAS modified: DocSetModified now (the old path
               returned without it, so closing the map might not prompt to save), and the error
               reports the progress with maybeExecuted:true (the removeConnections contract -
               verify with entity.get, do not retry). The id lists are capped at
               TIE_ERR_LIST_CAP entries each (e->data is a fixed buffer). */
            g_hfn.DocSetModified(doc, doc);
            e->code = -32603;
            e->name = "InternalError";
            SB db;
            sb_init(&db, e->data, sizeof e->data);
            sb_add(&db, "{\"reason\":\"AddChild refused a mesh; the ties before it in this group "
                        "already ran - verify with entity.get, do not retry\"");
            sb_add(&db, ",\"nodeID\":"); sb_addi(&db, ids[i]);
            sb_add(&db, ",\"maybeExecuted\":true,\"tied\":[");
            for (int t = 0; t < tied_n && t < TIE_ERR_LIST_CAP; t++) {
                if (t) sb_add(&db, ","); sb_addi(&db, tied_ids[t]);
            }
            sb_add(&db, "],\"removedEmptyEntities\":[");
            for (int t = 0; t < rm_n && t < TIE_ERR_LIST_CAP; t++) {
                if (t) sb_add(&db, ","); sb_addi(&db, rm_ids[t]);
            }
            sb_add(&db, "]}");
            return 0;
        }
        if (tied_n < TIE_MAX_MESHES) tied_ids[tied_n++] = ids[i];
        /* the emptied-old-brush-entity rule: an old parent that is a brush entity and now has
           no children is deleted in the same group (Hammer: RemoveNode(old, 3)) */
        if (old && obj_is(old, PIN_HAMMER, HV_MAPENTITY_VT) && node_child_count(old) == 0 &&
            gd_type(old) == 5) {
            int oid = -1;
            node_id_of(old, &oid);
            if (oid >= 0 && rm_n < TIE_MAX_MESHES) rm_ids[rm_n++] = oid;   /* review-b L6: an unreadable id is not reported as -1 */
            g_hfn.DeleteNode(doc, old);
        }
    }
    g_hfn.DocSetModified(doc, doc);
    tie_reply_body(out, ent, meshes, n, at_ids, at_n, rm_ids, rm_n, created,
                   (kvbuf[0] && !kv_ovf) ? kvbuf : 0, 0);
    return 1;
}

/* the tie reply body (FIX-B): shared by the full path above and the all-alreadyTied no-op, so
   the two cannot drift. The `doc` tail is appended by geo_dispatch, not here. nodeID is OMITTED
   when the entity's id is unreadable (review-b L6: entity.create's convention - read_tie treats
   a missing nodeID as an internal error, never a node -1), and a mesh row whose id is
   unreadable carries null. */
static void tie_reply_body(SB *out, CMapNode *ent, CMapNode **meshes, int n,
                           const int *at_ids, int at_n, const int *rm_ids, int rm_n,
                           int created, const char *kvbuf, int noop) {
    int eid = -1;
    node_id_of(ent, &eid);
    if (eid >= 0) { sb_add(out, "{\"nodeID\":"); sb_addi(out, eid); }
    else sb_add(out, "{");
    sb_add(out, ",\"handle\":"); sb_addjson(out, handle_new(ent));
    sb_add(out, ",\"class\":\"entity\"");
    char cn[512];
    cn[0] = 0;
    CDmElement *props = g_hfn.GetEntityProperties(ent);
    if (props && mem_ok(props)) child_string(props, "classname", cn, sizeof cn);
    sb_add(out, ",\"classname\":"); sb_addjson(out, cn);
    sb_add(out, ",\"created\":"); sb_add(out, created ? "true" : "false");
    float eo[3] = { 0, 0, 0 };
    node_origin(ent, eo);
    sb_add(out, ",\"origin\":"); sb_addf3(out, eo);
    sb_add(out, ",\"meshes\":[");
    for (int i = 0; i < n; i++) {
        int mid = -1;
        node_id_of(meshes[i], &mid);
        float mo[3] = { 0, 0, 0 };
        node_origin(meshes[i], mo);          /* unchanged by the tie (world-space transforms) */
        if (i) sb_add(out, ",");
        sb_add(out, "{\"nodeID\":");
        if (mid >= 0) sb_addi(out, mid); else sb_add(out, "null");   /* review-b L6 */
        sb_add(out, ",\"origin\":"); sb_addf3(out, mo);
        sb_add(out, "}");
    }
    sb_add(out, "],\"alreadyTied\":[");
    for (int i = 0; i < at_n; i++) { if (i) sb_add(out, ","); sb_addi(out, at_ids[i]); }
    sb_add(out, "],\"removedEmptyEntities\":[");
    for (int i = 0; i < rm_n; i++) { if (i) sb_add(out, ","); sb_addi(out, rm_ids[i]); }
    sb_add(out, "]");
    if (kvbuf) {                             /* additive echo (entity.create's keyValues shape) */
        sb_add(out, ",\"keyValues\":");
        sb_add(out, kvbuf);
    }
    /* the all-alreadyTied no-op opened no group: say so the way the Rust tool reads it
       (noop:true, undoLabel:null), so it neither counts an undo group nor an unsaved edit */
    if (noop) sb_add(out, ",\"noop\":true,\"undoLabel\":null");
    else sb_add(out, ",\"noop\":false,\"undoLabel\":\"MCP: tie meshes to entity\"");
    sb_add(out, "}");
}

/* ---- entity.untie (m2.19, re-triggers.md Q1) ----
   Hammer's own CMapDoc::MoveToWorld with a 1-entry vector: it opens its OWN undo group
   ("Untie from entity" - undo.undo reports that label), records recursively, moves every child
   to the entity's parent (the world, or a group the entity was in) and deletes the entity. So:
   NO undo_open here (a second group would stack an empty one on top), the children are collected
   BEFORE the call (the entity is gone afterwards), and the entity is never touched again. A side
   effect is Hammer selecting the untied children - UI state, harmless. */
static int op_entity_untie(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    long long id = -1;
    if (!params || !json_find_top(params, "nodeID")) {
        op_err(e, -32602, "InvalidParams", "nodeID required");
        return 0;
    }
    if (!json_int_top(params, "nodeID", &id) || id < 0 || id > NODEID_MAX) {
        op_err(e, -32602, "InvalidParams", "nodeID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    CMapNode *node = geo_entity_node(doc, id, e);   /* -32022 not found, -32602 not an entity */
    if (!node) return 0;
    /* a brush entity only: a point entity has no tied meshes, and neither has an entity without
       children (plan-triggers.md §5) */
    if (gd_type(node) != 5) {
        op_err(e, -32602, "InvalidParams", "not a brush entity");
        return 0;
    }
    if (g_hfn.NodeIsLocked(node, 0)) {
        op_err(e, -32018, "DocumentNotEditable", "node is locked");
        return 0;
    }
    int cc = node_child_count(node);
    if (cc == 0) {
        op_err(e, -32602, "InvalidParams", "not a brush entity");
        return 0;
    }
    /* review-b L7: only a CORRUPT count is refused as unreadable; a valid entity with more
       children than the reply lists is not an internal fault (Hammer itself has no cap) - the
       reply lists the first CHILDREN_LIST_MAX ids and reports meshesTruncated */
    if (cc < 0 || cc > 1000000 || !mem_ok(&node->m_Children.m_pMemory)) {
        op_err(e, -32603, "InternalError", "child list unreadable");
        return 0;
    }
    u32 *hs = CMapNode_ChildHandles(node);
    if (!hs || !mem_ok(hs) || !mem_ok(hs + cc - 1)) {
        op_err(e, -32603, "InternalError", "child list unreadable");
        return 0;
    }
    /* the children that go back to the entity's parent, for the reply - collected before the
       call, non-map-node handles skipped like geo_walk skips them; an unreadable id is skipped
       entirely, never reported as -1 (review-b L6) */
    static int kids[CHILDREN_LIST_MAX];
    int kn = 0, kid_trunc = 0;
    for (int i = 0; i < cc; i++) {
        CDmElement *ce = dm_element(hs[i]);
        if (!ce || !mem_ok(ce) || !is_map_node(ce)) continue;
        int cid = -1;
        node_id_of((CMapNode *)ce, &cid);
        if (cid < 0) continue;
        if (kn >= CHILDREN_LIST_MAX) { kid_trunc = 1; continue; }
        kids[kn++] = cid;
    }
    /* review-b L8 (defensive): per the RE, MoveToWorld calls entParent->vfn[57] without a
       check - a node whose parent field is unreadable is refused instead of dereferenced */
    if (!node_parent(node)) {
        op_err(e, -32603, "InternalError", "entity has no parent");
        return 0;
    }
    CMapNode *one[1] = { node };
    NodeVec v = { 1, 0, one, 1, S2_UTLVEC_EXTERNAL };
    g_hfn.DocMoveToWorld(doc, &v);       /* opens its own undo group; never report this node again */
    g_hfn.DocSetModified(doc, doc);
    sb_add(out, "{\"nodeID\":"); sb_addi(out, id);
    sb_add(out, ",\"meshes\":[");
    for (int i = 0; i < kn; i++) { if (i) sb_add(out, ","); sb_addi(out, kids[i]); }
    sb_add(out, "],\"entityDeleted\":true");
    if (kid_trunc) sb_add(out, ",\"meshesTruncated\":true");
    sb_add(out, ",\"undoLabel\":\"Untie from entity\"}");
    return 1;
}

typedef struct {
    SB *out;
    int listed, limit, truncated, visited;
    S2_CUtlSymbolLarge clsSym, tnSym;      /* interned once for the whole walk */
    const char *cls, *tname;
    int hasWithin; float c[3]; float r2;
} EWalk;

static void entity_find_walk(EWalk *w, CMapNode *node, int depth) {
    if (depth > 64 || w->visited > GEO_MAX_NODES) return;
    if (!mem_ok((u8 *)&node->m_Children.m_pMemory + 8)) return;
    int n = node->m_Children.m_Size;
    u32 *hs = CMapNode_ChildHandles(node);
    if (n <= 0 || n > 1000000 || !hs || !mem_ok(hs) || !mem_ok(hs + n - 1)) return;
    for (int i = 0; i < n; i++) {
        CDmElement *ce = dm_element(hs[i]);
        if (!is_map_node(ce)) continue;
        CMapNode *c = (CMapNode *)ce;
        w->visited++;
        if (obj_is(c, PIN_HAMMER, HV_MAPENTITY_VT)) {
            static char cb[512], tb[512];
            cb[0] = 0; tb[0] = 0;
            CDmElement *child = g_hfn.GetEntityProperties(c);
            if (child && mem_ok(child)) {
                CDmAttribute *a = g_hfn.FindAttribute(child, &w->clsSym);
                const char *s = a ? attr_cstr(a) : 0;
                if (s) probe_str(s, cb, sizeof cb);
                a = g_hfn.FindAttribute(child, &w->tnSym);
                s = a ? attr_cstr(a) : 0;
                if (s) probe_str(s, tb, sizeof tb);
            }
            int match = (!w->cls || ci_seq(cb, w->cls)) && (!w->tname || ci_seq(tb, w->tname));
            if (match && w->hasWithin) {
                float o[3] = { 0, 0, 0 };
                node_origin(c, o);
                float dx = o[0] - w->c[0], dy = o[1] - w->c[1], dz = o[2] - w->c[2];
                if (dx * dx + dy * dy + dz * dz > w->r2) match = 0;
            }
            if (match) {
                if (w->listed >= w->limit) w->truncated = 1;
                else {
                    float o[3] = { 0, 0, 0 };
                    int id = -1;
                    node_id_of(c, &id);
                    node_origin(c, o);
                    if (w->listed) sb_add(w->out, ",");
                    sb_add(w->out, "{\"nodeID\":"); sb_addi(w->out, id);
                    sb_add(w->out, ",\"classname\":"); sb_addjson(w->out, cb);
                    sb_add(w->out, ",\"targetname\":"); sb_addjson(w->out, tb);
                    sb_add(w->out, ",\"origin\":"); sb_addf3(w->out, o);
                    sb_add(w->out, "}");
                    w->listed++;
                }
            }
        }
        entity_find_walk(w, c, depth + 1);
    }
}

static int op_entity_find(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    char cls[512], tn[512];
    cls[0] = 0;
    tn[0] = 0;
    long long lim = ENTITY_FIND_DEFAULT;
    if (params) {
        json_string_top(params, "class", cls, sizeof cls);          /* top-level only (opus #1) */
        json_string_top(params, "targetname", tn, sizeof tn);
        /* an empty filter is not a match-all: it is the caller's mistake (review opus #15) */
        if (json_find_top(params, "class") && !cls[0]) {
            op_err(e, -32602, "InvalidParams", "class: a non-empty string is required");
            return 0;
        }
        if (json_find_top(params, "targetname") && !tn[0]) {
            op_err(e, -32602, "InvalidParams", "targetname: a non-empty string is required");
            return 0;
        }
        /* an out-of-range limit is InvalidParams, never a silent clamp to the default
           (review ds #11) */
        if (json_find_top(params, "limit") &&
            (!json_int_top(params, "limit", &lim) || lim < 1 || lim > ENTITY_FIND_MAX)) {
            op_err(e, -32602, "InvalidParams", "limit: an int in 1..2000 is required");
            return 0;
        }
    }
    float c[3] = { 0, 0, 0 }, radius = 0;
    int hasWithin = 0;
    const char *wp = params ? json_find_top(params, "within") : 0;
    if (wp) {
        if (*wp != '{' || !json_float3_top(wp, "origin", c) || !json_float1_top(wp, "radius", &radius) || !(radius > 0.0f)) {
            op_err(e, -32602, "InvalidParams", "within: {origin:[x,y,z], radius > 0} required");
            return 0;
        }
        hasWithin = 1;
    }
    CMapNode *wn = geo_world(doc);
    if (!wn) { op_err(e, -32603, "InternalError", "no world node"); return 0; }
    EWalk w;
    w.out = out; w.listed = 0; w.limit = (int)lim; w.truncated = 0; w.visited = 0;
    w.cls = cls[0] ? cls : 0; w.tname = tn[0] ? tn : 0;
    w.hasWithin = hasWithin; w.c[0] = c[0]; w.c[1] = c[1]; w.c[2] = c[2]; w.r2 = radius * radius;
    w.clsSym = dm_intern("classname");
    w.tnSym = dm_intern("targetname");
    sb_add(out, "{\"entities\":[");
    entity_find_walk(&w, wn, 0);
    sb_add(out, "],\"count\":"); sb_addi(out, w.listed);
    sb_add(out, ",\"visited\":"); sb_addi(out, w.visited);
    sb_add(out, ",\"truncated\":"); sb_add(out, w.truncated ? "true" : "false");
    sb_add(out, ",\"source\":\"live\"}");
    return 1;
}

/* make an existing mesh empty and identity-transformed so InitializeFromPolygonGroup can take WORLD
   points (it computes the new pivot from local bounds and applies it as a world position, which is
   only right for an identity transform - the state of a freshly created mesh) */
static int geo_reset_mesh(CMapMesh *mesh) {
    CMapNode *node = CMapMesh_Node(mesh);
    CHammerPolyMesh *hm = CMapMesh_PolyMesh(mesh);
    int n = hm->faces.count;
    if (n > 0) {
        if (n > GEO_MAX_FACES * 4) return 0;
        S2MeshHandle *buf = (S2MeshHandle *)geo_alloc(sizeof(S2MeshHandle) * (u64)n);
        if (!buf) return 0;
        S2HandleVec v; v.size = 0; v.pad = 0; v.mem = buf; v.alloc = n; v.flags = S2_UTLVEC_EXTERNAL;
        if (!g_hfn.CollectHandles(&hm->faces, &v)) { geo_free(buf); return 0; }
        if (v.size > 0) g_hfn.MeshDeleteFaces(mesh, &v);
        geo_free(buf);
    }
    /* m2.18 bug fix (re-props.md Q2): this used to zero slot 17, which is SetOrigin, not
       SetAngles - a rotated mesh lost its ORIGIN here and kept its rotation. Angles are slot 18
       (the mesh override is the sigs entry HVS_MESH_SET_ROTATION; HVS_MESH_SET_ANGLES' IDA name for slot 17 is
       wrong and its use is retired).
       The scales are reset with it, in Hammer's own order: "Clear Rotation and Scale" writes
       scales {1,1,1} (slot 19) and then angles {0,0,0} (slot 18). Without it a mesh scaled by
       node.setTransform would build the new world-point geometry under a 2x node scale. */
    if (node->m_Scales.x != 1.0f || node->m_Scales.y != 1.0f || node->m_Scales.z != 1.0f) {
        float one3[3] = { 1, 1, 1 };
        node->vt->SetScales(node, one3);
    }
    if (node->m_Angles.x != 0.0f || node->m_Angles.y != 0.0f || node->m_Angles.z != 0.0f) {
        float zero3[3] = { 0, 0, 0 };
        node->vt->SetAngles(node, zero3);
    }
    float zero[3] = { 0, 0, 0 };
    node->vt->SetPivotPosition(node, zero);
    return 1;
}

/* build polygons and run InitializeFromPolygonGroup (+ per-face materials) on mesh */
static int geo_build(CMapMesh *mesh, GGeom *g, int *facesOut, const char **why) {
    GPolyRec *recs = (GPolyRec *)geo_alloc(sizeof(GPolyRec) * (u64)g->nf);
    GVec *pts = (GVec *)geo_alloc(sizeof(GVec) * (u64)g->nidx);
    S2MeshHandle *fh = (S2MeshHandle *)geo_alloc(sizeof(S2MeshHandle) * (u64)g->nf);
    if (!recs || !pts || !fh) { geo_free(recs); geo_free(pts); geo_free(fh); *why = "out of memory"; return 0; }
    for (int f = 0; f < g->nf; f++) {
        recs[f].n = g->fcount[f];
        recs[f].pts = pts + g->fstart[f];
        for (int k = 0; k < g->fcount[f]; k++) pts[g->fstart[f] + k] = g->v[g->idx[g->fstart[f] + k]];
    }
    GPolyGroup grp; grp.count = g->nf; grp.pad = 0; grp.recs = recs;
    S2HandleVec out; out.size = 0; out.pad = 0; out.mem = fh; out.alloc = g->nf; out.flags = S2_UTLVEC_EXTERNAL;   /* caller-owned, never realloc */
    g_hfn.MeshInitFromPolygons(mesh, &grp, g->defmat, GEO_UV_MODE, &out);
    *facesOut = out.size;
    if (g->nmat && out.size == g->nf) {
        S2MeshHandle *sel = (S2MeshHandle *)geo_alloc(sizeof(S2MeshHandle) * (u64)g->nf);
        if (sel) {
            for (int mi = 0; mi < g->nmat; mi++) {
                int n = 0;
                for (int f = 0; f < g->nf; f++)
                    if (g->fmat[f] == mi) sel[n++] = fh[f];
                if (n) g_hfn.AssignMaterialToFaces(CMapMesh_PolyMesh(mesh), sel, n, g->mats[mi]);
            }
            geo_free(sel);
        }
    }
    geo_free(recs); geo_free(pts); geo_free(fh);
    if (out.size <= 0) { *why = "Hammer created no faces (degenerate polygons?)"; return 0; }
    return 1;
}

static void geo_mesh_summary(CMapMesh *mesh, SB *out) {
    int id = -1;
    float o[3] = { 0, 0, 0 };
    node_id_of(CMapMesh_Node(mesh), &id);
    node_origin(CMapMesh_Node(mesh), o);
    CHammerPolyMesh *hm = CMapMesh_PolyMesh(mesh);
    sb_add(out, "\"nodeID\":"); sb_addi(out, id);
    sb_add(out, ",\"class\":\"mesh\",\"origin\":"); sb_addf3(out, o);
    sb_add(out, ",\"vertexSlots\":"); sb_addi(out, hm->vertices.count);
    sb_add(out, ",\"faceSlots\":"); sb_addi(out, hm->faces.count);
}

/* ---------------- input hook: virtual buttons + view angles (worknotes/inputhook-scratch.md) ----------------
   Drives the local player through the game's own input pipeline instead of keypresses/console:
   - buttons: engine2 CInputValue::ApplyButtonDelta(iv, slot, +-1, frac) - the primitive `+name`/`-name`
     use - on the button's CInputValue, so held state, press/release edges, subtick steps, prediction and
     buttons_pb come out exactly as for a real key;
   - view angles: CClientInput CreateMove's override slot (32) returns our QAngle while enabled.
   Mechanism: vptr swap of the static CCSGOInput to a private copy of its vtable in which only the empty
   slots 30 (ProcessInput begin, before the button gather) and 32 (angle override) are replaced. No game
   code is patched; uninstall restores the original vptr. S2_VTLEN_CClientInput (generated) sizes
   the copy of the CCSGOInput vftable: right only because sdkcheck.py holds it `==` the sigs `vtlen`
   measurement of CCSGOInput's primary table (client.json CVC_INPUT_VT_SLOTS, read from its RTTI COL),
   which stays as that check - it catches the game adding a virtual to the concrete class. */
typedef void (*InApplyFn)(void *iv, int slot, float delta, float frac);
static void **g_inObj;                       /* the static CCSGOInput */
static void *g_inOrigVt;                     /* its original vtable */
static void *g_inVtCopy[1 + S2_VTLEN_CClientInput]; /* [0] = RTTI COL, then the slots */
static InApplyFn g_inApply;
static volatile long g_inLock;
static u64 g_inWant;                         /* virtual buttons held (IN_ bit mask) */
static u64 g_inApplied;                      /* bits whose +1 delta we currently own */
static u16 g_inTap[64];                      /* remaining frames of a tap per bit */
static int g_inAnglesOn;
static float g_inAngles[3];
static volatile long g_inFrames, g_inEdges, g_inAngleTicks;

static void in_lock(void) { while (il_cmpxchg(&g_inLock, 1, 0) != 0) {} }
static void in_unlock(void) { il_xchg(&g_inLock, 0); }

static void **in_table(void *self) { return (void **)((u8 *)self + S2_OFF_CClientInput_m_pButtons); }
static const char *in_btn_name(void *btn) {    /* btn+S2_OFF_ClientInputButton_t_m_InputValue = CInputValue, iv+0 = name */
    if (!btn || !mem_ok(btn)) return 0;
    const char *n = *(const char **)((u8 *)btn + S2_OFF_ClientInputButton_t_m_InputValue);
    return (n && mem_ok((void *)n)) ? n : 0;
}
static int ieq(const char *a, const char *b) {
    for (; *a && *b; a++, b++) {
        char x = *a, y = *b;
        if (x >= 'A' && x <= 'Z') x = (char)(x + 32);
        if (y >= 'A' && y <= 'Z') y = (char)(y + 32);
        if (x != y) return 0;
    }
    return *a == *b;
}
/* Matches a button by name AND requires its mask to be exactly 1 << i. The moveup / movedown buttons carry
   mask -1 (IN_ALL: they are not user-command buttons), so they never match and are not reachable through
   input.set (by design or not; recorded in s2sdk-types re/client.md section 5). */
static int in_find_bit(const char *name) {
    void **t = in_table(g_inObj);
    for (int i = 0; i < 64; i++) {
        const char *n = in_btn_name(t[i]);
        if (n && ieq(n, name) && *(u64 *)t[i] == (1ull << i)) return i;
    }
    return -1;
}

/* slot 30: ProcessInput begin (main-thread frame, before the button gather) */
static void in_hook_frame_begin(void *self, int slot, void *rec) {
    (void)rec;
    if (slot != 0) return;
    in_lock();
    u64 want = g_inWant;
    for (int i = 0; i < 64; i++) if (g_inTap[i]) { want |= 1ull << i; g_inTap[i]--; }
    u64 diff = want ^ g_inApplied;
    if (diff) {
        void **t = in_table(self);
        for (int i = 0; i < 64; i++) {
            u64 bit = 1ull << i;
            if (!(diff & bit) || !t[i]) continue;
            g_inApply((u8 *)t[i] + S2_OFF_ClientInputButton_t_m_InputValue, 0, (want & bit) ? 1.0f : -1.0f, 0.0f);
            g_inApplied ^= bit;
            il_inc(&g_inEdges);
        }
    }
    in_unlock();
    il_inc(&g_inFrames);
}
/* slot 32: CreateMove view-angle override; true = use *ang */
static int in_hook_override_angles(void *self, int slot, double curtime, float *ang) {
    (void)self; (void)curtime;
    if (slot != 0 || !g_inAnglesOn || !ang) return 0;
    in_lock();
    ang[0] = g_inAngles[0]; ang[1] = g_inAngles[1]; ang[2] = g_inAngles[2];
    in_unlock();
    il_inc(&g_inAngleTicks);
    return 1;
}

static int in_installed(void) { return g_inObj && *(void **)g_inObj == (void *)&g_inVtCopy[1]; }
static int in_install(OpErr *e) {
    if (in_installed()) return 1;
    u8 *cb = (u8 *)g_modBase[PIN_CLIENT], *eb = (u8 *)g_modBase[PIN_ENGINE2];
    void **obj = *(void ***)(cb + CG_INPUT_PTR);
    if (!obj_is(obj, PIN_CLIENT, CV_CSGOINPUT_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "g_pInput is not the pinned CCSGOInput (vtable)");
        return 0;
    }
    void **vt = (void **)(cb + CV_CSGOINPUT_VT);
    g_inVtCopy[0] = vt[-1];
    for (int i = 0; i < (int)S2_VTLEN_CClientInput; i++) g_inVtCopy[1 + i] = vt[i];
    g_inVtCopy[1 + S2_SLOT_CClientInput_unk3021] = (void *)in_hook_frame_begin;
    g_inVtCopy[1 + S2_SLOT_CClientInput_unk3023] = (void *)in_hook_override_angles;
    g_inApply = (InApplyFn)(eb + EFN_INPUTVALUE_APPLY);
    g_inObj = obj;
    g_inOrigVt = vt;
    *(void *volatile *)obj = (void *)&g_inVtCopy[1];   /* aligned 8-byte store: atomic on x64 */
    logline("input: hook installed (CCSGOInput vptr -> private vtable copy)");
    return 1;
}
static void in_uninstall(void) {
    if (!in_installed()) return;
    in_lock();
    /* drop our outstanding +1 deltas so no button stays held after we are gone */
    void **t = in_table(g_inObj);
    for (int i = 0; i < 64; i++)
        if ((g_inApplied >> i & 1) && t[i]) g_inApply((u8 *)t[i] + S2_OFF_ClientInputButton_t_m_InputValue, 0, -1.0f, 0.0f);
    g_inApplied = g_inWant = 0;
    g_inAnglesOn = 0;
    in_unlock();
    *(void *volatile *)g_inObj = g_inOrigVt;
    logline("input: hook removed");
}

/* each string in a JSON array value of `key` -> cb(name); returns count or -1 on a bad name */
static int in_each_name(const char *params, const char *key, int (*cb)(int bit, void *ctx), void *ctx,
                        char *bad, u32 badCap) {
    const char *p = params ? json_find(params, key) : 0;
    if (!p) return 0;
    if (*p != '[') return -1;
    p++;
    int n = 0;
    for (;;) {
        while (*p == ' ' || *p == ',') p++;
        if (*p == ']' || !*p) break;
        if (*p != '"') return -1;
        char name[48]; u32 k = 0;
        p++;
        while (*p && *p != '"' && k + 1 < sizeof name) name[k++] = *p++;
        name[k] = 0;
        if (*p == '"') p++;
        int bit = in_find_bit(name);
        if (bit < 0) { cpy(bad, name, badCap); return -1; }
        if (!cb(bit, ctx)) return -1;
        n++;
    }
    return n;
}
static int in_cb_press(int bit, void *c) { (void)c; g_inWant |= 1ull << bit; return 1; }
static int in_cb_release(int bit, void *c) { (void)c; g_inWant &= ~(1ull << bit); g_inTap[bit] = 0; return 1; }
static int in_cb_tap(int bit, void *c) { g_inTap[bit] = (u16)*(int *)c; return 1; }

static void in_status_json(SB *out) {
    sb_add(out, "{\"installed\":"); sb_add(out, in_installed() ? "true" : "false");
    sb_add(out, ",\"held\":[");
    int first = 1;
    void **t = g_inObj ? in_table(g_inObj) : 0;
    for (int i = 0; t && i < 64; i++) {
        if (!((g_inWant | g_inApplied) >> i & 1)) continue;
        const char *n = in_btn_name(t[i]);
        if (!first) sb_add(out, ",");
        first = 0;
        sb_addjson(out, n ? n : "?");
    }
    sb_add(out, "],\"angles\":");
    if (g_inAnglesOn) {
        sb_add(out, "["); sb_addf(out, g_inAngles[0]); sb_add(out, ",");
        sb_addf(out, g_inAngles[1]); sb_add(out, ","); sb_addf(out, g_inAngles[2]); sb_add(out, "]");
    } else sb_add(out, "null");
    sb_add(out, ",\"frames\":"); sb_addi(out, (int)g_inFrames);
    sb_add(out, ",\"edges\":"); sb_addi(out, (int)g_inEdges);
    sb_add(out, ",\"angleTicks\":"); sb_addi(out, (int)g_inAngleTicks);
    sb_add(out, ",\"buttons\":[");
    first = 1;
    for (int i = 0; t && i < 64; i++) {
        const char *n = in_btn_name(t[i]);
        if (!n) continue;
        if (!first) sb_add(out, ",");
        first = 0;
        sb_addjson(out, n);
    }
    sb_add(out, "]}");
}

/* input.set {press:[names], release:[names], tap:[names], frames:n, angles:[p,y,r] | angles:null}
   input.clear {}   input.status {}   input.uninstall {} */
static int op_input(const char *method, const char *params, SB *out, OpErr *e) {
    if (seq(method, "input.uninstall")) {
        in_uninstall();
        in_status_json(out);
        return 1;
    }
    if (!in_install(e)) return 0;
    if (seq(method, "input.set")) {
        char bad[48]; bad[0] = 0;
        long long frames = 1;
        if (params) json_int(params, "frames", &frames);
        if (frames < 1) frames = 1;
        if (frames > 1000) frames = 1000;
        int fr = (int)frames;
        float ang[3];
        int hasAng = params && json_float3(params, "angles", ang);
        const char *ap = params ? json_find(params, "angles") : 0;
        int angOff = ap && ap[0] == 'n' && ap[1] == 'u' && ap[2] == 'l' && ap[3] == 'l';
        if (ap && !hasAng && !angOff) { op_err(e, -32602, "InvalidParams", "angles must be [pitch,yaw,roll] or null"); return 0; }
        in_lock();
        u64 saveWant = g_inWant;
        int ok = in_each_name(params, "press", in_cb_press, 0, bad, sizeof bad) >= 0 &&
                 in_each_name(params, "release", in_cb_release, 0, bad, sizeof bad) >= 0 &&
                 in_each_name(params, "tap", in_cb_tap, &fr, bad, sizeof bad) >= 0;
        if (!ok) g_inWant = saveWant;
        else if (hasAng) { g_inAngles[0] = ang[0]; g_inAngles[1] = ang[1]; g_inAngles[2] = ang[2]; g_inAnglesOn = 1; }
        else if (angOff) g_inAnglesOn = 0;
        in_unlock();
        if (!ok) {
            char msg[96]; SB mb; sb_init(&mb, msg, sizeof msg);
            sb_add(&mb, "unknown button or bad list: "); sb_add(&mb, bad[0] ? bad : "?");
            op_err(e, -32602, "InvalidParams", msg);
            return 0;
        }
    } else if (seq(method, "input.clear")) {
        in_lock();
        g_inWant = 0;
        for (int i = 0; i < 64; i++) g_inTap[i] = 0;
        g_inAnglesOn = 0;
        in_unlock();
    }
    in_status_json(out);
    return 1;
}

/* ---------------- test-runner support: checkpoints, console, tick, save/restore unlock ----------------
   (worknotes/vtest-scratch.md, docs/flows/client-input-pipeline.md). Interfaces come from each module's
   exported CreateInterface and are identity-checked against the pinned vtables before any call. */
static void *mod_iface(int m, const char *name) {
    void *h = GetModuleHandleA(k_pinModules[m].name);
    if (!h) return 0;
    S2CreateInterfaceFn ci = (S2CreateInterfaceFn)GetProcAddress(h, S2_SYM_CREATEINTERFACE);
    if (!ci) return 0;
    int ret = -1;
    return ci(name, &ret);
}
static int cmdline_has(const char *flag) {
    const char *c = GetCommandLineA();
    u32 n = 0; while (flag[n]) n++;
    for (; c && *c; c++) {
        u32 i = 0;
        while (i < n && c[i] == flag[i]) i++;
        if (i == n && (c[n] == 0 || c[n] == ' ' || c[n] == '"')) return 1;
    }
    return 0;
}

/* save/restore unlock: ISource2ServerConfig::AllowSaveRestore (slot 21) returns false in CS2; engine2's
   save (CanSaveGame 0x1801E44F0) and load (0x1801CEA70) both test it. Swap the config object's vptr to a
   copy whose slot 21 returns true; disable restores the original. The copy length is the GAME-measured
   ISource2ServerConfig vftable length: the generated S2_VTLEN_ISource2ServerConfig, right only because
   sdkcheck.py holds it `==` the sigs `vtlen` measurement of the table the game actually calls through
   (server.json GSVC_GAMECONFIG_VT_SLOTS), which stays as that check (a drift fails the build; the fix
   is updating s2sdk upstream). The slot itself
   is the generated S2_SLOT_ISource2ServerConfig_AllowSaveRestore, held to the IDA-measured sigs entry
   GSVS_ALLOW_SAVE_RESTORE by pins.py and sdkcheck.py (its `sdk` field). */
static void **g_srObj;
static void *g_srOrigVt;
static void *g_srVtCopy[1 + S2_VTLEN_ISource2ServerConfig];
_Static_assert((int)S2_SLOT_ISource2ServerConfig_AllowSaveRestore < (int)S2_VTLEN_ISource2ServerConfig,
               "AllowSaveRestore must be an in-table slot of the measured game vftable");
static int sr_true(void *self) { (void)self; return 1; }
static int sr_enabled(void) { return g_srObj && *(void **)g_srObj == (void *)&g_srVtCopy[1]; }
static int sr_set(int enable, OpErr *e) {
    if (!enable) {
        if (sr_enabled()) *(void *volatile *)g_srObj = g_srOrigVt;
        return 1;
    }
    if (sr_enabled()) return 1;
    void **obj = (void **)mod_iface(PIN_SERVER, S2_IFACE_SOURCE2SERVERCONFIG);
    if (!obj_is(obj, PIN_SERVER, GSV_GAMECONFIG_SERVER_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "Source2ServerConfig001 is not the pinned CCSGameConfiguration (+8 view)");
        return 0;
    }
    void **vt = (void **)((u8 *)g_modBase[PIN_SERVER] + GSV_GAMECONFIG_SERVER_VT);
    g_srVtCopy[0] = vt[-1];
    for (int i = 0; i < (int)S2_VTLEN_ISource2ServerConfig; i++) g_srVtCopy[1 + i] = vt[i];
    g_srVtCopy[1 + S2_SLOT_ISource2ServerConfig_AllowSaveRestore] = (void *)sr_true;
    g_srObj = obj;
    g_srOrigVt = vt;
    *(void *volatile *)obj = (void *)&g_srVtCopy[1];
    logline("test: save/restore unlocked (ISource2ServerConfig::AllowSaveRestore -> true)");
    return 1;
}

typedef void (*TsCheckPointFn)(void *self, const char *name);
typedef int (*TsIsActiveFn)(void *self);
typedef int (*EcGetTickFn)(void *self);   /* IVEngineClient2::GetLastServerTick (the pin says GET_TICK) */
typedef void (*IsAddCommandTextFn)(void *self, int target, const char *text, int source);

static int op_test(const char *method, const char *params, SB *out, OpErr *e) {
    if (seq(method, "game.tick")) {
        void **ec = (void **)mod_iface(PIN_ENGINE2, S2_IFACE_SOURCE2ENGINETOCLIENT);
        if (!obj_is(ec, PIN_ENGINE2, EV_ENGINECLIENT_VT)) {
            op_err(e, -32017, "PrerequisiteFailed", "Source2EngineToClient001 is not the pinned CEngineClient");
            return 0;
        }
        int tick = ((EcGetTickFn)(*(void ***)ec)[S2_SLOT_IVEngineClient2_GetLastServerTick])(ec);
        sb_add(out, "{\"tick\":"); sb_addi(out, tick);
        sb_add(out, ",\"frames\":"); sb_addi(out, (int)g_inFrames);
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "game.saveRestore")) {
        int en = 1;
        if (params) json_bool(params, "enable", &en);
        if (!sr_set(en, e)) return 0;
        sb_add(out, "{\"enabled\":"); sb_add(out, sr_enabled() ? "true" : "false"); sb_add(out, "}");
        return 1;
    }
    if (seq(method, "console.exec")) {
        if (!cmdline_has("-testscript")) {
            op_err(e, -32011, "NotImplemented", "console.exec is only enabled in -testscript sessions");
            return 0;
        }
        static char cmd[1024];
        if (!params || !json_string(params, "command", cmd, sizeof cmd) || !cmd[0]) {
            op_err(e, -32602, "InvalidParams", "command (string) required");
            return 0;
        }
        void **is = (void **)mod_iface(PIN_ENGINE2, S2_IFACE_INPUTSERVICE);
        if (!obj_is(is, PIN_ENGINE2, EV_INPUTSERVICE_VT)) {
            op_err(e, -32017, "PrerequisiteFailed", "InputService_001 is not the pinned CInputService");
            return 0;
        }
        ((IsAddCommandTextFn)(*(void ***)is)[S2_SLOT_IInputService_AddCommandText])(is, 5, cmd, 0);
        sb_add(out, "{\"queued\":true}");
        return 1;
    }
    /* test.checkpoint {name} / test.active {} */
    void **ts = (void **)mod_iface(PIN_TIER0, S2_IFACE_TESTSCRIPTMANAGER);
    if (!obj_is(ts, PIN_TIER0, T0V_TESTSCRIPTMGR_VT)) {
        op_err(e, -32017, "PrerequisiteFailed", "TestScriptMgr001 is not the pinned CTestScriptMgr");
        return 0;
    }
    int active = ((TsIsActiveFn)(*(void ***)ts)[S2_SLOT_ITestScriptMgr_IsRunningTestScript])(ts);
    if (seq(method, "test.checkpoint")) {
        static char name[128];
        if (!params || !json_string(params, "name", name, sizeof name) || !name[0]) {
            op_err(e, -32602, "InvalidParams", "name (string) required");
            return 0;
        }
        ((TsCheckPointFn)(*(void ***)ts)[S2_SLOT_ITestScriptMgr_CheckPoint])(ts, name);
    }
    sb_add(out, "{\"active\":"); sb_add(out, active ? "true" : "false"); sb_add(out, "}");
    return 1;
}

/* ---------------- ModelDoc lifecycle (m2.11): .vmdl documents ----------------
   Object chain (hammer-mcp/research/modeldoc-re.md, all [V]): modeldoc_editor.dll's exported
   CreateInterface("ToolSystem2_ModelDoc") (*ret 0 = found*) == g_CModelDocEditorApp (MDG_APP_OBJ)
   -> app+S2_OFF_CBaseEditorApp_m_pActiveSession -> active session (may be 0) -> session+S2_OFF_CModelDocEditorSession_m_pDoc -> CModelDoc
   (replaced on every load/reload: never cache). The module is late-loaded (runs only while a
   ModelDoc window is open); modules_poll pins and verifies it when it appears.
   Everything below runs on the main-thread drain: OpenFile pops modal dialogs for missing/broken
   files, so only host-validated content-relative paths are sent (the Rust tool pre-checks), and
   session slot 21 SaveAndCompile (a "Compile?" modal) is never called. Compile returns nothing:
   `failed` comes from compileStatus ("Compile Failed", sticky), and the .vmdl_c mtime is the
   truth on the tool side. */
static CModelDocEditorApp *g_modeldoc;         /* resolved once; pinned module -> stable identity */
static ModelDocCompileFn g_mdCompile;
static int g_mdCompileBound;

/* Compile is non-virtual, non-exported: bound from the pinned RVA after verification */
static void modeldoc_bind(void) {
    if (g_mdCompileBound || !mod_ok(PIN_MODELDOC)) return;
    *(void **)&g_mdCompile = (void *)((u8 *)g_modBase[PIN_MODELDOC] + MDFN_SESSION_COMPILE);
    g_mdCompileBound = 1;
}

/* CModelDocEditorApp via modeldoc_editor.dll's exported CreateInterface - the static singleton,
   identity-checked against its pinned static object (mirrors resolve_hammer) */
static CModelDocEditorApp *resolve_modeldoc(void) {
    if (g_modeldoc) return g_modeldoc;
    if (!mod_ok(PIN_MODELDOC)) { logline("modeldoc: module not verified"); return 0; }
    u8 *mb = (u8 *)g_modBase[PIN_MODELDOC];
    void *hm = GetModuleHandleA(k_pinModules[PIN_MODELDOC].name);
    if (hm != (void *)mb) { logline("modeldoc: module handle changed"); return 0; }
    S2CreateInterfaceFn ci = (S2CreateInterfaceFn)GetProcAddress(hm, S2_SYM_CREATEINTERFACE);
    if (!ci) { logline("modeldoc: CreateInterface export missing"); return 0; }
    int ret = -1;
    CModelDocEditorApp *app = (CModelDocEditorApp *)ci(S2_TOOLSYSTEM2_MODELDOC_VERSION, &ret);
    if (!app || ret != 0) {                     /* ret 0 = found */
        lognum("modeldoc: ToolSystem2_ModelDoc unresolved, ret=", (u64)(long long)ret);
        return 0;
    }
    if ((void *)app != (void *)(mb + MDG_APP_OBJ)) { logline("modeldoc: app identity mismatch"); return 0; }
    if (!obj_is(app, PIN_MODELDOC, MDV_APP_VT)) { logline("modeldoc: app vtable mismatch"); return 0; }
    modeldoc_bind();
    g_modeldoc = app;
    logline("modeldoc: app resolved (ToolSystem2_ModelDoc export, ret 0)");
    return app;
}

/* active session with per-hop validation. reason = short diagnostic string. */
static CModelDocEditorSession *active_modeldoc(const char **reason) {
    *reason = "ok";
    if (!mod_ok(PIN_MODELDOC)) {
        *reason = (g_modState[PIN_MODELDOC] == MS_MISMATCH) ? "modeldoc_mismatch"
                : (g_modState[PIN_MODELDOC] == MS_VERIFYING) ? "modeldoc_verifying"
                : "modeldoc_not_loaded";
        return 0;
    }
    CModelDocEditorApp *app = resolve_modeldoc();
    if (!app) { *reason = "modeldoc_unavailable"; return 0; }
    CModelDocEditorSession *session = CModelDocEditorApp_ActiveSession(app);
    if (!session) { *reason = "no_session"; return 0; }
    if (!obj_is(session, PIN_MODELDOC, MDV_SESSION_VT)) { *reason = "bad_session"; return 0; }
    if (!session->doc) { *reason = "no_document"; return 0; }
    if (!obj_is(session->doc, PIN_MODELDOC, MDV_DOC_VT)) { *reason = "bad_doc"; return 0; }
    return session;
}

/* copy a game-owned C string, probing readability first and again at every page boundary
   (strings can point at freed or reallocated CUtlString storage). Returns 1 when the whole
   string fit, -1 when it was cut at cap-1 (out holds the prefix), 0 (empty out) when null or
   unreadable. */
static int md_str(const char *s, char *out, u32 cap) {
    out[0] = 0;
    if (!s || !mem_ok(s)) return 0;
    u32 i = 0;
    for (; i + 1 < cap; i++) {
        if (i && (((u64)(size_t)(s + i)) & 0xFFFu) == 0 && !mem_ok(s + i)) { out[0] = 0; return 0; }
        if (!(out[i] = s[i])) return 1;
    }
    out[i] = 0;
    return -1;
}

/* the doc object shared by modeldoc.active / modeldoc.open */
static void md_doc_json(SB *out, CModelDocEditorSession *session) {
    char path[1024];
    md_str(session->vt->GetPath(session), path, sizeof path);
    int dirty = session->vt->IsModified(session) ? 1 : 0;
    sb_add(out, "{\"path\":"); sb_addjson(out, path);
    sb_add(out, ",\"dirty\":"); sb_add(out, dirty ? "true" : "false");
    sb_add(out, ",\"isNewFile\":"); sb_add(out, session->isNewFile ? "true" : "false");
    sb_add(out, ",\"hasAsset\":"); sb_add(out, session->asset ? "true" : "false");
    /* session+S2_OFF_CModelDocEditorSession_m_Analysis is the embedded CModelDocAnalysis (an IErrorListener; ReportMessage
       receives an ErrorListenerMessage_t), not a CUtlString: seen live as binary garbage when read
       as char *. Never read; the compile outcome is the .vmdl_c mtime on the tool side. */
    sb_add(out, ",\"compileStatus\":null");
    sb_add(out, ",\"docType\":\"vmdl\"}");
}

static int op_modeldoc_render(const char *params, SB *out, OpErr *e);   /* mcp_bridge_modeldoc_render.inc */

static int op_modeldoc(const char *method, const char *params, SB *out, OpErr *e) {
    if (seq(method, "modeldoc.render")) return op_modeldoc_render(params, out, e);
    if (seq(method, "modeldoc.active")) {
        /* never errors on no doc: {doc:null, reason} */
        const char *reason = "?";
        CModelDocEditorSession *session = active_modeldoc(&reason);
        if (!session) {
            sb_add(out, "{\"doc\":null,\"reason\":"); sb_addjson(out, reason); sb_add(out, "}");
            return 1;
        }
        sb_add(out, "{\"doc\":");
        md_doc_json(out, session);
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "modeldoc.open")) {
        char path[520];
        path[0] = 0;
        if (!params || !json_string(params, "path", path, sizeof path) || !path[0]) {
            op_err(e, -32602, "InvalidParams", "path: relative to the addon content root (models/...), 1..512 chars, no '..'");
            return 0;
        }
        u32 pl = slen(path);
        if (pl > 512) { op_err(e, -32602, "InvalidParams", "path: 1..512 chars"); return 0; }
        /* every refusal below keeps a garbled path away from OpenFile, whose "Cannot open" /
           "outside the content dir" modals block the main thread until a human clicks */
        {
            const char *raw = json_find(params, "path");   /* json_string collapses escapes */
            for (const char *q = raw + 1; raw && *q && *q != '"'; q++)
                if (*q == '\\') {
                    op_err(e, -32602, "InvalidParams", "path: no JSON escapes (use '/' separators)");
                    return 0;
                }
        }
        for (u32 i = 0; i < pl; i++) {
            if ((u8)path[i] < 0x20 || (u8)path[i] == 0x7f) {
                op_err(e, -32602, "InvalidParams", "path: no control characters");
                return 0;
            }
        }
        if (path[0] == '/' || path[0] == '\\' || (pl > 1 && path[1] == ':')) {
            op_err(e, -32602, "InvalidParams",
                   "path: relative to the addon content root (models/...), not absolute");
            return 0;
        }
        if (pl < 6 || path[pl - 5] != '.' || (path[pl - 4] | 0x20) != 'v' || (path[pl - 3] | 0x20) != 'm'
            || (path[pl - 2] | 0x20) != 'd' || (path[pl - 1] | 0x20) != 'l') {
            op_err(e, -32602, "InvalidParams", "path: a .vmdl file");
            return 0;
        }
        for (u32 i = 0; i + 1 < pl; i++) {          /* only '..' needs a refusal here: the OpenFile
                                                       side resolves it, and OpenFile on a file
                                                       outside the content dir pops a modal */
            if (path[i] == '.' && path[i + 1] == '.') {
                op_err(e, -32602, "InvalidParams", "path: no '..'");
                return 0;
            }
        }
        CModelDocEditorApp *app = resolve_modeldoc();
        if (!app) { op_err(e, -32003, "NoActiveDocument", "modeldoc_unavailable"); return 0; }
        void *opened = app->vt->OpenFile(app, path, 1);   /* flags 1 = what the UI passes */
        logline("modeldoc: open ");
        logline(path);
        lognum("modeldoc: open rc ", (u64)(long long)(size_t)opened);
        const char *areason = "?";
        CModelDocEditorSession *as = active_modeldoc(&areason);   /* the opened session when it
                                                                     succeeded, else the prior one */
        sb_add(out, "{\"opened\":"); sb_add(out, opened ? "true" : "false");
        sb_add(out, ",\"path\":"); sb_addjson(out, path);
        sb_add(out, ",\"active\":");
        if (as) md_doc_json(out, as); else sb_add(out, "null");
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "modeldoc.reload")) {
        const char *reason = "?";
        CModelDocEditorSession *session = active_modeldoc(&reason);
        if (!session) { op_err(e, -32003, "NoActiveDocument", reason); return 0; }
        int dirty = session->vt->IsModified(session) ? 1 : 0;
        int force = 0;
        if (params) {
            const char *f = json_find(params, "force");
            if (f && f[0] == 't') force = 1;
        }
        if (dirty && !force) {
            op_err(e, -32016, "UnsavedChanges", "document is dirty; pass force:true to discard");
            return 0;
        }
        char path[1024];
        md_str(session->vt->GetPath(session), path, sizeof path);   /* capture before the reload */
        if (session->isNewFile || !path[0]) {       /* ReloadDocument would load "" into an
                                                       empty document: useless and destructive */
            op_err(e, -32015, "UntitledDocument", "no path to reload from");
            return 0;
        }
        session->vt->ReloadDocument(session);       /* replaces the CModelDoc: everything below
                                                       re-resolves */
        const char *areason = "?";
        CModelDocEditorSession *as = active_modeldoc(&areason);
        if (!as) logline("modeldoc: reload left no valid document");
        lognum("modeldoc: reload, dirty was ", (u64)dirty);
        sb_add(out, "{\"reloaded\":"); sb_add(out, as ? "true" : "false");
        sb_add(out, ",\"path\":"); sb_addjson(out, path);
        sb_add(out, ",\"discardedChanges\":"); sb_add(out, dirty ? "true" : "false");
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "modeldoc.save")) {
        const char *reason = "?";
        CModelDocEditorSession *session = active_modeldoc(&reason);
        if (!session) { op_err(e, -32003, "NoActiveDocument", reason); return 0; }
        if (session->isNewFile) {
            op_err(e, -32015, "UntitledDocument", "no path (Save As would open a modal)");
            return 0;
        }
        char path[1024];                            /* copied BEFORE the save: the path string may be
                                                       reallocated by SaveAndNotify */
        if (md_str(session->vt->GetPath(session), path, sizeof path) != 1 || !path[0]) {
            /* strict: a cut path handed to SaveAndNotify(path, path) would repoint the session */
            op_err(e, -32015, "UntitledDocument", "no usable path (Save As would open a modal)");
            return 0;
        }
        int dirty = session->vt->IsModified(session) ? 1 : 0;
        int saved = 1, skipped = 1;
        if (dirty) {
            skipped = 0;
            saved = session->vt->SaveAndNotify(session, path, path, 0, 0) ? 1 : 0;   /* the FileSave call */
            if (saved) session->vt->SetModified(session, 0);                        /* mirrors FileSave */
        }
        sb_add(out, "{\"saved\":"); sb_add(out, saved ? "true" : "false");
        sb_add(out, ",\"skippedClean\":"); sb_add(out, skipped ? "true" : "false");
        sb_add(out, ",\"path\":"); sb_addjson(out, path);
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "modeldoc.compile")) {
        const char *reason = "?";
        CModelDocEditorSession *session = active_modeldoc(&reason);
        if (!session) { op_err(e, -32003, "NoActiveDocument", reason); return 0; }
        if (!session->asset) {
            op_err(e, -32017, "PrerequisiteFailed", "no_asset");
            return 0;
        }
        int savedFirst = session->vt->IsModified(session) ? 1 : 0;  /* Compile saves first when modified */
        modeldoc_bind();
        g_mdCompile(session, 0);                    /* mode 0; returns nothing useful */
        /* Compile returns nothing and its status lands in a sink object (see md_doc_json):
           the outcome is unknown here - the tool checks the .vmdl_c mtime */
        sb_add(out, "{\"status\":null,\"failed\":null");
        sb_add(out, ",\"savedFirst\":"); sb_add(out, savedFirst ? "true" : "false");
        sb_add(out, "}");
        return 1;
    }
    if (seq(method, "modeldoc.nodeTypes")) {
        u8 *mb = (u8 *)g_modBase[PIN_MODELDOC];
        long long count = (long long)(*(const int *)(mb + MDG_NODETYPE_COUNT));
        u8 **descs = *(u8 ***)(mb + MDG_NODETYPE_ARRAY);
        if (count < 1 || count > 4096 || !descs || !mem_ok(descs) || !mem_ok(descs + count - 1)) {
            char why[128]; SB w; sb_init(&w, why, sizeof why);
            sb_add(&w, "node-type table unusable (count ");
            sb_addi(&w, count < 0 ? 0 : count);
            sb_add(&w, ")");
            op_err(e, -32017, "PrerequisiteFailed", why);
            return 0;
        }
        sb_add(out, "{\"count\":"); sb_addnum(out, (u64)count);
        sb_add(out, ",\"types\":[");
        int first = 1;
        for (long long i = 0; i < count; i++) {     /* desc = {u32 typeId; ...; className at +8} */
            u8 *desc = descs[i];
            if (!desc || !mem_ok(desc) || !mem_ok(desc + S2_OFF_CToolNodeTypeName_m_pszName + 7)) {
                op_err(e, -32017, "PrerequisiteFailed", "node-type descriptor unreadable");
                return 0;
            }
            u32 typeId = *(const u32 *)desc;
            char nm[128];                           /* bounded copy before the JSON writer */
            if (md_str(*(const char **)(desc + S2_OFF_CToolNodeTypeName_m_pszName), nm, sizeof nm) != 1) {
                op_err(e, -32017, "PrerequisiteFailed", "node-type className unreadable");
                return 0;
            }
            if (!first) sb_add(out, ",");
            first = 0;
            sb_add(out, "{\"typeId\":"); sb_addnum(out, typeId);
            sb_add(out, ",\"className\":"); sb_addjson(out, nm);
            sb_add(out, "}");
        }
        sb_add(out, "]}");
        return 1;
    }
    op_err(e, -32601, "MethodNotFound", method);    /* only the seven arrive here */
    return 0;
}

static int op_mesh_create(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    const char *why = "?";
    GGeom *g = params ? geom_parse(params, &why) : 0;
    if (!g) { op_err(e, -32602, "InvalidParams", params ? why : "params"); return 0; }
    CMapRootElement *root = doc->m_pRoot;
    if (!root || !mem_ok(&root->m_fileId)) { geom_free(g); op_err(e, -32603, "InternalError", "no document root"); return 0; }
    CMapDocHistory *hist = undo_open(doc, "MCP: create mesh");
    CMapMesh *mesh = g_hfn.CreateMeshElement(root->m_fileId, 0, "mcp_bridge", 0);
    if (!mesh || !is_mesh((CMapNode *)mesh)) { geom_free(g); op_err(e, -32603, "InternalError", "CreateElement<CMapMesh> failed"); return 0; }
    int faces = 0;
    if (!geo_build(mesh, g, &faces, &why)) { geom_free(g); op_err(e, -32603, "InternalError", why); return 0; }
    g_hfn.InsertNode(doc, CMapMesh_Node(mesh), 0);
    g_hfn.RecordNode(hist, CMapNode_Undoable(CMapMesh_Node(mesh)), 1);
    logline("mesh.create: ok");
    sb_add(out, "{");
    geo_mesh_summary(mesh, out);
    sb_add(out, ",\"inputVertices\":"); sb_addi(out, g->nv);
    sb_add(out, ",\"faces\":"); sb_addi(out, faces);
    sb_add(out, ",\"materials\":"); sb_addi(out, g->nmat + 1);
    sb_add(out, ",\"undoLabel\":\"MCP: create mesh\"}");
    geom_free(g);
    return 1;
}

static CMapMesh *geo_target_mesh(CMapDoc *doc, const char *params, OpErr *e) {
    long long id = -1;
    if (!params || !json_int_top(params, "nodeID", &id) || id < 0 || id > NODEID_MAX) {
        op_err(e, -32602, "InvalidParams", "nodeID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    CMapNode *n = geo_find(doc, (int)id);
    if (!n) { op_err(e, -32022, "NodeNotFound", "no node with this nodeID in the live document"); return 0; }
    if (!is_mesh(n)) { op_err(e, -32602, "InvalidParams", "node is not a mesh"); return 0; }
    return (CMapMesh *)n;
}

static int op_mesh_set_geometry(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    CMapMesh *mesh = geo_target_mesh(doc, params, e);
    if (!mesh) return 0;
    const char *why = "?";
    GGeom *g = geom_parse(params, &why);
    if (!g) { op_err(e, -32602, "InvalidParams", why); return 0; }
    CMapDocHistory *hist = undo_open(doc, "MCP: set mesh geometry");
    g_hfn.RecordNodeTree(hist, CMapNode_Undoable(CMapMesh_Node(mesh)), 1);
    if (!geo_reset_mesh(mesh)) { geom_free(g); op_err(e, -32603, "InternalError", "could not clear the mesh"); return 0; }
    int faces = 0;
    if (!geo_build(mesh, g, &faces, &why)) { geom_free(g); op_err(e, -32603, "InternalError", why); return 0; }
    logline("mesh.setGeometry: ok");
    sb_add(out, "{");
    geo_mesh_summary(mesh, out);
    sb_add(out, ",\"inputVertices\":"); sb_addi(out, g->nv);
    sb_add(out, ",\"faces\":"); sb_addi(out, faces);
    sb_add(out, ",\"undoLabel\":\"MCP: set mesh geometry\"}");
    geom_free(g);
    return 1;
}

static int op_mesh_set_material(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    CMapMesh *mesh = geo_target_mesh(doc, params, e);
    if (!mesh) return 0;
    char mat[260];
    if (!json_string(params, "material", mat, sizeof mat) || !mat[0]) { op_err(e, -32602, "InvalidParams", "material required"); return 0; }
    CHammerPolyMesh *hm = CMapMesh_PolyMesh(mesh);
    int n = hm->faces.count;
    if (n <= 0 || n > GEO_MAX_FACES * 4) { op_err(e, -32603, "InternalError", "mesh has no faces"); return 0; }
    S2MeshHandle *buf = (S2MeshHandle *)geo_alloc(sizeof(S2MeshHandle) * (u64)n);
    if (!buf) { op_err(e, -32603, "InternalError", "out of memory"); return 0; }
    S2HandleVec v; v.size = 0; v.pad = 0; v.mem = buf; v.alloc = n; v.flags = S2_UTLVEC_EXTERNAL;
    if (!g_hfn.CollectHandles(&hm->faces, &v) || v.size <= 0) {
        geo_free(buf); op_err(e, -32603, "InternalError", "could not collect face handles"); return 0;
    }
    CMapDocHistory *hist = undo_open(doc, "MCP: set mesh material");
    g_hfn.RecordNodeTree(hist, CMapNode_Undoable(CMapMesh_Node(mesh)), 1);
    g_hfn.AssignMaterialToFaces(hm, buf, v.size, mat);
    geo_free(buf);
    sb_add(out, "{");
    geo_mesh_summary(mesh, out);
    sb_add(out, ",\"faces\":"); sb_addi(out, v.size);
    sb_add(out, ",\"material\":"); sb_addjson(out, mat);
    sb_add(out, ",\"undoLabel\":\"MCP: set mesh material\"}");
    return 1;
}

static int op_node_delete(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    const char *p = params ? json_find_top(params, "nodeIDs") : 0;
    if (!p || *p != '[') { op_err(e, -32602, "InvalidParams", "nodeIDs: [int,...] required"); return 0; }
    int ids[256], n = 0;
    p = jws(p + 1);
    while (*p != ']') {
        double d;
        p = jnum(p, &d);
        /* the whole id must be an integer in the i32 range: a bigger one would alias another node
           through the (int) cast (review opus #5 / ds #1) */
        if (!p || d < 0 || d > 2147483647.0 || d != (double)(int)d || n >= 256) {
            op_err(e, -32602, "InvalidParams", "nodeIDs: up to 256 non-negative ints");
            return 0;
        }
        ids[n++] = (int)d;
        p = jws(p);
        if (*p == ',') p = jws(p + 1); else if (*p != ']') { op_err(e, -32602, "InvalidParams", "nodeIDs: malformed"); return 0; }
    }
    if (!n) { op_err(e, -32602, "InvalidParams", "nodeIDs: empty"); return 0; }
    CMapNode *nodes[256];
    for (int i = 0; i < n; i++) {
        nodes[i] = geo_find(doc, ids[i]);
        char why[96]; SB w; sb_init(&w, why, sizeof why);
        if (!nodes[i]) { sb_add(&w, "nodeID "); sb_addi(&w, ids[i]); sb_add(&w, " not found"); op_err(e, -32022, "NodeNotFound", why); return 0; }
        const char *cl = node_class(nodes[i]);
        if (seq(cl, "world") || seq(cl, "other")) { sb_add(&w, "nodeID "); sb_addi(&w, ids[i]); sb_add(&w, " cannot be deleted ("); sb_add(&w, cl); sb_add(&w, ")"); op_err(e, -32602, "InvalidParams", why); return 0; }
        if (g_hfn.NodeIsLocked(nodes[i], 0)) { sb_add(&w, "nodeID "); sb_addi(&w, ids[i]); sb_add(&w, " is locked"); op_err(e, -32018, "DocumentNotEditable", why); return 0; }
    }
    undo_open(doc, "MCP: delete nodes");
    for (int i = 0; i < n; i++)
        g_hfn.DeleteNode(doc, nodes[i]);
    sb_add(out, "{\"deleted\":[");
    for (int i = 0; i < n; i++) { if (i) sb_add(out, ","); sb_addi(out, ids[i]); }
    sb_add(out, "],\"undoLabel\":\"MCP: delete nodes\"}");
    return 1;
}

static int op_node_list(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    char cls[16]; cls[0] = 0;
    long long lim = 2000;
    if (params) { json_string(params, "class", cls, sizeof cls); json_int(params, "limit", &lim); }
    if (lim < 1 || lim > 10000) lim = 2000;
    CMapNode *wn = geo_world(doc);
    if (!wn) { op_err(e, -32603, "InternalError", "no world node"); return 0; }
    GWalk w; w.wantId = -1; w.found = 0; w.visited = 0; w.out = out; w.listed = 0; w.limit = (int)lim;
    w.cls = cls[0] ? cls : 0; w.truncated = 0;
    sb_add(out, "{\"nodes\":[");
    geo_walk(&w, wn, 0);
    sb_add(out, "],\"count\":"); sb_addi(out, w.listed);
    sb_add(out, ",\"visited\":"); sb_addi(out, w.visited);
    sb_add(out, ",\"truncated\":"); sb_add(out, w.truncated ? "true" : "false");
    sb_add(out, ",\"source\":\"live\"}");
    return 1;
}

/* ---- node.setTransform (m2.18, re-props.md Q2) ----
   The transform values are C++ fields of CMapNode (the S2_OFF_CMapPoint_m_Origin/m_Angles/m_Scales offsets
   s2sdk.h asserts), written through the vtable setters: slot 17 SetOrigin, 18 SetAngles, 19 SetScales (the mesh and
   prefab/group overrides live at the same slots). Each setter stores the field, forwards to the
   node's helpers and invalidates the transform and bounds, so no extra refresh call is needed;
   the entity overrides also move attached children. The whole param set is validated BEFORE the
   undo group opens (a refused call must not leave an empty group behind), and the group gets one
   RecordNodeTree - the same shape as Hammer's own "Clear Rotation and Scale" (the address is in
   re-props.md Q2), whose per-node order is scales then angles. */
static int op_node_set_transform(CMapDoc *doc, const char *params, SB *out, OpErr *e) {
    long long id = -1;
    if (!params || !json_find_top(params, "nodeID")) {
        op_err(e, -32602, "InvalidParams", "nodeID required");
        return 0;
    }
    if (!json_int_top(params, "nodeID", &id) || id < 0 || id > NODEID_MAX) {
        op_err(e, -32602, "InvalidParams", "nodeID: a non-negative int up to 2147483647 is required");
        return 0;
    }
    float origin[3] = { 0, 0, 0 }, angles[3] = { 0, 0, 0 }, scales[3] = { 1, 1, 1 };
    int has_origin = json_float3_top(params, "origin", origin);
    int has_angles = json_float3_top(params, "angles", angles);
    int has_scales = json_float3_top(params, "scales", scales);
    if (json_find_top(params, "origin") && !has_origin) {
        op_err(e, -32602, "InvalidParams", "origin: [x,y,z] required");
        return 0;
    }
    if (json_find_top(params, "angles") && !has_angles) {
        op_err(e, -32602, "InvalidParams", "angles: [pitch,yaw,roll] required");
        return 0;
    }
    if (json_find_top(params, "scales") && !has_scales) {
        op_err(e, -32602, "InvalidParams", "scales: [x,y,z] required");
        return 0;
    }
    if (!xform_any(has_origin, has_angles, has_scales)) {
        op_err(e, -32602, "InvalidParams", "at least one of origin, angles, scales is required");
        return 0;
    }
    if (has_origin && !xform_origin_ok(origin)) {
        /* built from XFORM_ORIGIN_MAX: the text can never lag the gate again (review W2 #5 named
           the old 65536 text while the gate had already moved to 1048576) */
        char why[96];
        xform_origin_reason(why, sizeof why);
        op_err(e, -32602, "InvalidParams", why);
        return 0;
    }
    if (has_scales && !xform_scales_ok(scales)) {
        op_err(e, -32602, "InvalidParams", "scales: each finite and in (0, 1000]");
        return 0;
    }
    /* angles: json_float3_top already refused a non-finite component (any angle is legal) */
    CMapNode *node = geo_find(doc, (int)id);
    if (!node) { op_err(e, -32022, "NodeNotFound", "no node with this nodeID in the live document"); return 0; }
    if (!obj_is(node, PIN_HAMMER, HV_MAPENTITY_VT) && !obj_is(node, PIN_HAMMER, HV_MAPMESH_VT)) {
        op_err(e, -32602, "InvalidParams", "not an entity or mesh");
        return 0;
    }
    if (g_hfn.NodeIsLocked(node, 0)) {
        op_err(e, -32018, "DocumentNotEditable", "node is locked");
        return 0;
    }
    CMapDocHistory *hist = undo_open(doc, "MCP: set transform");
    g_hfn.RecordNodeTree(hist, CMapNode_Undoable(node), 1);
    /* scales, then angles, then origin (Hammer's own order; the fields are independent) */
    if (has_scales) node->vt->SetScales(node, scales);
    if (has_angles) node->vt->SetAngles(node, angles);
    if (has_origin) node->vt->SetOrigin(node, origin);
    g_hfn.DocSetModified(doc, doc);
    float ro[3] = { 0, 0, 0 }, ra[3] = { 0, 0, 0 }, rs[3] = { 0, 0, 0 };
    int ok_o = node_origin(node, ro), ok_a = node_angles(node, ra), ok_s = node_scales(node, rs);
    sb_add(out, "{\"nodeID\":"); sb_addi(out, id);
    sb_add(out, ",\"class\":\""); sb_add(out, node_class(node)); sb_add(out, "\"");
    sb_add(out, ",\"origin\":"); if (ok_o) sb_addf3(out, ro); else sb_add(out, "null");
    sb_add(out, ",\"angles\":"); if (ok_a) sb_addf3(out, ra); else sb_add(out, "null");
    sb_add(out, ",\"scales\":"); if (ok_s) sb_addf3(out, rs); else sb_add(out, "null");
    sb_add(out, ",\"undoLabel\":\"MCP: set transform\"}");
    return 1;
}

typedef int (*GeoOp)(CMapDoc *doc, const char *params, SB *out, OpErr *e);
/* shared gate: active doc (+ editable + expectDoc for mutating ops), then the op */
static void geo_dispatch(const Req *r, SB *b, GeoOp op, int mutating) {
    const char *reason = "?";
    CMapDoc *doc = active_doc(&reason);
    if (!doc) {
        char data[192]; SB db; sb_init(&db, data, sizeof data);
        sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason); sb_add(&db, "}");
        build_error_ext(b, 1, r->id, -32003, "NoActiveDocument", data);
        return;
    }
    const char *why = "?";
    if (mutating && !doc_editable(doc, &why)) {
        char data[192]; SB db; sb_init(&db, data, sizeof data);
        sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
        build_error_ext(b, 1, r->id, -32018, "DocumentNotEditable", data);
        return;
    }
    if (mutating && !expect_doc_ok(r->params, &why)) {
        char data[192]; SB db; sb_init(&db, data, sizeof data);
        sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
        build_error_ext(b, 1, r->id, -32004, "DocumentChanged", data);
        return;
    }
    static char rbuf[512 * 1024];
    SB rb; sb_init(&rb, rbuf, sizeof rbuf);
    OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
    if (!op(doc, r->params, &rb, &e)) {
        build_error_ext(b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
    } else {
        /* m2.17: an undoable write reports the document state its own op left behind (inside this
           dispatch: nothing can run between the op and this read). Reads carry no `doc`. The tail
           goes on BEFORE the overflow check, so a result the tail would push over the buffer is
           refused with -32603 instead of being emitted cut. */
        if (mutating) sb_add_doc_tail(&rb, doc);
        if (rb.ovf) {
            /* entity.get has no `limit` param: name the real cause (review opus #17) */
            if (seq(r->method, "entity.get"))
                build_error(b, 1, r->id, -32603, "entity has too many or too large keyvalues");
            else
                build_error(b, 1, r->id, -32603, "response too large (use limit)");
        } else {
            sb_add(b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(b, r->id);
            sb_add(b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(b, rb.buf);
            sb_add(b, "}\n");
        }
    }
}


#include "mcp_bridge_game.inc"
#include "mcp_bridge_pet.inc"
#include "mcp_bridge_pet_render.inc"
#include "mcp_bridge_modeldoc_render.inc"
#include "mcp_bridge_map.inc"          /* m2.16: map.open (needs pet.inc's editor_app_root/kv3_*) */

static void exec_request(const Req *r, char *out, u32 outcap) {
    SB b; sb_init(&b, out, outcap);
    const Method *m = find_method(r->method);
    if (!m) { build_error(&b, 1, r->id, -32601, "MethodNotFound"); if (b.ovf) logline("exec: overflow"); return; }
    if (!m->impl) {
        char data[192]; SB db; sb_init(&db, data, sizeof data);
        sb_add(&db, "{\"blockedOn\":"); sb_addjson(&db, m->blockedOn ? m->blockedOn : "unknown"); sb_add(&db, "}");
        build_error_ext(&b, 1, r->id, -32011, "NotImplemented", data);
        if (b.ovf) logline("exec: overflow");
        return;
    }
    u32 mask = verified_mask();
    if ((mask & m->needs) != m->needs) {
        /* the module named in the error is the first unmet need in this order; toolframework2
           comes first because it is the prerequisite of everything else (pet.load is the first
           row that needs it alone). The two tables must stay parallel: an entry missing from
           `bits` leaves the loop matching nothing and the error names the PIN_HAMMER default. */
        static const u32 bits[] = { NEED_TF2, NEED_SCENE, NEED_ENGINE, NEED_RSV, NEED_HAMMER, NEED_CLIENT, NEED_SERVER, NEED_TIER0, NEED_SCHEMA, NEED_SOUND, NEED_MODELDOC, NEED_PET };
        static const int mods[] = { PIN_TF2, PIN_SCENESYSTEM, PIN_ENGINE2, PIN_RSV, PIN_HAMMER, PIN_CLIENT, PIN_SERVER, PIN_TIER0, PIN_SCHEMA, PIN_SOUND, PIN_MODELDOC, PIN_PET };
        _Static_assert(sizeof bits == sizeof mods, "the gate tables must be parallel");
        int mod = PIN_HAMMER;
        for (u32 i = 0; i < sizeof bits / sizeof bits[0]; i++)
            if ((m->needs & bits[i]) && !(mask & bits[i])) { mod = mods[i]; break; }
        char data[192]; SB db; sb_init(&db, data, sizeof data);
        sb_add(&db, "{\"module\":"); sb_addjson(&db, k_pinModules[mod].name);
        sb_add(&db, ",\"check\":\"identity\",\"state\":"); sb_addjson(&db, mod_state_name(mod));
        sb_add(&db, "}");
        build_error_ext(&b, 1, r->id, -32017, "PrerequisiteFailed", data);
        if (b.ovf) logline("exec: overflow");
        return;
    }
    if (seq(m->name, "entity.create")) {
        const char *reason = "?";
        CMapDoc *doc = active_doc(&reason);
        if (!doc) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32003, "NoActiveDocument", data);
        } else {
            const char *why = "?";
            if (!doc_editable(doc, &why)) {
                char data[192]; SB db; sb_init(&db, data, sizeof data);
                sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
                build_error_ext(&b, 1, r->id, -32018, "DocumentNotEditable", data);
            } else if (!expect_doc_ok(r->params, &why)) {
                char data[192]; SB db; sb_init(&db, data, sizeof data);
                sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
                build_error_ext(&b, 1, r->id, -32004, "DocumentChanged", data);
            } else {
                static char rbuf[16384];   /* up to 32 keyvalues + their `applied`/`warnings` echo */
                SB rb; sb_init(&rb, rbuf, sizeof rbuf);
                OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
                CreateTarget t;
                /* validate the params and resolve the class in the node factory BEFORE the undo
                   group exists: a refused create must not leave an empty "MCP: create entity"
                   group for undo_last to pop (review L3, the setKeyValues pattern) */
                if (!entity_create_target(r->params, &t, &e)) {
                    build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
                } else {
                    undo_open(doc, "MCP: create entity");
                    if (!entity_create_apply(doc, r->params, &t, &rb, &e)) {
                        build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
                    } else {
                        /* m2.17: the state this write left behind, appended before the overflow
                           check (a tail that does not fit is -32603, never a cut reply) */
                        sb_add_doc_tail(&rb, doc);
                        if (rb.ovf) {
                            /* the create ran but its result does not fit: say so, never emit cut
                               JSON (review ds #2) */
                            build_error(&b, 1, r->id, -32603, "response too large (use fewer keyvalues)");
                        } else {
                            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
                            sb_add(&b, rbuf);
                            sb_add(&b, "}\n");
                        }
                    }
                }
            }
        }
    } else if (seq(m->name, "undo.undo") || seq(m->name, "undo.redo")) {
        /* m2.17: `undo.redo` is the pair partner's apply-and-pop - the same operation `undo.undo`
           performs on the undo stack. Both arms share every gate; the differences are the history
           they act on and the result key that names the direction.
           Why the redo half is safe to call (protocol-diff-m2.17.md has the evidence): the two
           histories are one 4-slot CUndoHistory implementation, and pet.dll's own Redo
           (PETFN_DOC_REDO, live-verified in m2.13) is exactly "the pair partner's vtable slot 2",
           so slot 2 is direction-agnostic - each stack applies and pops its own top group. The
           local "is there anything to redo" gate is the redo stack's OWN count, never its
           `HasEntries` (whose linked-history fallback consults another root's stack). */
        const int redo = seq(m->name, "undo.redo");
        const char *why = "?";
        const char *reason = "?";
        CMapDoc *doc = active_doc(&reason);
        if (!doc) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32003, "NoActiveDocument", data);
        } else if (!doc_editable(doc, &why)) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32018, "DocumentNotEditable", data);
        } else if (!expect_doc_ok(r->params, &why)) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32004, "DocumentChanged", data);
        } else if (redo && (!g_hfn.GetRedoHistory(doc) ||
                            !obj_is(g_hfn.GetRedoHistory(doc), PIN_HAMMER, HV_HISTORY_VT))) {
            /* the pair partner is unreadable (no root history, or the class check fails): report it
               instead of calling slot 2 on a pointer of unknown shape */
            build_error_ext(&b, 1, r->id, -32017, "PrerequisiteFailed",
                            "{\"reason\":\"bad_redo_history\"}");
        } else {
            /* undo.undo acts on the ACTIVE history (doc_editable proved it is this document's own
               undo stack); undo.redo on the redo stack `GetRedoHistory` returns */
            CMapDocHistory *hist = redo ? g_hfn.GetRedoHistory(doc) : g_hfn.GetActiveHistory();
            char undoname[128];
            u32 depth0 = 0, serial0 = 0;
            history_top_info(hist, undoname, sizeof undoname, &depth0, &serial0);
            if (!depth0) {
                /* an empty stack is a RESULT (nothing was popped, nothing was sent further), never
                   an error - the POC's `undone:false` contract */
                sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{");
                sb_add(&b, redo ? "\"redone\":false,\"name\":null" : "\"undone\":false,\"name\":null");
                sb_add(&b, ",\"doc\":"); sb_add_doc_object(&b, doc);
                sb_add(&b, "}}\n");
            } else {
                hist->vt->Undo(hist);
                char name2[128];
                u32 depth1 = 0, serial1 = 0;
                history_top_info(hist, name2, sizeof name2, &depth1, &serial1);
                lognum(redo ? "redo: depth " : "undo: depth ", depth0);
                lognum(redo ? "redo: -> " : "undo: -> ", depth1);
                sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{");
                sb_add(&b, redo ? "\"redone\":" : "\"undone\":");
                sb_add(&b, depth1 < depth0 ? "true" : "false");
                /* the name of the group that was popped (read before the call) */
                sb_add(&b, ",\"name\":"); sb_addjson(&b, undoname);
                if (redo) { sb_add(&b, ",\"redoDepth\":"); sb_addnum(&b, depth1); }
                else { sb_add(&b, ",\"undoDepth\":"); sb_addnum(&b, depth1); }
                sb_add(&b, ",\"doc\":"); sb_add_doc_object(&b, doc);
                sb_add(&b, "}}\n");
            }
        }
    } else if (seq(m->name, "entity.setKeyValues")) {
        const char *why = "?";
        const char *reason = "?";
        CMapDoc *doc = active_doc(&reason);
        if (!doc) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32003, "NoActiveDocument", data);
        } else if (!doc_editable(doc, &why)) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32018, "DocumentNotEditable", data);
        } else if (!expect_doc_ok(r->params, &why)) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32004, "DocumentChanged", data);
        } else {
            static char rbuf[65536];      /* + previous/previousTruncated/valueUndo (m2.15) */
            OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
            SetKvTarget t;
            /* validate the params and resolve the node BEFORE the undo group exists: a refused call
               must not leave an empty "MCP: set keyvalues" group behind for undo_last to pop
               (review opus #13) */
            if (!set_keyvalues_target(doc, r->params, &t, &e)) {
                build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
            } else {
                undo_open(doc, "MCP: set keyvalues");
                SB rb; sb_init(&rb, rbuf, sizeof rbuf);
                set_keyvalues_apply(doc, &t, &rb);
                /* m2.17: the state this write left behind, appended before the overflow check (a
                   tail that does not fit is -32603, never a cut reply) */
                sb_add_doc_tail(&rb, doc);
                if (rb.ovf) {
                    build_error(&b, 1, r->id, -32603, "response too large (use fewer keyvalues)");
                } else {
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
                    sb_add(&b, rbuf);
                    sb_add(&b, "}\n");
                }
            }
        }
    } else if (seq(m->name, "edit.batch")) {
        const char *why = "?";
        const char *reason = "?";
        CMapDoc *doc = active_doc(&reason);
        if (!doc) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32003, "NoActiveDocument", data);
        } else if (!doc_editable(doc, &why)) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32018, "DocumentNotEditable", data);
        } else if (!expect_doc_ok(r->params, &why)) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32004, "DocumentChanged", data);
        } else {
            const char *ops = r->params ? json_find_top(r->params, "ops") : 0;
            if (!ops || *ops != '[') {
                build_error(&b, 1, r->id, -32602, "invalid params: ops");
            } else {
                undo_open(doc, batch_undo_label(ops));   /* one undo group for the whole batch (protocol §9) */
                static char results[256 * 1024];   /* hammer-mcp: <= 64 entity.create ops, <= 32 keys each */
                SB rb; sb_init(&rb, results, sizeof results);
                sb_add(&rb, "[");
                static char opbuf[8192], pbuf[8192], rbuf[65536];   /* op params are truncated at pbuf: hammer-mcp keeps each op < 6000 B */
                char handles[16][16];
                int hcount = 0, failed = -1, idx = 0, first = 1, batch_ovf = 0;
                /* the nodes this group has already recorded (review W2 #7): a created node's id
                   (from its create reply) and every node a setKeyValues op wrote. The old rule
                   keyed on the 16-handle table only, so a 17th create or a second setKeyValues on
                   an existing node called RecordNodeTree twice in one group. */
                int recorded[32];
                int rcount = 0;
                const char *p = ops + 1;
                for (;;) {
                    const char *before = p;    /* batch_op_next advances or returns 0; never spin */
                    if (!batch_op_next(&p, opbuf, sizeof opbuf, pbuf, sizeof pbuf)) break;
                    if (p == before) { logline("edit.batch: op splitter stalled"); break; }
                    char method[64];
                    method[0] = 0;
                    json_string(opbuf, "method", method, sizeof method);
                    batch_subst_handle(pbuf, sizeof pbuf, handles, hcount);
                    SB ob; sb_init(&ob, rbuf, sizeof rbuf);
                    OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
                    int ok = 0;
                    if (seq(method, "entity.create")) ok = op_entity_create(doc, pbuf, &ob, &e);
                    else if (seq(method, "entity.setKeyValues")) {
                        /* m2.18: a node this group already recorded - one the batch created (its
                           CreateNodeByName recorded it in this group, re-props.md Q1) or one an
                           earlier setKeyValues op of this batch wrote - must not RecordNodeTree
                           again: Hammer records each node once per group and a second record's
                           effect on the undo is unverified (review W2 #7). Any other node records
                           once, which is what makes the group a real value undo. */
                        int rec = 1;
                        int kv_id = -1;      /* the node this op writes, when it resolves one */
                        char hb[16];
                        hb[0] = 0;
                        if (json_string_top(pbuf, "handle", hb, sizeof hb) && hb[0]) {
                            const char *hwhy = "?";
                            CMapNode *hn = (CMapNode *)handle_lookup(hb, &hwhy);
                            if (hn) node_id_of(hn, &kv_id);
                        } else {
                            long long nid = -1;
                            if (json_int_top(pbuf, "nodeID", &nid)) kv_id = (int)nid;
                        }
                        if (kv_id >= 0) rec = batch_record_decision(recorded, &rcount, 32, kv_id);
                        ok = op_set_keyvalues(doc, pbuf, &ob, &e, rec);
                    } else op_err(&e, -32011, "NotImplemented", method);
                    if (!first) sb_add(&rb, ",");
                    first = 0;
                    if (!ok) {
                        failed = idx;
                        sb_add(&rb, "{\"error\":{\"code\":");
                        sb_addi(&rb, e.code);
                        sb_add(&rb, ",\"message\":"); sb_addjson(&rb, e.name);
                        sb_add(&rb, ",\"data\":"); sb_add(&rb, e.data[0] ? e.data : "{}");
                        sb_add(&rb, "}}");
                        break;
                    }
                    if (ob.ovf) batch_ovf = 1;
                    sb_add(&rb, rbuf);
                    char htok[16];
                    htok[0] = 0;
                    long long nid2 = -1;
                    if (seq(method, "entity.create")) {
                        if (json_string(rbuf, "handle", htok, sizeof htok) && hcount < 16)
                            cpy(handles[hcount++], htok, sizeof handles[0]);
                        /* the created node is recorded by its own create: later setKeyValues ops
                           on it (by `$N` handle or by this id) must not record it again */
                        if (json_int_top(rbuf, "nodeID", &nid2) && rcount < 32)
                            recorded[rcount++] = (int)nid2;
                    }
                    idx++;
                }
                sb_add(&rb, "]");
                if (batch_ovf || rb.ovf) {
                    /* the ops ran but their results do not fit: say so instead of sending cut JSON */
                    build_error_ext(&b, 1, r->id, -32012, "PartialFailure",
                                    "{\"failedIndex\":null,\"reason\":\"batch result too large; the ops ran - verify with entity.find, do not retry\"}");
                } else if (failed >= 0) {
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"error\":{\"code\":-32012,\"message\":\"PartialFailure\",\"data\":{\"results\":");
                    sb_add(&b, results);
                    sb_add(&b, ",\"failedIndex\":");
                    sb_addi(&b, failed);
                    sb_add(&b, "}}}\n");
                } else {
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"results\":");
                    sb_add(&b, results);
                    sb_add(&b, ",\"failedIndex\":null,\"doc\":");
                    sb_add_doc_object(&b, doc);   /* m2.17: one group, read inside this dispatch */
                    sb_add(&b, "}}\n");
                }
            }
        }
    } else if (seq(m->name, "mesh.create")) {
        geo_dispatch(r, &b, op_mesh_create, 1);
    } else if (seq(m->name, "mesh.setGeometry")) {
        geo_dispatch(r, &b, op_mesh_set_geometry, 1);
    } else if (seq(m->name, "mesh.setMaterial")) {
        geo_dispatch(r, &b, op_mesh_set_material, 1);
    } else if (seq(m->name, "node.delete")) {
        geo_dispatch(r, &b, op_node_delete, 1);
    } else if (seq(m->name, "node.list")) {
        geo_dispatch(r, &b, op_node_list, 0);
    } else if (seq(m->name, "node.setTransform")) {
        geo_dispatch(r, &b, op_node_set_transform, 1);
    } else if (seq(m->name, "entity.addConnections")) {
        geo_dispatch(r, &b, op_entity_add_connections, 1);
    } else if (seq(m->name, "entity.removeConnections")) {
        geo_dispatch(r, &b, op_entity_remove_connections, 1);
    } else if (seq(m->name, "entity.tieMeshes")) {
        geo_dispatch(r, &b, op_entity_tie_meshes, 1);
    } else if (seq(m->name, "entity.untie")) {
        geo_dispatch(r, &b, op_entity_untie, 1);
    } else if (seq(m->name, "entity.get")) {
        geo_dispatch(r, &b, op_entity_get, 0);
    } else if (seq(m->name, "entity.find")) {
        geo_dispatch(r, &b, op_entity_find, 0);
    } else if (seq(m->name, "test.checkpoint") || seq(m->name, "test.active") || seq(m->name, "console.exec") ||
               seq(m->name, "game.tick") || seq(m->name, "game.saveRestore")) {
        static char rbuf[512];
        SB rb; sb_init(&rb, rbuf, sizeof rbuf);
        OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
        if (!op_test(m->name, r->params, &rb, &e)) {
            build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
        } else {
            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(&b, rbuf);
            sb_add(&b, "}\n");
        }
    } else if (seq(m->name, "game.local") || seq(m->name, "game.weapons") || seq(m->name, "game.radar") ||
               seq(m->name, "game.selectWeapon") || seq(m->name, "game.schemaProbe") ||
               seq(m->name, "game.rules") || seq(m->name, "game.entities")) {
        static char rbuf[98304];                 /* radar: up to 64 players + bomb + hostages; entities: up to 256 x ~250 B */
        SB rb; sb_init(&rb, rbuf, sizeof rbuf);
        OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
        if (!op_game_state(m->name, r->params, &rb, &e)) {
            build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
        } else if (rb.ovf) {
            build_error(&b, 1, r->id, -32603, "InternalError: game state result too large");
        } else {
            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(&b, rbuf);
            sb_add(&b, "}\n");
        }
    } else if (m->name[0] == 's' && m->name[1] == 'o' && m->name[5] == '.') {   /* sound.* */
        static char rbuf[98304];                 /* drain: up to 256 events (~300 B each), rest requeued */
        SB rb; sb_init(&rb, rbuf, sizeof rbuf);
        OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
        if (!op_sound(m->name, r->params, &rb, &e)) {
            build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
        } else if (rb.ovf) {
            build_error(&b, 1, r->id, -32603, "InternalError: sound drain result too large");
        } else {
            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(&b, rbuf);
            sb_add(&b, "}\n");
        }
    } else if (m->name[0] == 'i' && m->name[1] == 'n' && m->name[5] == '.') {   /* input.* */
        static char rbuf[2048];
        SB rb; sb_init(&rb, rbuf, sizeof rbuf);
        OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
        if (!op_input(m->name, r->params, &rb, &e)) {
            build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
        } else {
            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(&b, rbuf);
            sb_add(&b, "}\n");
        }
    } else if (seq(m->name, "world.render")) {
        static char rbuf[4096];
        SB rb; sb_init(&rb, rbuf, sizeof rbuf);
        OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
        if (!op_world_render(r->params, &rb, &e)) {
            build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
        } else {
            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(&b, rbuf);
            sb_add(&b, "}\n");
        }
    } else if (seq(m->name, "view.render")) {
        const char *reason = "?";
        CMapDoc *doc = active_doc(&reason);
        if (!doc) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32003, "NoActiveDocument", data);
        } else {
            static char rbuf[4096];
            SB rb; sb_init(&rb, rbuf, sizeof rbuf);
            OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
            if (!op_view_render(doc, r->params, &rb, &e)) {
                build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
            } else {
                sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
                sb_add(&b, rbuf);
                sb_add(&b, "}\n");
            }
        }
    } else if (seq(m->name, "map.open")) {
        static char rbuf[2048];                  /* the path + epoch; fixed shape (m2.16) */
        SB rb; sb_init(&rb, rbuf, sizeof rbuf);
        OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
        if (!op_map_open(r->params, &rb, &e)) {
            build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
        } else if (rb.ovf) {
            build_error(&b, 1, r->id, -32603, "InternalError: map.open result too large");
        } else {
            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(&b, rb.buf);
            sb_add(&b, "}\n");
        }
    } else if (seq(m->name, "map.reload")) {
        const char *why = "?";
        const char *reason = "?";
        CMapDoc *doc = active_doc(&reason);
        if (!doc) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32003, "NoActiveDocument", data);
        } else if (!doc_editable(doc, &why)) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32018, "DocumentNotEditable", data);
        } else if (!expect_doc_ok(r->params, &why)) {
            /* the document the caller checked with doc.active is not this one any more: refuse
               instead of reloading (and, with force, discarding) the other document (review L2) */
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32004, "DocumentChanged", data);
        } else {
            CMapDocSession *docSession = CMapDoc_Session(doc);
            CHammerEditorSession *session = CMapDocSession_Owner(docSession);
            u8 isNew = session ? session->m_bIsNew : 1;
            char *pathp = CMapDocSession_Path(docSession);
            int dirty = g_hfn.DocOrDependencyChanged(doc, 0);
            int force = 0;
            if (r->params) {
                const char *f = json_find(r->params, "force");
                if (f && f[0] == 't') force = 1;
            }
            if (isNew || !pathp || !*pathp) {
                /* a new/untitled document has no file to reload from: the same gate, code and data
                   text `map.save` answers with, before anything is called (review M1). What
                   ReloadFromDisk would do with no path is unverified (a modal, a no-op or worse) -
                   it is never called. */
                build_error_ext(&b, 1, r->id, -32015, "UntitledDocument", "{\"reason\":\"no path (Save-As is modal)\"}");
            } else if (dirty && !force) {
                build_error_ext(&b, 1, r->id, -32016, "UnsavedChanges",
                                "{\"reason\":\"document is dirty; pass force:true to discard\"}");
            } else {
                /* review-5 L2: the path text for the reply is copied into a local buffer BEFORE
                   ReloadFromDisk. The reload body plausibly rebinds the session or reassigns its
                   CUtlString (its tail calls the document-manager reload helper and then a
                   session function - the addresses are in review-opus-5.md L2), so reading
                   `pathp` after the call would be a read of possibly freed memory. `md_str`
                   probes the source as it copies, so a stale pointer still cannot fault. The
                   copy is what the reply reports. */
                char pathbuf[512];
                pathbuf[0] = 0;
                md_str(pathp, pathbuf, sizeof pathbuf);
                /* ReloadFromDisk (S2_SLOT_CBaseEditorSession_ReloadDocument; `CHammerEditorSession` slot 7,
                   sigs/signatures/hammer.json) materialises NO return value: its body ends in a tail
                   jump to the title/view refresh function ("the same tail as the save path",
                   idabins file-edit-reload-compile.md §A step 4), whose epilogue pops its frame and
                   returns without writing rax - so no success/failure can be read here, unlike Save
                   (slot 18, whose return map.save does check). The slot is declared `void` in
                   bridge/s2sdk.h. `reloaded:true` below therefore means "the call was made on a
                   document that has a path", not "the file was re-read"; the disassembly evidence
                   is in protocol-diff-m2.16.md. Do not invent a return type. */
                session->vt->ReloadFromDisk(session);
                doc_fingerprint();          /* reload changes the fingerprint -> epoch bump */
                sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"reloaded\":true,\"path\":");
                sb_addjson(&b, pathbuf[0] ? pathbuf : "");
                sb_add(&b, ",\"discardedChanges\":");
                sb_add(&b, dirty ? "true" : "false");
                sb_add(&b, ",\"epoch\":"); sb_addnum(&b, g_fp.epoch);
                sb_add(&b, "}}\n");
            }
        }
    } else if (seq(m->name, "map.save")) {
        const char *reason = "?";
        CMapDoc *doc = active_doc(&reason);
        if (!doc) {
            char data[192]; SB db; sb_init(&db, data, sizeof data);
            sb_add(&db, "{\"reason\":"); sb_addjson(&db, reason); sb_add(&db, "}");
            build_error_ext(&b, 1, r->id, -32003, "NoActiveDocument", data);
        } else {
            const char *why = "?";
            if (!doc_editable(doc, &why)) {
                char data[192]; SB db; sb_init(&db, data, sizeof data);
                sb_add(&db, "{\"reason\":"); sb_addjson(&db, why); sb_add(&db, "}");
                build_error_ext(&b, 1, r->id, -32018, "DocumentNotEditable", data);
            } else {
                CMapDocSession *docSession = CMapDoc_Session(doc);
                CHammerEditorSession *session = CMapDocSession_Owner(docSession);
                u8 isNew = session ? session->m_bIsNew : 1;
                char *pathp = CMapDocSession_Path(docSession);
                if (isNew || !pathp || !*pathp) {
                    build_error_ext(&b, 1, r->id, -32015, "UntitledDocument", "{\"reason\":\"no path (Save-As is modal)\"}");
                } else {
                    int dirty = g_hfn.DocOrDependencyChanged(doc, 0);
                    int saved = 1, skipped = 1;
                    if (dirty) {
                        skipped = 0;
                        u8 rc = session->vt->Save(session, pathp, pathp, 0, 0);
                        saved = rc ? 1 : 0;
                    }
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"saved\":");
                    sb_add(&b, saved ? "true" : "false");
                    sb_add(&b, ",\"path\":"); sb_addjson(&b, pathp);
                    sb_add(&b, ",\"skippedClean\":"); sb_add(&b, skipped ? "true" : "false");
                    sb_add(&b, "}}\n");
                }
            }
        }
    } else if (m->name[0] == 'm' && m->name[1] == 'o' && m->name[2] == 'd' && m->name[3] == 'e' &&
               m->name[4] == 'l' && m->name[5] == 'd' && m->name[6] == 'o' && m->name[7] == 'c' &&
               m->name[8] == '.') {   /* modeldoc.* (m2.11; render m2.14) */
        static char rbuf[256 * 1024];            /* nodeTypes: up to 4096 descriptors x ~48 B */
        SB rb; sb_init(&rb, rbuf, sizeof rbuf);
        OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
        if (!op_modeldoc(m->name, r->params, &rb, &e)) {
            build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
        } else if (rb.ovf) {
            build_error(&b, 1, r->id, -32603, "InternalError: modeldoc result too large");
        } else {
            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(&b, rbuf);
            sb_add(&b, "}\n");
        }
    } else if (m->name[0] == 'p' && m->name[1] == 'e' && m->name[2] == 't' && m->name[3] == '.') {
        /* pet.* (m2.12) */
        static char rbuf[MAX_LINE - 2048];       /* the operator registry, a doc's arrays, or pet.text (the reply line is MAX_LINE) */
        SB rb; sb_init(&rb, rbuf, sizeof rbuf);
        OpErr e; e.code = 0; e.name = 0; e.data[0] = 0;
        if (!op_pet(m->name, r->params, &rb, &e)) {
            build_error_ext(&b, 1, r->id, e.code, e.name, e.data[0] ? e.data : 0);
        } else if (rb.ovf) {
            build_error(&b, 1, r->id, -32603, "InternalError: pet result too large");
        } else {
            sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, r->id);
            sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
            sb_add(&b, rb.buf);
            sb_add(&b, "}\n");
        }
    } else if (seq(m->name, "doc.active")) {
        const char *reason = "?";
        CMapDoc *doc = active_doc(&reason);
        doc_fingerprint();                 /* epoch/id current for this answer */
        sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":");
        sb_addjson(&b, r->id);
        sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{");
        if (!doc) {
            sb_add(&b, "\"doc\":null,\"reason\":");
            sb_addjson(&b, reason);
        } else {
            CMapDocSession *docSession = CMapDoc_Session(doc);
            u8 *hb = (u8 *)g_modBase[PIN_HAMMER];
            char *pathp = CMapDocSession_Path(docSession);
            u32 active = doc->m_bActive;
            CHammerEditorSession *session = CMapDocSession_Owner(docSession);
            u32 isNew = session ? session->m_bIsNew : 0;
            int dirty = -1;                                          /* -1 -> null */
            void *mgr = *(void **)(hb + HG_DOCDATA_MANAGER);
            if (mgr) dirty = g_hfn.ChangedSinceSave(mgr, doc) ? 1 : 0;
            const char *why = "?";
            int editable = doc_editable(doc, &why);
            char undoname[128];
            u32 depth = 0, serial = 0;
            undo_group_info(doc, undoname, sizeof undoname, &depth, &serial);
            sb_add(&b, "\"doc\":{\"id\":\"d"); sb_addnum(&b, g_fp.docSeq);
            sb_add(&b, "\",\"path\":"); sb_addjson(&b, pathp ? pathp : "");
            sb_add(&b, ",\"dirty\":");
            if (dirty < 0) sb_add(&b, "null"); else sb_add(&b, dirty ? "true" : "false");
            sb_add(&b, ",\"active\":");
            sb_add(&b, active ? "true" : "false");
            sb_add(&b, ",\"editable\":");
            sb_add(&b, editable ? "true" : "false");
            sb_add(&b, ",\"notEditableReason\":");
            sb_addjson(&b, editable ? "ok" : why);
            sb_add(&b, ",\"epoch\":");
            sb_addnum(&b, g_fp.epoch);
            sb_add(&b, ",\"docType\":\"vmap\",\"isNewFile\":");
            sb_add(&b, isNew ? "true" : "false");
            sb_add(&b, ",\"undoName\":");
            if (depth) sb_addjson(&b, undoname); else sb_add(&b, "null");
            sb_add(&b, ",\"undoDepth\":");
            sb_addnum(&b, depth);
            sb_add(&b, ",\"topSerial\":");
            sb_addnum(&b, serial);
            sb_add(&b, "}");       /* close doc */
        }
        sb_add(&b, "}}\n");        /* close result + envelope */
    } else {
        build_error(&b, 1, r->id, -32601, "MethodNotFound");
    }
    if (b.ovf) logline("exec: response buffer overflow");
}

/* main-thread drain (slot 13) */
static void drain(void) {
    static volatile int draining = 0;
    if (draining) return;                    /* re-entrancy guard (modal nested loop) */
    draining = 1;
    u64 t0 = GetTickCount64();
    for (int n = 0; n < 4; n++) {
        if (!out_space()) break;             /* P0-6: never execute without room to answer */
        Req r;
        if (!q_pop(&r)) break;
        static char out[MAX_LINE];
        if (r.deadlineMs) {
            u64 now = GetTickCount64();
            if (now > r.receivedAt + r.deadlineMs) {
                SB b; sb_init(&b, out, sizeof out);
                build_error(&b, 1, r.id, -32008, "QueueTimeout");
                out_push(out, r.gen);
                if (r.params) HeapFree(GetProcessHeap(), 0, r.params);
                continue;
            }
        }
        g_inflightStart = GetTickCount64();
        exec_request(&r, out, sizeof out);
        g_inflightStart = 0;
        out_push(out, r.gen);
        if (r.params) HeapFree(GetProcessHeap(), 0, r.params);
        if (GetTickCount64() - t0 > 8) break;
    }
    draining = 0;
}

/* ---------------- listener thread ---------------- */
static int write_discovery(void);            /* defined below */

/* wait until no outbound record for this generation is pending (bounded) */
static void flush_outbound(u32 gen, u32 timeoutMs) {
    u64 t0 = GetTickCount64();
    while (GetTickCount64() - t0 < timeoutMs) {
        int pending = 0;
        EnterCriticalSection(g_ocs);
        for (int i = 0; i < g_oCount; i++)
            if (g_out[(g_oHead + i) % OUT_CAP].gen == gen) { pending = 1; break; }
        LeaveCriticalSection(g_ocs);
        if (!pending) return;
        Sleep(10);
    }
}

static u32 __stdcall ipc_thread(void *param) {
    (void)param;
    logline("ipc: thread start");
    unsigned char wsa[512];
    if (WSAStartup(0x0202, wsa) != 0) { logline("ipc: WSAStartup failed"); return 0; }

    SOCKET ls = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    if (ls == INVALID_SOCKET) { logline("ipc: socket failed"); return 0; }
    struct { u16 family; u16 port; u8 addr[4]; u8 pad[8]; } sa;
    for (u32 i = 0; i < sizeof sa; i++) ((u8 *)&sa)[i] = 0;
    sa.family = AF_INET;
    sa.addr[0] = 127; sa.addr[1] = 0; sa.addr[2] = 0; sa.addr[3] = 1;   /* 127.0.0.1 */
    if (bind(ls, &sa, sizeof sa) != 0) { logline("ipc: bind failed"); closesocket(ls); return 0; }
    int namelen = sizeof sa;
    if (getsockname(ls, &sa, &namelen) != 0) { logline("ipc: getsockname failed"); closesocket(ls); return 0; }
    g_port = (u16)((sa.port >> 8) | (sa.port << 8));
    g_listener = ls;
    lognum("ipc: listening port ", g_port);
    if (listen(ls, 8) != 0) { logline("ipc: listen failed"); closesocket(ls); return 0; }
    if (!write_discovery()) logline("ipc: discovery write failed");

    char *line = (char *)HeapAlloc(GetProcessHeap(), 0, MAX_LINE);
    char *resp = (char *)HeapAlloc(GetProcessHeap(), 0, MAX_LINE);
    if (!line || !resp) { logline("ipc: buffer alloc failed"); return 0; }

    SOCKET client = INVALID_SOCKET;
    u32 gen = 0;
    int authed = 0, overflow = 0, close_after_reply = 0;
    u32 linelen = 0;
    u64 helloDeadline = 0;
    static char cbuf[4096];

    for (;;) {
        if (g_shuttingDown) break;
        fd_set rfds;
        FD_ZERO(&rfds);
        FD_SET(&rfds, ls);
        if (client != INVALID_SOCKET) FD_SET(&rfds, client);
        struct { int sec; int usec; } tv;
        tv.sec = 0; tv.usec = 100000;                       /* 100 ms tick */
        int sel = select(0, &rfds, 0, 0, &tv);
        u64 now = GetTickCount64();

        /* new connection: accept it even while another is being served, so
           extras can be rejected with Busy instead of waiting in the backlog */
        if (sel > 0 && fd_isset(&rfds, ls)) {
            int alen = sizeof sa;
            SOCKET c = accept(ls, &sa, &alen);
            if (c != INVALID_SOCKET) {
                if (client != INVALID_SOCKET || g_authedClient != INVALID_SOCKET) {
                    SB b; sb_init(&b, resp, MAX_LINE);
                    build_error(&b, 0, 0, -32007, "Busy");
                    send_all(c, resp, b.len);
                    shutdown(c, SD_BOTH);
                    closesocket(c);
                    logline("ipc: extra client -> Busy");
                } else {
                    client = c;
                    gen = g_nextGen++;
                    authed = 0; linelen = 0; overflow = 0; close_after_reply = 0;
                    helloDeadline = now + 5000;
                    lognum("ipc: client connected gen=", gen);
                }
            }
        }

        /* bytes from the served connection */
        if (client != INVALID_SOCKET && sel > 0 && fd_isset(&rfds, client)) {
            int n = recv(client, cbuf, sizeof cbuf, 0);
            if (n <= 0) {
                q_purge_gen(gen);
                if (authed) { g_authedClient = INVALID_SOCKET; g_clientGen = 0; }
                closesocket(client);
                client = INVALID_SOCKET;
                logline("ipc: client closed");
                continue;
            }
            for (int i = 0; i < n && !close_after_reply; i++) {
                char ch = cbuf[i];
                if (ch != '\n') {
                    if (linelen + 1 < MAX_LINE) line[linelen++] = ch;
                    else overflow = 1;
                    continue;
                }
                line[linelen] = 0;
                u32 len = linelen;
                linelen = 0;
                if (overflow) {
                    SB b; sb_init(&b, resp, MAX_LINE);
                    build_error(&b, 0, 0, -32600, "line too long");
                    send_all(client, resp, b.len);
                    overflow = 0;
                    close_after_reply = 1;
                    break;
                }
                if (!line[0]) continue;

                char idbuf[128]; idbuf[0] = 0;
                char method[64]; method[0] = 0;
                int hasId = json_string(line, "id", idbuf, sizeof idbuf);
                int hasMethod = json_string(line, "method", method, sizeof method);

                if (!hasMethod || !json_find(line, "jsonrpc")) {
                    SB b; sb_init(&b, resp, MAX_LINE);
                    build_error(&b, hasId, idbuf, -32700, "parse error");
                    if (!authed) { send_all(client, resp, b.len); close_after_reply = 1; }
                    else out_push(resp, gen);
                    continue;
                }

                if (!authed) {
                    /* the first frame must be control.hello (id optional) */
                    if (!seq(method, "control.hello")) {
                        SB b; sb_init(&b, resp, MAX_LINE);
                        build_error(&b, hasId, idbuf, -32001, "NotAuthenticated");
                        send_all(client, resp, b.len);
                        close_after_reply = 1;
                        break;
                    }
                    long long proto = 0;
                    char tokbuf[128]; tokbuf[0] = 0;
                    json_int(line, "protocol", &proto);
                    json_string(line, "token", tokbuf, sizeof tokbuf);
                    if (proto != 1) {
                        SB b; sb_init(&b, resp, MAX_LINE);
                        build_error(&b, hasId, idbuf, -32002, "VersionMismatch");
                        send_all(client, resp, b.len);
                        logline("ipc: hello protocol mismatch -> close");
                        close_after_reply = 1;
                        break;
                    }
                    if (!ct_eq(tokbuf, g_token)) {
                        SB b; sb_init(&b, resp, MAX_LINE);
                        build_error(&b, hasId, idbuf, -32001, "NotAuthenticated");
                        send_all(client, resp, b.len);
                        logline("ipc: hello bad token -> close");
                        close_after_reply = 1;
                        break;
                    }
                    authed = 1;
                    g_authedClient = client;
                    g_clientGen = gen;
                    SB b; sb_init(&b, resp, MAX_LINE);
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":");
                    if (hasId) sb_addjson(&b, idbuf); else sb_add(&b, "null");
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"protocol\":1,\"bridgeVersion\":\"" BRIDGE_BUILD "\",\"gameBuild\":\"" GAME_BUILD "\",\"capabilities\":");
                    build_caps(&b);
                    sb_add(&b, "}");
                    sb_add(&b, "}\n");
                    send_all(client, resp, b.len);      /* the auth gate: deliver synchronously */
                    logline("ipc: hello ok");
                    continue;
                }

                if (!hasId) {   /* v1 has no client notifications */
                    SB b; sb_init(&b, resp, MAX_LINE);
                    build_error(&b, 0, 0, -32600, "invalid request: id required");
                    out_push(resp, gen);
                    continue;
                }

                SB b; sb_init(&b, resp, MAX_LINE);
                if (seq(method, "control.ping")) {
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, idbuf);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"ok\":true,\"uptimeMs\":");
                    sb_addnum(&b, now - g_startTick);
                    sb_add(&b, ",\"mainThreadAgeMs\":");
                    /* one read per volatile + saturating difference: slot 13 can store a
                       tick newer than `now` between the two reads, and the unsigned
                       difference then wrapped to 2^64-1 ("blocked" to every client) */
                    u64 last = g_lastTick;
                    sb_addnum(&b, (last && now > last) ? now - last : 0);
                    sb_add(&b, ",\"drainBlocked\":");
                    u64 inflight = g_inflightStart;
                    sb_add(&b, (inflight && now > inflight && now - inflight > 250) ? "true" : "false");
                    sb_add(&b, ",\"inflight\":");
                    sb_addnum(&b, inflight ? 1 : 0);
                    sb_add(&b, ",\"queueDepth\":");
                    sb_addnum(&b, (u64)q_depth());
                    sb_add(&b, ",\"modules\":");
                    sb_add(&b, "{");
                    for (int mi = 0; mi < PIN_MODULE_COUNT; mi++) {
                        if (mi) sb_add(&b, ",");
                        sb_addjson(&b, k_pinModules[mi].name); sb_add(&b, ":\"");
                        sb_add(&b, mod_state_name(mi)); sb_add(&b, "\"");
                    }
                    sb_add(&b, "}");
                    sb_add(&b, ",\"revision\":");
                    sb_addnum(&b, (u64)g_capsRevision);
                    sb_add(&b, ",\"shuttingDown\":");
                    sb_add(&b, g_shuttingDown ? "true" : "false");
                    sb_add(&b, "}}\n");
                    out_push(resp, gen);
                } else if (seq(method, "control.status")) {
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, idbuf);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"queue\":[");
                    EnterCriticalSection(g_qcs);
                    for (int qi = 0; qi < g_qCount; qi++) {
                        Req *rq = &g_q[(g_qHead + qi) % QCAP];
                        if (qi) sb_add(&b, ",");
                        sb_add(&b, "{\"id\":"); sb_addjson(&b, rq->id);
                        sb_add(&b, ",\"method\":"); sb_addjson(&b, rq->method);
                        sb_add(&b, ",\"ageMs\":");
                        sb_addnum(&b, GetTickCount64() - rq->receivedAt);
                        sb_add(&b, "}");
                    }
                    LeaveCriticalSection(g_qcs);
                    sb_add(&b, "],\"jobs\":[],\"listener\":");
                    sb_add(&b, (g_listener != INVALID_SOCKET) ? "true" : "false");
                    sb_add(&b, ",\"drainBlocked\":false,\"inflight\":");
                    sb_addnum(&b, g_inflightStart ? 1 : 0);
                    sb_add(&b, ",\"revision\":");
                    sb_addnum(&b, (u64)g_capsRevision);
                    sb_add(&b, ",\"shuttingDown\":");
                    sb_add(&b, g_shuttingDown ? "true" : "false");
                    sb_add(&b, "}}\n");
                    out_push(resp, gen);
                } else if (seq(method, "control.capabilities")) {
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, idbuf);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":");
                    build_caps(&b);
                    sb_add(&b, "}\n");     /* result IS the capabilities object */
                    out_push(resp, gen);
                } else if (seq(method, "control.version")) {
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, idbuf);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"protocol\":1,\"bridgeVersion\":\"" BRIDGE_BUILD "\",\"gameBuild\":\"" GAME_BUILD "\",\"hostModules\":[");
                    for (int mi = 0; mi < PIN_MODULE_COUNT; mi++) {
                        if (mi) sb_add(&b, ",");
                        sb_add(&b, "{\"name\":"); sb_addjson(&b, k_pinModules[mi].name);
                        sb_add(&b, ",\"state\":"); sb_addjson(&b, mod_state_name(mi));
                        sb_add(&b, ",\"timeDateStamp\":");
                        char hexbuf[16];
                        u32 v = k_pinModules[mi].timeStamp;
                        for (int h = 0; h < 8; h++) { u32 nib = (v >> (28 - 4 * h)) & 15; hexbuf[h] = (char)(nib < 10 ? '0' + nib : 'a' + nib - 10); }
                        hexbuf[8] = 0;
                        sb_addjson(&b, hexbuf);
                        sb_add(&b, "}");
                    }
                    sb_add(&b, "]}}\n");
                    out_push(resp, gen);
                } else if (seq(method, "control.cancel")) {
                    const char *pp = json_find(line, "params");
                    int found = 0;
                    if (pp) {
                        char pid[128]; pid[0] = 0;
                        if (json_string(pp, "id", pid, sizeof pid)) found = q_remove_id(pid);
                    }
                    if (found) {
                        sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, idbuf);
                        sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"cancelled\":true}}\n");
                    } else {
                        build_error_ext(&b, 1, idbuf, -32010, "NotCancellable", "{\"reason\":\"not queued or already running\"}");
                    }
                    out_push(resp, gen);
                } else if (seq(method, "control.shutdown")) {
                    sb_add(&b, "{\"jsonrpc\":\"2.0\",\"id\":"); sb_addjson(&b, idbuf);
                    sb_add(&b, ",\"bridge\":{\"protocol\":1,\"build\":\"" BRIDGE_BUILD "\"},\"result\":{\"closing\":true}}\n");
                    flush_outbound(gen, 1000);          /* let queued drain replies finish first */
                    send_all(client, resp, b.len);
                    logline("ipc: control.shutdown");
                    g_shuttingDown = 1;
                    WakeAllConditionVariable(g_qcv);
                    WakeAllConditionVariable(g_ocv);
                    if (g_listener != INVALID_SOCKET) shutdown(g_listener, SD_BOTH);
                    close_after_reply = 1;
                    break;
                } else if (method[0] == 'c' && method[1] == 'o' && method[2] == 'n' && method[3] == 't' &&
                           method[4] == 'r' && method[5] == 'o' && method[6] == 'l' && method[7] == '.') {
                    build_error(&b, 1, idbuf, -32601, "MethodNotFound");
                    out_push(resp, gen);
                } else {
                    const Method *mm = find_method(method);
                    if (!mm) {
                        build_error(&b, 1, idbuf, -32601, "MethodNotFound");
                        out_push(resp, gen);
                    } else if (!mm->impl) {
                        char data[192]; SB db; sb_init(&db, data, sizeof data);
                        sb_add(&db, "{\"blockedOn\":");
                        sb_addjson(&db, mm->blockedOn ? mm->blockedOn : "unknown");
                        sb_add(&db, "}");
                        build_error_ext(&b, 1, idbuf, -32011, "NotImplemented", data);
                        out_push(resp, gen);
                    } else if (mm->needs == 0) {
                        /* needs==0 methods are the control.* fast path above */
                        build_error(&b, 1, idbuf, -32601, "MethodNotFound");
                        out_push(resp, gen);
                    } else {
                        Req rq;
                        cpy(rq.id, idbuf, sizeof rq.id);
                        cpy(rq.method, method, sizeof rq.method);
                        rq.params = 0;
                        const char *pp = json_find(line, "params");
                        if (pp && *pp == '{') {
                            /* braces inside JSON strings do not count (m2.13: pet.apply carries
                               whole .vpcf texts, whose string values may hold `{`/`}`) */
                            u32 plen = 0, depth = 0, instr = 0;
                            while (pp[plen] && plen < len) {
                                char ch = pp[plen];
                                if (instr) {
                                    if (ch == '\\' && pp[plen + 1]) plen++;
                                    else if (ch == '"') instr = 0;
                                } else if (ch == '"') instr = 1;
                                else if (ch == '{') depth++;
                                else if (ch == '}') { depth--; if (!depth) { plen++; break; } }
                                plen++;
                            }
                            if (depth == 0 && plen) {
                                rq.params = (char *)HeapAlloc(GetProcessHeap(), 0, plen + 1);
                                if (rq.params) { memcpy(rq.params, pp, plen); rq.params[plen] = 0; }
                            }
                        }
                        rq.receivedAt = GetTickCount64();
                        rq.deadlineMs = DEFAULT_DEADLINE_MS;
                        int bad = 0;
                        if (rq.params) {
                            long long dm = 0;
                            if (json_int(rq.params, "deadlineMs", &dm)) {
                                if (dm < 0) bad = 1;
                                else rq.deadlineMs = (u64)dm;      /* explicit 0 = no deadline */
                            }
                        }
                        rq.gen = gen;
                        if (bad) {
                            if (rq.params) HeapFree(GetProcessHeap(), 0, rq.params);
                            build_error(&b, 1, idbuf, -32602, "invalid params: deadlineMs < 0");
                            out_push(resp, gen);
                        } else if (!q_push(&rq)) {
                            if (rq.params) HeapFree(GetProcessHeap(), 0, rq.params);
                            build_error(&b, 1, idbuf, -32007, "Busy");
                            out_push(resp, gen);
                        } else {
                            logline("ipc: request queued for main thread");
                        }
                    }
                }
            }
        }

        /* M2.2 (d): expire queued requests while Hammer is stuck (modal) */
        q_sweep_expired();

        /* hello deadline */
        if (client != INVALID_SOCKET && !authed && now > helloDeadline) {
            logline("ipc: hello timeout -> close");
            closesocket(client);
            client = INVALID_SOCKET;
        }

        /* deferred close after a synchronous reply */
        if (client != INVALID_SOCKET && close_after_reply) {
            q_purge_gen(gen);
            if (authed) { g_authedClient = INVALID_SOCKET; g_clientGen = 0; }
            closesocket(client);
            client = INVALID_SOCKET;
            logline("ipc: client closed");
        }
    }
    if (client != INVALID_SOCKET) closesocket(client);
    HeapFree(GetProcessHeap(), 0, line);
    HeapFree(GetProcessHeap(), 0, resp);
    closesocket(ls);
    g_listener = INVALID_SOCKET;
    logline("ipc: thread exit");
    return 0;
}

/* ---------------- discovery + listener start ---------------- */
static char g_discoveryPath[512];
static char g_tmpPath[520];

static int write_discovery(void) {
    char buf[1024];
    SB b; sb_init(&b, buf, sizeof buf);
    sb_add(&b, "{\n  \"pid\": ");           sb_addnum(&b, GetCurrentProcessId());
    sb_add(&b, ",\n  \"protocol\": 1");
    sb_add(&b, ",\n  \"bridgeVersion\": \"" BRIDGE_BUILD "\"");
    sb_add(&b, ",\n  \"gameBuild\": \"" GAME_BUILD "\"");
    sb_add(&b, ",\n  \"transport\": \"tcp\"");
    sb_add(&b, ",\n  \"host\": \"127.0.0.1\"");
    sb_add(&b, ",\n  \"port\": ");           sb_addnum(&b, g_port);
    sb_add(&b, ",\n  \"pipeName\": null");
    sb_add(&b, ",\n  \"token\": \"");        sb_add(&b, g_token);
    sb_add(&b, "\",\n  \"startedAt\": ");    sb_addnum(&b, g_startTick);
    sb_add(&b, "\n}\n");
    HANDLE h = CreateFileA(g_tmpPath, GENERIC_WRITE, 0, 0, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, 0);
    if (h == INVALID_HANDLE_VALUE || !h) { logline("discovery: tmp open failed"); return 0; }
    u32 w = 0;
    WriteFile(h, buf, b.len, &w, 0);
    CloseHandle(h);
    if (!MoveFileExA(g_tmpPath, g_discoveryPath, MOVEFILE_REPLACE_EXISTING)) { logline("discovery: move failed"); return 0; }
    logline("discovery: written");
    return 1;
}

static void start_listener_once(void) {
    if (g_started || g_shuttingDown) return;
    g_started = 1;
    g_startTick = GetTickCount64();

    u8 raw[32];
    if (BCryptGenRandom(0, raw, sizeof raw, BCRYPT_USE_SYSTEM_PREFERRED_RNG) != 0) {
        logline("token: BCryptGenRandom failed; refusing to start (fail closed)");
        return;
    }
    const char *hex = "0123456789abcdef";
    for (u32 i = 0; i < 32; i++) { g_token[i * 2] = hex[raw[i] >> 4]; g_token[i * 2 + 1] = hex[raw[i] & 15]; }
    g_token[64] = 0;

    char base[260];
    u32 n = GetEnvironmentVariableA("LOCALAPPDATA", base, sizeof base);
    if (n == 0 || n >= sizeof base) {
        const char *fb = "C:\\users\\steamuser\\AppData\\Local";
        cpy(base, fb, sizeof base);
        logline("discovery: LOCALAPPDATA unset; using C:\\users\\steamuser\\AppData\\Local");
    }
    SB p; sb_init(&p, g_discoveryPath, sizeof g_discoveryPath);
    sb_add(&p, base); sb_add(&p, "\\cs2mcpbridge");
    CreateDirectoryA(g_discoveryPath, 0);
    sb_init(&p, g_discoveryPath, sizeof g_discoveryPath);
    sb_add(&p, base); sb_add(&p, "\\cs2mcpbridge\\session.json");
    sb_init(&p, g_tmpPath, sizeof g_tmpPath);
    sb_add(&p, base); sb_add(&p, "\\cs2mcpbridge\\session.json.tmp");

    logline("bridge: starting listener");
    g_writerThread = CreateThread(0, 0, writer_thread, 0, 0, 0);
    g_ipcThread = CreateThread(0, 0, ipc_thread, 0, 0, 0);
}

/* ---------------- IToolSystem2 object ---------------- */
typedef struct M0Obj { void **vt; void *factory; u32 tool_idx; u32 reserved; } M0Obj;
static M0Obj g_obj;
/* our object's vtable: IAppSystem (generated S2_SLOT_IAppSystem_*, 11 slots) + IToolSystem2's own
   slots 11..16 (b): the tool plugin shape upstream has no declaration for (see the comment inside
   build_vtable) */
static void *g_vt[S2_VTLEN_IAppSystem + 6];
/* F5: IToolSystem2's own slots are the IAppSystem vtable length + 0..5, and the
   highest one written must stay inside the array. */
_Static_assert(S2_VTLEN_IAppSystem + 5 < sizeof g_vt / sizeof g_vt[0],
               "IToolSystem2 slots must fit in g_vt");

static int  m0_Connect(M0Obj *self, void *factory) { self->factory = factory; g_connectFactory = factory; logline("Connect"); return 1; }
static void m0_Disconnect(M0Obj *self) { (void)self; logline("Disconnect"); }
static void *m0_QueryInterface(M0Obj *self, const char *name) { (void)self; (void)name; return 0; }
static int  m0_Init(M0Obj *self) { (void)self; logline("Init"); return 1; }
static void m0_PreShutdown(M0Obj *self) {
    (void)self;
    logline("PreShutdown");
    in_uninstall();
    snd_uninstall();
    snd_listener_uninstall();
    { OpErr e; sr_set(0, &e); }
    g_shuttingDown = 1;
    WakeAllConditionVariable(g_qcv);
    WakeAllConditionVariable(g_ocv);
    if (g_listener != INVALID_SOCKET) shutdown(g_listener, SD_BOTH);
    if (g_authedClient != INVALID_SOCKET) shutdown(g_authedClient, SD_BOTH);
}
static void m0_Shutdown(M0Obj *self) {
    (void)self;
    logline("Shutdown");
    m0_PreShutdown(self);
    if (g_ipcThread) { WaitForSingleObject(g_ipcThread, 2000); CloseHandle(g_ipcThread); g_ipcThread = 0; }
    if (g_writerThread) { WaitForSingleObject(g_writerThread, 2000); CloseHandle(g_writerThread); g_writerThread = 0; }
    if (g_discoveryPath[0]) DeleteFileA(g_discoveryPath);
    logline("Shutdown done");
}
static const void *m0_GetDependencies(M0Obj *self) { (void)self; return 0; }
static int  m0_GetTier(M0Obj *self) { (void)self; return (int)S2_CONST_APP_SYSTEM_TIER3; }
static void m0_Reconnect(M0Obj *self, void *factory, const char *name) { (void)self; (void)factory; (void)name; }
static int  m0_IsSingleton(M0Obj *self) { (void)self; return 0; }
static int  m0_GetBuildType(M0Obj *self) { (void)self; return (int)S2_CONST_kBuildTypeRelease; }
static int  m0_Slot11(M0Obj *self, void *passthrough, u32 idx) { (void)self; g_passthrough = passthrough; (void)idx; logline("slot11 -> 1"); return 1; }
static void m0_Slot12(M0Obj *self, void *request) { (void)self; (void)request; }
static void m0_Slot13(M0Obj *self) {
    (void)self;
    g_lastTick = GetTickCount64();
    /* pin THIS module once (verification threads run in it; never unload live code) */
    static int pinned = 0;
    if (!pinned) {
        void *mod = 0;
        if (GetModuleHandleExW(MODULE_PIN_FLAGS, (const void *)&m0_Slot13, &mod)) pinned = 1;
    }
    modules_poll();
    doc_fingerprint();
    start_listener_once();
    drain();
}
static int  m0_Slot14(M0Obj *self) { (void)self; return 0; }
static void m0_Slot15(M0Obj *self, u32 a, u32 b) { (void)self; (void)a; (void)b; }
static int  m0_Slot16(M0Obj *self, void *msg) { (void)self; (void)msg; logline("slot16"); return 0; }

static void build_vtable(void) {
    /* IAppSystem slots 0..10 come from the generated header (s2sdk_gen.h). Slots 11..16 (spelled
       S2_VTLEN_IAppSystem + 0..5) are IToolSystem2's own virtuals on the same vtable - a bridge-specific
       extension of IAppSystem with no upstream declaration and no pin; the indices are bridge-named:
         +0 (11): the tools framework hands us the passthrough object every frame;
         +1/+3/+4 (12/14/15): unimplemented stubs; +2 (13): the per-frame tools hook that drives the
         bridge (poll + main-thread drain); +5 (16): frame-end message callback (always 0 here). */
    g_vt[S2_SLOT_IAppSystem_Connect] = (void *)m0_Connect;      g_vt[S2_SLOT_IAppSystem_Disconnect] = (void *)m0_Disconnect;
    g_vt[S2_SLOT_IAppSystem_QueryInterface] = (void *)m0_QueryInterface; g_vt[S2_SLOT_IAppSystem_Init] = (void *)m0_Init;
    g_vt[S2_SLOT_IAppSystem_Shutdown] = (void *)m0_Shutdown;    g_vt[S2_SLOT_IAppSystem_PreShutdown] = (void *)m0_PreShutdown;
    g_vt[S2_SLOT_IAppSystem_GetDependencies] = (void *)m0_GetDependencies; g_vt[S2_SLOT_IAppSystem_GetTier] = (void *)m0_GetTier;
    g_vt[S2_SLOT_IAppSystem_Reconnect] = (void *)m0_Reconnect;  g_vt[S2_SLOT_IAppSystem_IsSingleton] = (void *)m0_IsSingleton;
    g_vt[S2_SLOT_IAppSystem_GetBuildType] = (void *)m0_GetBuildType;
    g_vt[S2_VTLEN_IAppSystem + 0] = (void *)m0_Slot11; g_vt[S2_VTLEN_IAppSystem + 1] = (void *)m0_Slot12;
    g_vt[S2_VTLEN_IAppSystem + 2] = (void *)m0_Slot13; g_vt[S2_VTLEN_IAppSystem + 3] = (void *)m0_Slot14;
    g_vt[S2_VTLEN_IAppSystem + 4] = (void *)m0_Slot15; g_vt[S2_VTLEN_IAppSystem + 5] = (void *)m0_Slot16;
    g_obj.vt = g_vt;
}

__declspec(dllexport) void *CreateInterface(const char *name, int *ret) {
    if (!g_obj.vt) { build_vtable(); q_init(); }
    if (seq(name, S2_TOOLSYSTEM2_VERSION)) { if (ret) *ret = 0; logline("CreateInterface -> object"); return &g_obj; }
    if (ret) *ret = 1;
    return 0;
}

/* LAST line of the TU (C1b/C1c): the ties fail closed here - a bridge-side value that
   never showed up (s2sdk.h's S2_UTLVEC_EXTERNAL alias, the S2HandleVec typedef) is a #error naming
   it, a drifted one fails the _Static_assert. After mcp_bridge_game.inc, which is included above. */
#include "s2sdk_gen_ties.h"
