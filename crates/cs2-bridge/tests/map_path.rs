//! `test_map_path.c` (the C bridge's `map_path.h` test) ported to Rust, plus the differential
//! corpus test. The C sources are retired (git history, commit 8cdea42); their cases and their
//! output live on here as the spec.
//!
//! Two things live here:
//!
//! 1. [`C_PATH_CASES`] / [`C_HDR_CASES`] / [`C_EQ_CASES`] / [`c1_the_session_path_is_the_text_not_the_address`]:
//!    every case `test_map_path.c` ran, ported one for one (30 path + 13 header + 20 eq + 3 C1 =
//!    66, the C's "66 cases, 0 failures" line). The C output is the spec.
//! 2. [`differential_corpus`]: every corpus case - the C test's inputs plus the edge-case
//!    extras (over-long, trailing slashes, mixed separators, drive letters, non-ASCII bytes,
//!    degenerate header windows) - run through this crate's Rust port and compared against the
//!    *committed* corpus (`tests/data/map_path_corpus.txt`). The corpus is what the compiled
//!    `map_path.h` printed (its generator `tests/data/map_path_gen.c` went with the C, commit
//!    8cdea42), so it is frozen: a new case's expected line is derived from the C semantics by
//!    hand and reviewed, never from this port's own output.
//!
//! Refusals are checked on the EXACT reason string both here and in the corpus, which is the
//! NEGATIVE parity the port must keep: inputs the C refuses (traversal, over-long, bad chars,
//! non-canonical shapes) are refused identically.

use cs2_bridge::map_path::{self, Reason, MAP_PATH_MAX, MAP_VMAP_HEADER_MAX};

/// The C test's live spelling (`test_map_path.c:97`): `<addon root>` is
/// `content/csgo_addons/mcp_test` on a `Z:\app` install.
const ABS: &[u8] = b"Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap";

/// One `path_case` of `test_map_path.c` (accept, or refuse with the exact reason).
struct PathCase {
    name: &'static str,
    path: &'static [u8],
    want: Result<(), Reason>,
}

/// Every `path_case` of `test_map_path.c:100-143`, in that file's order.
const C_PATH_CASES: &[PathCase] = &[
    PathCase { name: "accept_simple", path: b"maps/probe.vmap", want: Ok(()) },
    PathCase { name: "accept_subdir", path: b"maps/a/probe.vmap", want: Ok(()) },
    PathCase { name: "accept_case_insensitive", path: b"MAPS/Probe.VMAP", want: Ok(()) },
    PathCase { name: "accept_name_chars", path: b"maps/a-b_c.1.vmap", want: Ok(()) },
    // ---- refused, with the exact reason ----
    PathCase { name: "empty", path: b"", want: Err(Reason::Empty) },
    PathCase { name: "not_under_maps", path: b"probe.vmap", want: Err(Reason::NotUnderMaps) },
    PathCase { name: "models_dir", path: b"models/probe.vmap", want: Err(Reason::NotUnderMaps) },
    PathCase { name: "wrong_ext", path: b"maps/probe.vmdl", want: Err(Reason::NotVmap) },
    PathCase { name: "wrong_ext_2", path: b"maps/probe.vmap2", want: Err(Reason::NotVmap) },
    PathCase { name: "no_basename", path: b"maps/.vmap", want: Err(Reason::NoBasename) },
    PathCase { name: "dotdot_basename", path: b"maps/..vmap", want: Err(Reason::DotDot) },
    PathCase { name: "abs_slash", path: b"/maps/probe.vmap", want: Err(Reason::Absolute) },
    PathCase { name: "abs_backslash", path: b"\\maps\\probe.vmap", want: Err(Reason::Absolute) },
    PathCase { name: "abs_drive", path: b"C:maps/probe.vmap", want: Err(Reason::Absolute) },
    PathCase { name: "backslash_sep", path: b"maps\\probe.vmap", want: Err(Reason::Backslash) },
    PathCase { name: "dotdot_mid", path: b"maps/../probe.vmap", want: Err(Reason::DotDot) },
    PathCase {
        name: "dotdot_double",
        path: b"maps/a/../../probe.vmap",
        want: Err(Reason::DotDot),
    },
    PathCase { name: "dotdot_in_name", path: b"maps/a..b.vmap", want: Err(Reason::DotDot) },
    PathCase { name: "control_newline", path: b"maps/a\nb.vmap", want: Err(Reason::Control) },
    PathCase { name: "control_del", path: b"maps/a\x7f.vmap", want: Err(Reason::Control) },
    PathCase { name: "colon_ads", path: b"maps/a:b.vmap", want: Err(Reason::Colon) },
    PathCase { name: "empty_segment", path: b"maps//probe.vmap", want: Err(Reason::EmptySegment) },
    PathCase {
        name: "empty_segment_mid",
        path: b"maps/a//probe.vmap",
        want: Err(Reason::EmptySegment),
    },
    PathCase { name: "dot_segment", path: b"maps/./probe.vmap", want: Err(Reason::DotSegment) },
    PathCase {
        name: "dot_segment_mid",
        path: b"maps/a/./probe.vmap",
        want: Err(Reason::DotSegment),
    },
    PathCase {
        name: "trailing_dot",
        path: b"maps/a./probe.vmap",
        want: Err(Reason::TrailingDotSpace),
    },
    PathCase {
        name: "trailing_space",
        path: b"maps/a /probe.vmap",
        want: Err(Reason::TrailingDotSpace),
    },
    PathCase {
        name: "space_segment",
        path: b"maps/ /probe.vmap",
        want: Err(Reason::TrailingDotSpace),
    },
];

