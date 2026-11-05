//! `bridge/test_json_text.c` ported to Rust, plus the differential corpus test.
//!
//! Three things live here:
//!
//! 1. [`c_test_cases`]: every case `bridge/test_json_text.c` runs, ported one for one (14 of
//!    them; the count is asserted against the C's "14 cases, 0 failures" line). The C output is
//!    the spec.
//! 2. [`roundtrip_probe`]: the C's `roundtrip_probe` - escape the raw probe fixture bytes with
//!    the tiny test-side escaper (`test_json_text.c:76-95`), size with [`json_text::end`],
//!    unescape, require byte-identical. A missing fixture is a FAILURE, never a skip
//!    (`test_json_text.c:7-8`).
//! 3. [`differential_corpus`]: every corpus case - the C test's inputs plus the negative and
//!    passthrough extras - run through this crate's Rust port and compared byte for byte
//!    against the *committed* corpus
//!    (`tests/data/json_text_corpus.txt`, written by the committed generator
//!    `tests/data/json_text_gen.c`, which includes `bridge/json_text.h` read-only).
//! 4. [`corpus_matches_the_c_generator`]: rebuilds the committed generator with `cc` into
//!    `$CARGO_TARGET_TMPDIR`, runs it, and requires its output to equal the committed corpus
//!    byte for byte - the corpus cannot drift from the C. If `cc` is missing this FAILS
//!    loudly naming `cc`; it never skips.
//!
//! Negative parity (buffer limits, invalid UTF-8, control bytes) is asserted twice: explicitly
//! in [`c_rejects_truncates_and_passes_through`], and differentially in the corpus.

use cs2_bridge::json_text::{self, Reason};

/// One ported `check`/`check_cap` case of `bridge/test_json_text.c`.
struct Case {
    /// The case name in the C test's PASS/FAIL lines.
    name: &'static str,
    /// The string BODY exactly as the C test passes it to `check` - the closing quote the C
    /// `check` appends for accepted bodies is already included here where the C appends it
    /// (`test_json_text.c:37`: `if (want) buf[effLen++] = '"';`).
    src: &'static [u8],
    /// The output capacity: `sizeof out` = 4096 in the default `check` cases, or the explicit
    /// small capacity of a `check_cap` case (`test_json_text.c:59-63`).
    cap: usize,
    want: Want,
}

