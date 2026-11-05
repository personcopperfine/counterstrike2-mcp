/* map_path_gen.c - the committed generator of the committed differential corpus for the Rust
 * port of bridge/map_path.h (WP4.H3).
 *
 * Includes the bridge header read-only and records, for every case, what the COMPILED C
 * produces - one TAB-separated line per case, hex-encoded bytes. Its stdout is the committed
 * corpus crates/cs2-bridge/tests/data/map_path_corpus.txt: regenerate with the commands below
 * and the Rust test map_path.rs::corpus_matches_the_c_generator fails if the two drift apart.
 *
 * Record shapes:
 *   P  <name> <pathHex>   ok                - map_open_path_ok accepted
 *   P  <name> <pathHex>   err <reason>      - map_open_path_ok refused, C reason verbatim
 *   H  <name> <windowHex> ok                - map_vmap_header_ok accepted
 *   H  <name> <windowHex> err <reason>      - map_vmap_header_ok refused, C reason verbatim
 *   Q  <name> <aHex> <bHex> <1|0>           - map_path_eq's answer
 *   COUNT <n>                               - the number of records
 *
 * pathHex for P/Q carries the whole C string INCLUDING its NUL (the C reads strlen+1 bytes and
 * stops at the NUL); windowHex for H is exactly the `len` bytes the helper is handed - the
 * header's NUL is part of the window, and one case has no NUL at all.
 *
 * Case list: every input of bridge/test_map_path.c under that test's shape names (its
 * 513/512-byte `big` buffers are built here the same way), plus the edge-case extras the Rust
 * test gates on: a 1027-byte over-long path, trailing slashes, mixed separators, a drive
 * letter in absolute form, raw non-ASCII (valid and invalid UTF-8) in a path and in path_eq,
 * an empty header window, a NUL-only window, a sign-prefixed version word, and 8.3 short
 * names / a trailing-dot component in path_eq.
 *
 * Build & run (from the repo root):
 *   cc -std=c11 -O1 -Wall -Wextra -I bridge \
 *       -o target/scratch/map_path_gen crates/cs2-bridge/tests/data/map_path_gen.c
 *   target/scratch/map_path_gen > crates/cs2-bridge/tests/data/map_path_corpus.txt
 */
#include <stdio.h>
#include <string.h>

typedef unsigned char u8;            /* the bridge's own typedefs; map_path.h needs only these */
typedef unsigned int u32;

#include "map_path.h"

static int g_count;

static void hex_write(const char *p, u32 n) {
    static const char *hex = "0123456789abcdef";
    for (u32 i = 0; i < n; i++) {
        putchar(hex[(u8)p[i] >> 4]);
        putchar(hex[(u8)p[i] & 15]);
    }
}

/* one map_open_path_ok case: the C string (NUL included) is the input, the C verdict the line */
static void p_case(const char *name, const char *path) {
    const char *reason = "?";
    int got = map_open_path_ok(path, &reason);
    printf("P\t%s\t", name);
    hex_write(path, (u32)strlen(path) + 1);
    if (got) {
        printf("\tok\n");
    } else {
        printf("\terr\t%s\n", reason);
    }
    g_count++;
}

/* one map_vmap_header_ok case over a whole window (bytes handed to the helper verbatim) */
static void h_case(const char *name, const char *window, u32 len) {
    const char *reason = "?";
    int got = map_vmap_header_ok(window, len, &reason);
    printf("H\t%s\t", name);
    hex_write(window, len);
    if (got) {
        printf("\tok\n");
    } else {
        printf("\terr\t%s\n", reason);
    }
    g_count++;
}

/* the hdr_case shape: a NUL-terminated header string, window = strlen + 1 */
static void h_str(const char *name, const char *head) {
    h_case(name, head, (u32)strlen(head) + 1);
}

/* one map_path_eq case; both sides are C strings (NULs included), the verdict recorded */
static void q_case(const char *name, const char *a, const char *b) {
    int got = map_path_eq(a, b);
    printf("Q\t%s\t", name);
    hex_write(a, (u32)strlen(a) + 1);
    printf("\t");
    hex_write(b, (u32)strlen(b) + 1);
    printf("\t%d\n", got);
    g_count++;
}

/* the `big` buffer of test_map_path.c:104-116: "maps/" + nameLen x 'a' + ".vmap" */
static char big[1200];
static void build_big(int nameLen) {
    memcpy(big, "maps/", 5);
    for (int i = 0; i < nameLen; i++) big[5 + i] = 'a';
    memcpy(big + 5 + nameLen, ".vmap", 5);
    big[5 + nameLen + 5] = 0;
}