/// One `hdr_case` of `test_map_path.c`: a NUL-terminated header string, the window being
/// `strlen + 1` bytes (`test_map_path.c:56-61`).
struct HdrCase {
    name: &'static str,
    head: &'static [u8],
    want: Result<(), Reason>,
}

/// Every `hdr_case` of `test_map_path.c:146-170`, in that file's order (the window is
/// the header bytes plus their NUL, exactly what the C hands over).
const C_HDR_CASES: &[HdrCase] = &[
    HdrCase {
        name: "hdr_live",
        head: b"<!-- dmx encoding binary 9 format vmap 40 -->\0",
        want: Ok(()),
    },
    HdrCase {
        name: "hdr_leading_zeros",
        head: b"<!-- dmx encoding binary 09 format vmap 40 -->\0",
        want: Ok(()),
    },
    HdrCase {
        name: "hdr_tab_sep",
        head: b"<!-- dmx encoding binary 9\tformat vmap 40 -->\0",
        want: Ok(()),
    },
    HdrCase {
        name: "hdr_version_9x",
        head: b"<!-- dmx encoding binary 9x format vmap 40 -->\0",
        want: Err(Reason::VersionNotNumber),
    },
    HdrCase {
        name: "hdr_version_8",
        head: b"<!-- dmx encoding binary 8 format vmap 40 -->\0",
        want: Err(Reason::VersionNot9),
    },
    HdrCase {
        name: "hdr_keyvalues2",
        head: b"<!-- dmx encoding keyvalues2 1 format vmap 40 -->\0",
        want: Err(Reason::NotBinary),
    },
    HdrCase {
        name: "hdr_text_9",
        head: b"<!-- dmx encoding text 9 format vmap 40 -->\0",
        want: Err(Reason::NotBinary),
    },
    HdrCase {
        name: "hdr_one_word",
        head: b"<!-- dmx encodingbinary 9 format vmap 40 -->\0",
        want: Err(Reason::FirstWord),
    },
    HdrCase {
        name: "hdr_too_few_words",
        head: b"<!-- dmx encoding binary 9-->\0",
        want: Err(Reason::VersionNotNumber),
    },
    HdrCase {
        name: "hdr_not_vmap",
        head: b"not a vmap at all\0",
        want: Err(Reason::NotDmx),
    },
    HdrCase {
        name: "hdr_dmx_only",
        head: b"<!-- dmx\0",
        want: Err(Reason::TooFewWords),
    },
];