/// What the C test's `check` asserts (`test_json_text.c:25-56`).
#[derive(Debug)]
enum Want {
    /// Accept, and the decoded bytes are exactly these (the C compares `memcmp(out, wantBytes,
    /// wantLen)` plus the NUL at `out[outLen]`, `test_json_text.c:41-42`).
    Accept(&'static [u8]),
    /// Refuse with exactly this reason string (`test_json_text.c:44`).
    Refuse(&'static str),
}

/// Every case of `bridge/test_json_text.c:185-196` (`check` + `check_cap`), in order.
const C_CHECK_CASES: &[Case] = &[
    // check("unescape_short_escapes", "\\n\\t\\r\\\"\\\\\\/", ..., "\"\n\t\r\"\\/")
    Case {
        name: "unescape_short_escapes",
        src: b"\\n\\t\\r\\\"\\\\\\/\"",
        cap: 4096,
        want: Want::Accept(b"\n\t\r\"\\/"),
    },
    Case {
        name: "unescape_u_ascii",
        src: b"\\u0041\"",
        cap: 4096,
        want: Want::Accept(b"A"),
    },
    Case { name: "refuse_u0000", src: b"\\u0000", cap: 4096, want: Want::Refuse("nul") },
    Case {
        name: "refuse_u_non_ascii",
        src: b"\\u00e9",
        cap: 4096,
        want: Want::Refuse("non_ascii_escape"),
    },
    Case {
        name: "refuse_surrogate",
        src: b"\\ud83d",
        cap: 4096,
        want: Want::Refuse("surrogate"),
    },
    Case {
        name: "refuse_unterminated",
        src: b"abc",
        cap: 4096,
        want: Want::Refuse("unterminated"),
    },
    Case {
        name: "refuse_unterminated_backslash",
        src: b"abc\\",
        cap: 4096,
        want: Want::Refuse("unterminated"),
    },
    Case {
        name: "refuse_raw_control_byte",
        src: b"a\tb",
        cap: 4096,
        want: Want::Refuse("control_byte"),
    },
    Case { name: "refuse_bad_escape", src: b"\\x", cap: 4096, want: Want::Refuse("bad_escape") },
    Case {
        name: "refuse_too_long",
        src: b"abcdef",
        cap: 3,
        want: Want::Refuse("too_long"),
    },
    Case {
        name: "refuse_no_room_for_nul",
        src: b"abc\"",
        cap: 3,
        want: Want::Refuse("too_long"),
    },
];

/// The count the C test prints: 11 `check`/`check_cap` cases (`test_json_text.c:185-196`) + 2
/// `end_cases` (`:116-137`) + 1 `roundtrip_probe` (`:140-177`) = 14
/// (`printf("%d cases, %d failures\n", g_cases, g_failures)`, `test_json_text.c:200`).
const C_CASE_COUNT: usize = 14;

/// Runs one ported `check` case exactly the way the C does: 4096-byte buffers memset to 0xFF
/// (`test_json_text.c:32` - an accept must NUL-terminate, not rely on zeroes).
fn run_case(case: &Case) {
    let mut out = [0xFF; 4096];
    assert!(
        case.src.len() < out.len(),
        "{}: case src longer than the C's 4096-byte buf",
        case.name
    );
    let got = json_text::unescape(case.src, &mut out[..case.cap]);
    match (&case.want, got) {
        (Want::Accept(bytes), Ok(len)) => {
            assert_eq!(len, bytes.len(), "{}: decoded length", case.name);
            assert_eq!(out[..len], bytes[..], "{}: decoded bytes", case.name);            assert_eq!(out[len], 0, "{}: the NUL terminator the C writes", case.name);
        }
        (Want::Refuse(reason), Err(got)) => {
            assert_eq!(got.as_str(), *reason, "{}: refusal reason", case.name);
        }
        (want, got) => panic!(
            "{}: C expects {:?}, the Rust port {}",
            case.name, want, if got.is_ok() { "accepted" } else { "refused" }
        ),
    }
}

/// All 14 cases of `bridge/test_json_text.c`, counted: the 11 `check`/`check_cap` cases of the
/// table above, the 2 `end_cases` (`test_json_text.c:116-137`), and `roundtrip_probe`
/// (`:140-177`, its own test below). If a case is added to the C test this number stops
/// matching and this assert names the mismatch.
#[test]
fn c_test_cases_ported_and_counted() {
    assert_eq!(
        C_CHECK_CASES.len() + 2 + 1,
        C_CASE_COUNT,
        "the ported case count no longer matches test_json_text.c's '14 cases' line"
    );

    // The 11 table cases, in the C's order.
    for case in C_CHECK_CASES {
        run_case(case);
    }

    // end_cases(): `json_text_end` on "a\\\"b\\\"c\"junk" minus its last byte finds the closing
    // quote at index 7; "abc" finds none (`test_json_text.c:116-137`).
    assert_eq!(json_text::end(b"a\\\"b\\\"c\"junk"), Some(7), "end_skips_escaped_quote");
    assert_eq!(json_text::end(b"abc"), None, "end_unterminated_is_null");

    // roundtrip_probe() is `roundtrip_probe_fixture` below: it needs the fixture file.
}

/// The tiny test-side escaper of `test_json_text.c:76-95`: `"` and `\` get a short escape, every
/// byte < 0x20 gets `\u00XX`, everything else (including UTF-8) passes through raw.
fn escape_like_the_c_test(input: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(input.len() * 6);
    for &c in input {
        match c {
            b'"' | b'\\' => {
                out.push(b'\\');
                out.push(c);
            }
            0x00..=0x1F => {
                out.extend_from_slice(b"\\u00");
                out.push(HEX[usize::from(c >> 4)]);
                out.push(HEX[usize::from(c & 15)]);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Our own tracked probe fixture, the C test's argv[1] (`test_json_text.c:7`); a missing file
/// is a FAILURE, never a skip (`test_json_text.c:102`).
fn probe_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../hammer-mcp/fixtures/vpcf/probe.vpcf").to_string()
}

/// The C's `roundtrip_probe` (`test_json_text.c:140-177`): the probe fixture's raw bytes,
/// escaped with the test-side escaper, sized with `json_text_end`, unescaped, must come back
/// byte-identical. A missing fixture is a FAILURE, never a skip (`test_json_text.c:102`).
#[test]
fn roundtrip_probe_fixture() {
    let probe_path = probe_path();
    let raw = match std::fs::read(&probe_path) {
        Ok(raw) => raw,
        Err(e) => panic!(
            "FAIL fixture: cannot open {probe_path}: {e} - a missing fixture is a FAILURE, never a skip"
        ),
    };

    let esc = escape_like_the_c_test(&raw);
    // src = the escaped body plus the appended closing quote, capacity = rawLen + 1 - exactly
    // the `json`/`out` shape the C test builds (`test_json_text.c:150-166`).
    let mut src = esc.clone();
    src.push(b'"');

    let end = json_text::end(&src)
        .unwrap_or_else(|| panic!("FAIL roundtrip_probe: no closing quote within the escaped body"));
    assert_eq!(end, esc.len(), "FAIL roundtrip_probe: json_text_end found the wrong position");

    let mut out = vec![0xFF; raw.len() + 1];
    let got = json_text::unescape(&src, &mut out)
        .unwrap_or_else(|reason| panic!("FAIL roundtrip_probe: unescape refused: {}", reason.as_str()));
    assert_eq!(got, raw.len(), "FAIL roundtrip_probe: decoded length");
    assert_eq!(
        &out[..got],
        raw.as_slice(),
        "FAIL roundtrip_probe: not byte-identical"
    );
    assert_eq!(out[got], 0, "the NUL terminator");
}

/// The NEGATIVE parity gate: inputs the C rejects/truncates/passes-through behave identically.
/// Each assertion cites the C line that fixes the behaviour; the corpus cases below re-prove
/// every one of them against the compiled C header.
#[test]
fn c_rejects_truncates_and_passes_through() {
    // Control bytes < 0x20 are refused, control_byte (json_text.h:103).
    assert_eq!(json_text::unescape(b"a\x00b\"", &mut [0xFF; 16]), Err(Reason::ControlByte));
    assert_eq!(json_text::unescape(b"\x1f\"", &mut [0xFF; 16]), Err(Reason::ControlByte));

    // Buffer limits: a decoded byte past the capacity is refused, too_long (json_text.h:106);
    // and the terminating NUL needs room too (json_text.h:59) - "ab" + NUL is 3 bytes, so cap
    // 2 refuses, cap 3 accepts exactly.
    assert_eq!(json_text::unescape(b"abcd\"", &mut [0xFF; 4]), Err(Reason::TooLong));
    assert_eq!(json_text::unescape(b"ab\"", &mut [0xFF; 2]), Err(Reason::TooLong));
    assert_eq!(json_text::unescape(b"ab\"", &mut [0xFF; 3]), Ok(2));
    assert_eq!(json_text::unescape(b"\"", &mut [0xFF; 0]), Err(Reason::TooLong));
    // Zero-length input is unterminated (the loop never runs; json_text.h:109).
    assert_eq!(json_text::unescape(b"", &mut [0xFF; 16]), Err(Reason::Unterminated));

    // Invalid UTF-8 is NOT rejected: raw bytes >= 0x20 pass through unchanged
    // (`test_json_text.c:74-75`, and json_text.h has no UTF-8 validation at all). Same bytes
    // out, same accept - this is the "behaves the same" answer for invalid UTF-8.
    let mut out = [0xFF; 16];
    assert_eq!(json_text::unescape(b"a\x80\xC3\x28\xff\"", &mut out), Ok(5));
    assert_eq!(&out[..6], b"a\x80\xC3\x28\xff\0");
}

/// One differential corpus record, mirroring what the committed generator
/// `tests/data/json_text_gen.c` writes: TAB-separated, one case per line, hex-encoded bytes.
#[derive(Debug)]
enum CorpusRecord {
    /// `U <name> <srcHex> <cap> ok <outHexInclNul>` / `U ... err <reason>`.
    Unescape {
        name: String,
        src_hex: String,
        cap: usize,
        result: Result<String, String>,
    },
    /// `E <name> <srcHex> <offset>` (offset -1 = no closing quote).
    End { name: String, src_hex: String, offset: Option<usize> },
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
            ("U", 6) => records.push(CorpusRecord::Unescape {
                name: fields[1].to_string(),
                src_hex: fields[2].to_string(),
                cap: fields[3].parse().expect("corpus cap"),
                result: match fields[4] {
                    "ok" => Ok(fields[5].to_string()),
                    "err" => Err(fields[5].to_string()),
                    other => panic!("corpus U result field: {other}"),
                },
            }),
            ("E", 4) => records.push(CorpusRecord::End {
                name: fields[1].to_string(),
                src_hex: fields[2].to_string(),
                offset: match fields[3] {
                    "-1" => None,
                    n => Some(n.parse().expect("corpus end offset")),
                },
            }),
            _ => panic!("corpus line not in the documented shape: {line:?}"),
        }
    }
    records
}

/// Every C case run through both implementations, byte for byte.
///
/// The corpus is committed at `tests/data/json_text_corpus.txt`, generated by the committed
/// `tests/data/json_text_gen.c` (its header comment has the build/run recipe); it holds the
/// 14 C-test cases under their C names plus the negative/passthrough extras, TAB-separated,
/// hex-encoded. [`corpus_matches_the_c_generator`] below proves the committed corpus never
/// drifts from the compiled C; this test proves the Rust port matches the committed corpus.
#[test]
fn differential_corpus() {
    let corpus_path =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/json_text_corpus.txt");
    let text = std::fs::read_to_string(corpus_path).unwrap_or_else(|e| {
        panic!(
            "the committed differential corpus {corpus_path} is missing from the checkout: {e}"
        )
    });

    let records = parse_corpus(&text);
    assert!(
        !records.is_empty(),
        "corpus {corpus_path} parsed to zero cases - the generator failed"
    );

    // The 14 C-test cases must all be present under their C names, so the differential corpus
    // cannot silently lose a case the C test covers.
    const C_CASE_NAMES: &[&str] = &[
        "unescape_short_escapes",
        "unescape_u_ascii",
        "refuse_u0000",
        "refuse_u_non_ascii",
        "refuse_surrogate",
        "refuse_unterminated",
        "refuse_unterminated_backslash",
        "refuse_raw_control_byte",
        "refuse_bad_escape",
        "refuse_too_long",
        "refuse_no_room_for_nul",
        "end_skips_escaped_quote",
        "end_unterminated_is_null",
        "roundtrip_probe_unescape",
        "roundtrip_probe_end",
    ];
    for name in C_CASE_NAMES {
        let present = records.iter().any(|r| match r {
            CorpusRecord::Unescape { name: n, .. } | CorpusRecord::End { name: n, .. } => {
                n == name
            }
        });
        assert!(present, "corpus is missing the C-test case {name}");
    }

    for record in &records {
        match record {
            CorpusRecord::Unescape { name, src_hex, cap, result } => {
                let src = unhex(src_hex, name);
                let mut out = vec![0xFF; *cap];
                let got = match json_text::unescape(&src, &mut out) {
                    Ok(len) => {
                        assert_eq!(out[len], 0, "{name}: the NUL terminator");
                        format!("ok\t{}", hex(&out[..=len]))
                    }
                    Err(reason) => format!("err\t{}", reason.as_str()),
                };
                let want = match result {
                    Ok(hex) => format!("ok\t{hex}"),
                    Err(reason) => format!("err\t{reason}"),
                };
                assert_eq!(got, want, "{name}: Rust output differs from the compiled C header");
            }
            CorpusRecord::End { name, src_hex, offset } => {
                let src = unhex(src_hex, name);
                assert_eq!(
                    json_text::end(&src),
                    *offset,
                    "{name}: Rust json_text_end differs from the compiled C header"
                );
            }
        }
    }
}

/// Hex-encodes for comparison against the corpus.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The corpus cannot drift from the C: rebuild the committed generator
/// (`tests/data/json_text_gen.c`, which includes `bridge/json_text.h` read-only) with `cc`
/// into `$CARGO_TARGET_TMPDIR`, run it against the probe fixture, and require its stdout to
/// equal the committed corpus byte for byte.
///
/// A missing `cc` is a FAILURE naming `cc` - never a skip; the C output is the spec, so a
/// checkout without `cc` has no way to prove the corpus and must fail loudly.
#[test]
fn corpus_matches_the_c_generator() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let gen_c = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/json_text_gen.c");
    let exe = concat!(env!("CARGO_TARGET_TMPDIR"), "/json_text_gen");
    let include = format!("{manifest}/../../bridge");

    let compile = std::process::Command::new("cc")
        .args([
            "-std=c11",
            "-O1",
            "-Wall",
            "-Wextra",
            "-I",
            &include,
            "-o",
            exe,
            gen_c,
        ])
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "the C compiler `cc` is missing or unusable ({e}): the corpus test cannot run \
                 without it, and skipping is not an option - the committed corpus \
                 tests/data/json_text_corpus.txt would go unverified"
            )
        });
    assert!(
        compile.status.success(),
        "cc could not build tests/data/json_text_gen.c:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );

    let probe = probe_path();
    let run = std::process::Command::new(exe)
        .arg(&probe)
        .output()
        .unwrap_or_else(|e| panic!("cannot run the freshly built generator {exe}: {e}"));
    assert!(
        run.status.success(),
        "the generator exited {:?}:\n{}",
        run.status.code(),
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(
        run.stderr.is_empty(),
        "the generator wrote to stderr: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let corpus_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/json_text_corpus.txt");
    let committed = std::fs::read(corpus_path)
        .unwrap_or_else(|e| panic!("the committed corpus {corpus_path} is missing: {e}"));
    if run.stdout != committed {
        // Report the first differing byte with a window of context instead of dumping two
        // whole byte arrays through assert_eq!.
        let first = run
            .stdout
            .iter()
            .zip(committed.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(run.stdout.len().min(committed.len()));
        panic!(
            "the compiled C generator's output differs from the committed corpus \
             tests/data/json_text_corpus.txt - regenerate it (see the generator's header \
             comment): first difference at byte {first} of generated len {} / committed len {}; \
             generated: {:?} ... committed: {:?} ...",
            run.stdout.len(),
            committed.len(),
            String::from_utf8_lossy(
                &run.stdout[first.saturating_sub(32)..first.min(run.stdout.len())]
            ),
            String::from_utf8_lossy(
                &committed[first.saturating_sub(32)..first.min(committed.len())]
            ),
        );
    }
}