/* test_map_path.c's live spelling: <addon root> is content/csgo_addons/mcp_test on Z:\app */
static const char *const ABS = "Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";
static const char *const LIVE_HDR = "<!-- dmx encoding binary 9 format vmap 40 -->";

int main(void) {
    /* ---- map_open_path_ok: the C test's cases (test_map_path.c:100-143) ---- */
    p_case("accept_simple", "maps/probe.vmap");
    p_case("accept_subdir", "maps/a/probe.vmap");                 /* a map in a subdirectory */
    p_case("accept_case_insensitive", "MAPS/Probe.VMAP");         /* case-insensitive */
    p_case("accept_name_chars", "maps/a-b_c.1.vmap");
    build_big(503);
    p_case("over_bound_513", big);                                /* 513 bytes: one over */
    build_big(502);
    p_case("exact_bound_512", big);                               /* 512 bytes: the bound */
    p_case("empty", "");
    p_case("not_under_maps", "probe.vmap");
    p_case("models_dir", "models/probe.vmap");
    p_case("wrong_ext", "maps/probe.vmdl");
    p_case("wrong_ext_2", "maps/probe.vmap2");
    p_case("no_basename", "maps/.vmap");
    p_case("dotdot_basename", "maps/..vmap");
    p_case("abs_slash", "/maps/probe.vmap");
    p_case("abs_backslash", "\\maps\\probe.vmap");
    p_case("abs_drive", "C:maps/probe.vmap");
    p_case("backslash_sep", "maps\\probe.vmap");                  /* review M1 */
    p_case("dotdot_mid", "maps/../probe.vmap");
    p_case("dotdot_double", "maps/a/../../probe.vmap");
    p_case("dotdot_in_name", "maps/a..b.vmap");  /* the modeldoc.open/pet.open rule */
    p_case("control_newline", "maps/a\nb.vmap");
    p_case("control_del", "maps/a\x7f.vmap");
    p_case("colon_ads", "maps/a:b.vmap");                         /* an ADS, never a map */
    /* review M1: canonical form only - Win32 opens every one for `maps/probe.vmap` */
    p_case("empty_segment", "maps//probe.vmap");
    p_case("empty_segment_mid", "maps/a//probe.vmap");
    p_case("dot_segment", "maps/./probe.vmap");
    p_case("dot_segment_mid", "maps/a/./probe.vmap");
    p_case("trailing_dot", "maps/a./probe.vmap");
    p_case("trailing_space", "maps/a /probe.vmap");
    p_case("space_segment", "maps/ /probe.vmap");

    /* ---- map_open_path_ok: the corpus extras ---- */
    build_big(1012);
    p_case("x_over_long_1024", big);                              /* 1027 bytes */
    p_case("x_trailing_slash", "maps/probe.vmap/");               /* an empty last segment */
    p_case("x_trailing_slash_dir", "maps/a/");
    p_case("x_drive_letter_abs", "Z:\\maps\\probe.vmap");
    p_case("x_mixed_separators", "maps/a\\b.vmap");
    p_case("x_non_ascii", "maps/pr\xc3" "\xa9" "be.vmap");        /* valid UTF-8 */
    p_case("x_non_ascii_raw", "maps/\x80\x8a.vmap");              /* not UTF-8, still bytes */
    p_case("x_dot_end_basename", "maps/probe.vmap.");             /* the suffix gate gets it */
    p_case("x_space_end_basename", "maps/probe.vmap ");

    /* ---- map_vmap_header_ok: the C test's cases (test_map_path.c:146-170) ---- */
    h_str("hdr_live", "<!-- dmx encoding binary 9 format vmap 40 -->");      /* the live header */
    h_str("hdr_leading_zeros", "<!-- dmx encoding binary 09 format vmap 40 -->"); /* parse = 9 */
    h_str("hdr_tab_sep", "<!-- dmx encoding binary 9\tformat vmap 40 -->");  /* split_whitespace */
    h_str("hdr_version_9x", "<!-- dmx encoding binary 9x format vmap 40 -->"); /* not the # 9 */
    h_str("hdr_version_8", "<!-- dmx encoding binary 8 format vmap 40 -->"); /* H1: 16-byte trap */
    h_str("hdr_keyvalues2", "<!-- dmx encoding keyvalues2 1 format vmap 40 -->"); /* ditto */
    h_str("hdr_text_9", "<!-- dmx encoding text 9 format vmap 40 -->");
    h_str("hdr_one_word", "<!-- dmx encodingbinary 9 format vmap 40 -->");   /* one word, not 3 */
    h_str("hdr_too_few_words", "<!-- dmx encoding binary 9-->");             /* fewer than six */
    h_str("hdr_not_vmap", "not a vmap at all");
    h_str("hdr_dmx_only", "<!-- dmx");
    {
        /* the header NUL is what ends it: the words after the NUL are not a header */
        static const char late[] =
            "<!-- not a dmx file at all --> xxxxx<!-- dmx encoding binary 9 format vmap 40 -->";
        h_case("hdr_words_after_nul", late, (u32)sizeof late);
    }
    /* a window with no NUL at all is a refusal, never an overread (42 bytes of the live hdr) */
    h_case("hdr_no_nul_window", LIVE_HDR, 42);

    /* ---- map_vmap_header_ok: the corpus extras ---- */
    h_case("x_empty_window", "", 0);                              /* the C's !len arm */
    h_str("x_nul_only_window", "");                               /* strlen+1 = 1: just the NUL */
    h_str("x_plus_version", "<!-- dmx encoding binary +9 format vmap 40 -->"); /* reader parses */
    h_str("x_minus_version", "<!-- dmx encoding binary -9 format vmap 40 -->");
    h_str("x_double_space", "<!--  dmx encoding binary 9 format vmap 40 -->"); /* one space only */
    {
        /* the live header inside a full-size window: the NUL ends it, the tail is junk */
        static char window[MAP_VMAP_HEADER_MAX];
        memset(window, 'x', sizeof window);
        memcpy(window, LIVE_HDR, strlen(LIVE_HDR) + 1);           /* the NUL travels with it */
        h_case("x_full_window", window, (u32)sizeof window);
    }

    /* ---- map_path_eq: the C test's cases (test_map_path.c:172-195) ---- */
    q_case("eq_same", ABS, ABS);
    q_case("eq_case", ABS, "z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap");
    q_case("eq_forward", ABS, "Z:/app/content/csgo_addons/mcp_test/maps/probe.vmap");
    q_case("eq_trailing_sep", ABS, "Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap\\");
    q_case("eq_upper", "z:\\APP\\content\\CSGO_ADDONS\\MCP_TEST\\maps\\PROBE.VMAP", ABS);
    q_case("eq_other_file", ABS, "Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe2.vmap");
    q_case("eq_other_addon", ABS, "Z:\\app\\content\\csgo_addons\\other\\maps\\probe.vmap");
    q_case("eq_rel_backslash", ABS, "maps\\probe.vmap");   /* review L2: no tail rule */
    q_case("eq_rel_forward", ABS, "maps/probe.vmap");      /* ... the tool sends */
    q_case("eq_content_rel", ABS, "content\\csgo_addons\\mcp_test\\maps\\probe.vmap");
    q_case("eq_caps_rel", ABS, "CSGO_ADDONS/MCP_TEST/MAPS/PROBE.VMAP");
    q_case("eq_other_rel", ABS, "csgo_addons\\other\\maps\\probe.vmap");
    q_case("eq_other_rel2", ABS, "maps\\other\\probe.vmap");
    q_case("eq_basename_only", ABS, "probe.vmap");
    q_case("eq_rel_pair", "maps\\probe.vmap", "maps\\probe2.vmap");
    q_case("eq_rel_fold", "maps\\probe.vmap", "maps/probe.vmap"); /* both relative: folding */
    q_case("eq_rel_vs_basename", "maps\\probe.vmap", "probe.vmap");
    q_case("eq_empty_vs_abs", "", ABS);
    q_case("eq_backslash_only", ABS, "\\");
    q_case("eq_drive_only", ABS, "Z:");

    /* ---- map_path_eq: the corpus extras ---- */
    q_case("x_mixed_fold", "maps\\a\\b.vmap", "maps/a/b.vmap");
    q_case("x_multi_trailing_sep", "maps/p.vmap\\\\\\", "maps/p.vmap/");
    q_case("x_drive_case", "c:\\x\\maps\\p.vmap", "C:\\x\\maps\\p.vmap");
    q_case("x_non_ascii_differs", "maps/p\xc3\xa9.vmap", "MAPS/P\xc3\x89.VMAP"); /* not folded */
    q_case("x_non_ascii_same", "maps/p\xc3\xa9.vmap", "maps/p\xc3\xa9.vmap");
    q_case("x_both_seps_only", "\\\\", "////");                   /* both strip to empty */
    q_case("x_83_short_name", "maps/probe~1.vmap", "maps/probe.vmap"); /* not folded (review) */
    q_case("x_trailing_dot_component", "maps/probe.vmap", "maps/probe.vmap.");

    printf("COUNT\t%d\n", g_count);
    return 0;
}