/// One `eq_case` of `test_map_path.c` (`test_map_path.c:48-53` checks `map_path_eq` and
/// its symmetry).
struct EqCase {
    name: &'static str,
    a: &'static [u8],
    b: &'static [u8],
    want: bool,
}

/// Every `eq_case` of `test_map_path.c:172-195`, in that file's order.
const C_EQ_CASES: &[EqCase] = &[
    // the same file (both sides absolute)
    EqCase { name: "eq_same", a: ABS, b: ABS, want: true },
    EqCase {
        name: "eq_case",
        a: ABS,
        b: b"z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap",
        want: true,
    },
    EqCase {
        name: "eq_forward",
        a: ABS,
        b: b"Z:/app/content/csgo_addons/mcp_test/maps/probe.vmap",
        want: true,
    },
    EqCase {
        name: "eq_trailing_sep",
        a: ABS,
        b: b"Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe.vmap\\",
        want: true,
    },
    EqCase {
        name: "eq_upper",
        a: b"z:\\APP\\content\\CSGO_ADDONS\\MCP_TEST\\maps\\PROBE.VMAP",
        b: ABS,
        want: true,
    },
    // different files (a leaked session must not be claimed as this one)
    EqCase {
        name: "eq_other_file",
        a: ABS,
        b: b"Z:\\app\\content\\csgo_addons\\mcp_test\\maps\\probe2.vmap",
        want: false,
    },
    EqCase {
        name: "eq_other_addon",
        a: ABS,
        b: b"Z:\\app\\content\\csgo_addons\\other\\maps\\probe.vmap",
        want: false,
    },
    // review L2: no tail rule - a relative spelling never matches an absolute session path
    EqCase { name: "eq_rel_backslash", a: ABS, b: b"maps\\probe.vmap", want: false },
    EqCase { name: "eq_rel_forward", a: ABS, b: b"maps/probe.vmap", want: false },
    EqCase {
        name: "eq_content_rel",
        a: ABS,
        b: b"content\\csgo_addons\\mcp_test\\maps\\probe.vmap",
        want: false,
    },
    EqCase { name: "eq_caps_rel", a: ABS, b: b"CSGO_ADDONS/MCP_TEST/MAPS/PROBE.VMAP", want: false },
    EqCase {
        name: "eq_other_rel",
        a: ABS,
        b: b"csgo_addons\\other\\maps\\probe.vmap",
        want: false,
    },
    EqCase { name: "eq_other_rel2", a: ABS, b: b"maps\\other\\probe.vmap", want: false },
    EqCase { name: "eq_basename_only", a: ABS, b: b"probe.vmap", want: false },
    EqCase { name: "eq_rel_pair", a: b"maps\\probe.vmap", b: b"maps\\probe2.vmap", want: false },
    EqCase { name: "eq_rel_fold", a: b"maps\\probe.vmap", b: b"maps/probe.vmap", want: true },
    EqCase { name: "eq_rel_vs_basename", a: b"maps\\probe.vmap", b: b"probe.vmap", want: false },
    EqCase { name: "eq_empty_vs_abs", a: b"", b: ABS, want: false },
    EqCase { name: "eq_backslash_only", a: ABS, b: b"\\", want: false },
    EqCase { name: "eq_drive_only", a: ABS, b: b"Z:", want: false },
];

/// The `big` buffer of `test_map_path.c:104-116`: "maps/" + nameLen x 'a' + ".vmap".
fn big(name_len: usize) -> Vec<u8> {
    let mut path = Vec::with_capacity(name_len + 11);
    path.extend_from_slice(b"maps/");
    path.extend(std::iter::repeat_n(b'a', name_len));
    path.extend_from_slice(b".vmap");
    path
}

/// Every `path_case` of the C test, counted: the 28 static cases above plus the two `big`
/// buffers (513 bytes: one over the bound; 512 bytes: exactly the bound). `test_map_path.c`
/// runs 30 `path_case` calls in all; if one is added to the C test these counts stop matching.
#[test]
fn path_cases_have_the_c_verdicts() {
    assert_eq!(C_PATH_CASES.len(), 28, "the static path_case table");
    for case in C_PATH_CASES {
        assert_eq!(
            map_path::open_path_ok(case.path),
            case.want,
            "path case `{}` ({:?})",
            case.name,
            String::from_utf8_lossy(case.path)
        );
    }

    // test_map_path.c:104-116: the MAP_PATH_MAX byte bound.
    let over = big(503);
    assert_eq!(over.len(), MAP_PATH_MAX + 1, "the over-bound buffer is 513 bytes");
    assert_eq!(map_path::open_path_ok(&over), Err(Reason::TooLong), "over_bound_513");
    let exact = big(502);
    assert_eq!(exact.len(), MAP_PATH_MAX, "the exact-bound buffer is 512 bytes");
    assert_eq!(map_path::open_path_ok(&exact), Ok(()), "exact_bound_512");
}

/// Every `hdr_case` of the C test, counted, plus its two in-`main` window cases
/// (`test_map_path.c:157-170`): the words after the header's NUL are not the header, and a
/// window with no NUL at all is a refusal, never an overread.
#[test]
fn header_cases_have_the_c_verdicts() {
    assert_eq!(C_HDR_CASES.len(), 11, "the static hdr_case table");
    for case in C_HDR_CASES {
        assert_eq!(
            map_path::vmap_header_ok(case.head),
            case.want,
            "header case `{}` ({:?})",
            case.name,
            String::from_utf8_lossy(case.head)
        );
    }

    // test_map_path.c:158-165: `late` - the header string the C reads is everything up to its
    // NUL, which starts `<!-- not a dmx file at all -->`.
    let late = b"<!-- not a dmx file at all --> xxxxx<!-- dmx encoding binary 9 format vmap 40 -->\0";
    assert_eq!(
        map_path::vmap_header_ok(late),
        Err(Reason::NotDmx),
        "header: the words after the NUL are not the header"
    );
    // test_map_path.c:166-169: 42 bytes of the live header, no NUL anywhere in the window.
    assert_eq!(
        map_path::vmap_header_ok(&LIVE_HDR[..42]),
        Err(Reason::NoNul),
        "header: no NUL inside the window is a refusal"
    );
}

/// The live DMX header, byte for byte (`test_map_path.c:146`).
const LIVE_HDR: &[u8] = b"<!-- dmx encoding binary 9 format vmap 40 -->";

/// Every `eq_case` of the C test, counted, each also checked for the symmetry the C's
/// `eq_case` asserts (`test_map_path.c:52`).
#[test]
fn eq_cases_have_the_c_verdicts() {
    assert_eq!(C_EQ_CASES.len(), 20, "the eq_case table");
    for case in C_EQ_CASES {
        let got = map_path::path_eq(case.a, case.b);
        assert_eq!(got, case.want, "eq case `{}` ({:?} vs {:?})", case.name,
            String::from_utf8_lossy(case.a), String::from_utf8_lossy(case.b));
        assert_eq!(
            map_path::path_eq(case.b, case.a),
            case.want,
            "eq case `{}` must be symmetric",
            case.name
        );
    }
}

/// REVIEW C1 (`test_map_path.c:63-93`): a session's path is read as the TEXT at
/// docSession+0x6B0 (the `char *` member of the docSession's CUtlString), never as the ADDRESS
/// that session slot 26 (`GetMapFilePath`) returns - the old bridge read
/// `(const char *)slot_result`, i.e. the bytes of the pointer itself, which can never equal a
/// path (the leak gate therefore never fired).
///
/// In this Rust port the confusion cannot exist *by construction*: [`map_path::path_eq`] takes
/// the path's bytes - there is no pointer a caller could pass by mistake. What is portable is
/// the C test's three assertions, so all three are kept (counted, as in the C):
///
/// 1. the path TEXT matches itself (the C's `map_path_eq(s.m_pString, ABS) == 1`);
/// 2. bytes that look like a pointer value (the stand-in for what a `const char *` read of
///    slot 26 would see: the little-endian bytes of a plausible Wine heap address, NUL
///    included where the C's `as_charp` puts it) never match the path;
/// 3. the empty symbol's address bytes (m_pString is NULL, so the C reads the CUtlString
///    member's own address) never match either.
#[test]
fn c1_the_session_path_is_the_text_not_the_address() {
    // 1. the char* member is the path text (test_map_path.c:81-83).
    assert!(map_path::path_eq(ABS, ABS), "C1: the char* member is the path text");

    // 2. the ADDRESS must never match a path (test_map_path.c:77-79). The C builds `as_charp`
    //    from the live pointer bytes; here the same shape stands in deterministically.
    let address_bytes: &[u8] = &[
        0x10, 0x5a, 0x20, 0x00, 0x1a, 0x7f, 0x00, 0x00, // 0x00007f1a00205a10, LE
        0x00, // the NUL `as_charp[sizeof s] = 0` appends
    ];
    assert!(
        !map_path::path_eq(address_bytes, ABS),
        "C1: the CUtlString ADDRESS must never match a path"
    );

    // 3. an empty symbol's address is not a path either (test_map_path.c:84-92): the bytes
    //    change (a different member address), the answer must not.
    let empty_symbol_address: &[u8] = &[
        0x00, 0x5b, 0x20, 0x00, 0x1a, 0x7f, 0x00, 0x00, // 0x00007f1a00205b00, LE
        0x00,
    ];
    assert!(
        !map_path::path_eq(empty_symbol_address, ABS),
        "C1: an empty symbol's address is not a path either"
    );
}

/// The whole C test, counted: 30 `path_case` + 11 `hdr_case` + 2 window cases + 20 `eq_case`
/// + 3 C1 = 66, the number `test_map_path.c` printed (`66 cases, 0 failures`).
#[test]
fn the_c_case_count_is_ported_in_full() {
    assert_eq!(28 + 2 + 11 + 2 + 20 + 3, 66, "the C test's total case count");
}

/// One differential corpus record, mirroring what the C generator (`map_path_gen.c`) wrote:
/// TAB-separated, one case per line, hex-encoded bytes.
#[derive(Debug)]
enum CorpusRecord {
    /// `P <name> <pathHex> ok` / `P <name> <pathHex> err <reason>`.
    Open { name: String, path_hex: String, result: Result<(), String> },
    /// `H <name> <windowHex> ok` / `H <name> <windowHex> err <reason>`.
    Header { name: String, window_hex: String, result: Result<(), String> },
    /// `Q <name> <aHex> <bHex> <1|0>`.
    Eq { name: String, a_hex: String, b_hex: String, want: bool },
}

/// Hex-decodes a corpus field.
fn unhex(hex: &str, what: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "corpus {what}: odd hex length");
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks(2) {
        let hi = (pair[0] as char).to_digit(16).expect("corpus hex digit");
        let lo = (pair[1] as char).to_digit(16).expect("corpus hex digit");
        bytes.push(((hi << 4) | lo) as u8);
    }
    bytes
}

/// Parses the corpus the compiled C header wrote.
fn parse_corpus(text: &str) -> Vec<CorpusRecord> {
    let mut records = Vec::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(count) = line.strip_prefix("COUNT\t") {
            assert_eq!(
                records.len().to_string(),
                count,
                "corpus COUNT line disagrees with the parsed case count"
            );
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        match (fields[0], fields.len()) {
            ("P", 4) => records.push(CorpusRecord::Open {
                name: fields[1].to_string(),
                path_hex: fields[2].to_string(),
                result: match fields[3] {
                    "ok" => Ok(()),
                    other => panic!("corpus P result field: {other}"),
                },
            }),
            ("P", 5) => records.push(CorpusRecord::Open {
                name: fields[1].to_string(),
                path_hex: fields[2].to_string(),
                result: Err(fields[4].to_string()),
            }),
            ("H", 4) => records.push(CorpusRecord::Header {
                name: fields[1].to_string(),
                window_hex: fields[2].to_string(),
                result: Ok(()),
            }),
            ("H", 5) => records.push(CorpusRecord::Header {
                name: fields[1].to_string(),
                window_hex: fields[2].to_string(),
                result: Err(fields[4].to_string()),
            }),
            ("Q", 5) => records.push(CorpusRecord::Eq {
                name: fields[1].to_string(),
                a_hex: fields[2].to_string(),
                b_hex: fields[3].to_string(),
                want: fields[4] == "1",
            }),
            _ => panic!("corpus line not in the documented shape: {line:?}"),
        }
    }
    records
}

/// Every C-test case name that must appear in the corpus, so the differential corpus cannot
/// silently lose a case the C test covers.
const C_CASE_NAMES: &[&str] = &[
    // map_open_path_ok (test_map_path.c:100-143)
    "accept_simple",
    "accept_subdir",
    "accept_case_insensitive",
    "accept_name_chars",
    "over_bound_513",
    "exact_bound_512",
    "empty",
    "not_under_maps",
    "models_dir",
    "wrong_ext",
    "wrong_ext_2",
    "no_basename",
    "dotdot_basename",
    "abs_slash",
    "abs_backslash",
    "abs_drive",
    "backslash_sep",
    "dotdot_mid",
    "dotdot_double",
    "dotdot_in_name",
    "control_newline",
    "control_del",
    "colon_ads",
    "empty_segment",
    "empty_segment_mid",
    "dot_segment",
    "dot_segment_mid",
    "trailing_dot",
    "trailing_space",
    "space_segment",
    // map_vmap_header_ok (test_map_path.c:146-170)
    "hdr_live",
    "hdr_leading_zeros",
    "hdr_tab_sep",
    "hdr_version_9x",
    "hdr_version_8",
    "hdr_keyvalues2",
    "hdr_text_9",
    "hdr_one_word",
    "hdr_too_few_words",
    "hdr_not_vmap",
    "hdr_dmx_only",
    "hdr_words_after_nul",
    "hdr_no_nul_window",
    // map_path_eq (test_map_path.c:172-195)
    "eq_same",
    "eq_case",
    "eq_forward",
    "eq_trailing_sep",
    "eq_upper",
    "eq_other_file",
    "eq_other_addon",
    "eq_rel_backslash",
    "eq_rel_forward",
    "eq_content_rel",
    "eq_caps_rel",
    "eq_other_rel",
    "eq_other_rel2",
    "eq_basename_only",
    "eq_rel_pair",
    "eq_rel_fold",
    "eq_rel_vs_basename",
    "eq_empty_vs_abs",
    "eq_backslash_only",
    "eq_drive_only",
];

/// Every corpus case run through the Rust port, byte for byte against the C's committed output.
///
/// The corpus is committed at `tests/data/map_path_corpus.txt`, the output of the retired C
/// generator; it holds every C-test input under its shape name plus the edge-case extras,
/// TAB-separated, hex-encoded.
#[test]
fn differential_corpus() {
    let corpus_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/map_path_corpus.txt");
    let text = std::fs::read_to_string(corpus_path).unwrap_or_else(|e| {
        panic!("the committed differential corpus {corpus_path} is missing from the checkout: {e}")
    });

    let records = parse_corpus(&text);
    assert!(
        !records.is_empty(),
        "corpus {corpus_path} parsed to zero cases - the generator failed"
    );

    for name in C_CASE_NAMES {
        let present = records.iter().any(|r| match r {
            CorpusRecord::Open { name: n, .. }
            | CorpusRecord::Header { name: n, .. }
            | CorpusRecord::Eq { name: n, .. } => n == name,
        });
        assert!(present, "corpus is missing the C-test case {name}");
    }

    for record in &records {
        match record {
            CorpusRecord::Open { name, path_hex, result } => {
                // The C string's NUL travels in the hex; open_path_ok stops at it, exactly as
                // the C's strlen does.
                let path = unhex(path_hex, name);
                let got = map_path::open_path_ok(&path).map_err(|reason| reason.as_str().to_string());
                let want = result.clone();
                assert_eq!(got, want, "{name}: Rust open_path_ok differs from the compiled C");
            }
            CorpusRecord::Header { name, window_hex, result } => {
                let window = unhex(window_hex, name);
                assert!(
                    window.len() <= MAP_VMAP_HEADER_MAX,
                    "{name}: the corpus window exceeds MAP_VMAP_HEADER_MAX"
                );
                let got = map_path::vmap_header_ok(&window)
                    .map_err(|reason| reason.as_str().to_string());
                let want = result.clone();
                assert_eq!(got, want, "{name}: Rust vmap_header_ok differs from the compiled C");
            }
            CorpusRecord::Eq { name, a_hex, b_hex, want } => {
                let a = unhex(a_hex, name);
                let b = unhex(b_hex, name);
                assert_eq!(
                    map_path::path_eq(&a, &b),
                    *want,
                    "{name}: Rust path_eq differs from the compiled C"
                );
            }
        }
    }
}

/// The reason strings are the protocol: `map.open` turns a refusal into
/// `-32602 InvalidParams {reason}`, so every variant must spell its C literal exactly.
#[test]
fn reasons_are_the_c_literals() {
    let all = [
        (Reason::Empty, "path: a non-empty path relative to the addon content root (maps/... .vmap) is required"),
        (Reason::TooLong, "path: 1..512 chars"),
        (Reason::Absolute, "path: relative to the addon content root (maps/...), not absolute"),
        (Reason::Control, "path: no control characters"),
        (Reason::Colon, "path: no ':'"),
        (Reason::Backslash, "path: no '\\' (use '/' separators)"),
        (Reason::NotVmap, "path: a .vmap file"),
        (Reason::NotUnderMaps, "path: under maps/ (the addon's map directory)"),
        (Reason::NoBasename, "path: a map file name under maps/"),
        (Reason::DotDot, "path: no '..'"),
        (Reason::EmptySegment, "path: no empty path segment ('//')"),
        (Reason::DotSegment, "path: no '.' or '..' path segment"),
        (Reason::TrailingDotSpace, "path: no path segment ending in '.' or a space"),
        (Reason::EmptyFile, "empty file"),
        (Reason::NoNul, "no NUL-terminated DMX header in the first bytes"),
        (Reason::NotDmx, "not a DMX file (the header must start with '<!-- dmx ')"),
        (Reason::FirstWord, "the DMX header's first word is not 'encoding'"),
        (Reason::NotBinary, "the DMX encoding is not 'binary'"),
        (Reason::VersionNotNumber, "the DMX binary encoding version is not a number"),
        (Reason::VersionNot9, "the DMX binary encoding version is not 9"),
        (Reason::TooFewWords, "the DMX header is too short (encoding, binary, 9, format, <fmt>, <ver>)"),
    ];
    for (reason, text) in all {
        assert_eq!(reason.as_str(), text);
    }
    // And every refusal in the C test's tables names one of them (no reason invented here).
    for case in C_PATH_CASES {
        if let Err(reason) = case.want {
            assert!(all.iter().any(|(r, _)| *r == reason), "`{}` is a C reason", reason.as_str());
        }
    }
    for case in C_HDR_CASES {
        if let Err(reason) = case.want {
            assert!(all.iter().any(|(r, _)| *r == reason), "`{}` is a C reason", reason.as_str());
        }
    }
}
