//! Compatibility with the pre-conformance reader, commit 686975d.
//!
//! WP5 round 2: `cs2-kv3` is in production (hammer-mcp reads user- and tool-written files with
//! it), so the reader must accept the **union** of what commit 686975d accepted and what
//! ValveKeyValue (commit 28ca0e5) accepts — with 686975d's *value* where both platforms accept
//! an input but decode it differently. Round 1 had deliberately made the reader stricter, which
//! is what these tables pin against: every entry below is an input the OLD reader accepted, and
//! the expected trees are written out by hand here, not computed from the current parser.
//!
//! Upstream-only leniencies (single-quoted strings, `|` flags, real newlines inside quoted
//! strings, `+5`/`.5`/`5.` numbers, `inf`/`nan` literals, CI header keywords, commented blobs)
//! are the round-1 conformance file's job and stay; inputs **both** platforms accept but
//! *decode* differently are asserted to the 686975d value (the `\x` rows below and in
//! `vkv_conformance.rs`). Flag **chains** (`a:b:1`) nest, as 686975d read them: `a` flagging
//! `b:1`; `grammar.rs` and the depth budget in `errors.rs` pin that.

use cs2_kv3::{parse, Header, Number, Value};

const HDR: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
                   format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->";

/// The header, then `{`, the `body` lines and `}`, CRLF, as `errors.rs`/`grammar.rs` build theirs.
fn file(body: &[&str]) -> String {
    let mut text = String::from(HDR);
    text.push_str("\r\n{\r\n");
    for line in body {
        text.push_str(line);
        text.push_str("\r\n");
    }
    text.push('}');
    text
}

fn root(text: &str) -> cs2_kv3::Object {
    parse(text).expect("686975d accepted this input").root
}

fn get<'a>(object: &'a cs2_kv3::Object, key: &str) -> &'a Value {
    object.get(key).unwrap_or_else(|| panic!("no key {key}"))
}

fn number(text: &str) -> Value {
    Value::Number(Number::parse(text).expect("a valid lexeme"))
}

/// The number the old reader produced for a lexeme: the text verbatim.
fn as_lexeme(value: &Value) -> &str {
    match value {
        Value::Number(n) => &n.text,
        other => panic!("expected a number, got {other:?}"),
    }
}

// ----------------------------------------------------------------------------------------
// Round 2 item 1: commas between object entries
// ----------------------------------------------------------------------------------------

#[test]
fn commas_between_object_entries() {
    // The 686975d fixture `parse_only_loose.kv3`'s `c` key.
    let tree = root(&file(&["\tc = { d = 2, e = 3, }"]));
    let c = get(&tree, "c").as_object().expect("c is an object");
    assert_eq!(c.get("d"), Some(&number("2")), "first entry");
    assert_eq!(c.get("e"), Some(&number("3")), "second entry");
    assert_eq!(c.len(), 2);

    // And a trailing comma after the last entry.
    let tree = root(&file(&["\tf = null,", "\tg = \"x\""]));
    assert_eq!(get(&tree, "f"), &Value::Null);
    assert_eq!(get(&tree, "g").as_str(), Some("x"));

    // Even a leading comma (the old loop skipped commas wherever they sat).
    let c = get(&root(&file(&["\tc = { , a = 1, , b = 2 }"])), "c").clone();
    let c = c.as_object().expect("c");
    assert_eq!(c.get("a"), Some(&number("1")));
    assert_eq!(c.get("b"), Some(&number("2")));
}

#[test]
fn a_leading_comma_inside_the_value_is_686975d() {
    // The old value loop ran the same skip on a nested object's start.
    let tree = root(&file(&["\tc = { , d = 2 }"]));
    let c = get(&tree, "c").as_object().expect("c");
    assert_eq!(c.get("d"), Some(&number("2")));
    assert_eq!(c.len(), 1);
}

// ----------------------------------------------------------------------------------------
// Round 2 item 2: array elements without commas (and with a leading one)
// ----------------------------------------------------------------------------------------

#[test]
fn array_elements_without_commas() {
    // The crate docs at 686975d said "commas optional": `[ 1 2 ]` was one array of two.
    let tree = root(&file(&["\tb = [ 1 2 ]"]));
    let b = get(&tree, "b").as_array().expect("b");
    assert_eq!(b, &vec![number("1"), number("2")]);

    // The check the review asked for: `[ , 1 ]` — the old reader skipped leading commas
    // (`while comma bump` before the element check), so this parsed as one element.
    let tree = root(&file(&["\tb = [ , 1 ]"]));
    let b = get(&tree, "b").as_array().expect("b");
    assert_eq!(b, &vec![number("1")]);

    // Trailing commas, packed rows, LF spacing: the loose fixture's `b` again.
    let tree = root(&file(&["\tb = [ 1, 2, 3, ]"]));
    let b = get(&tree, "b").as_array().expect("b");
    assert_eq!(b, &vec![number("1"), number("2"), number("3")]);
}

// ----------------------------------------------------------------------------------------
// Round 2 item 3: `\r` and other escapes, the 686975d table
// ----------------------------------------------------------------------------------------

/// The set the 686975d `quoted_string` mapped: `\n`, `\t`, `\r`, `\"`, `\\`; every other `\X`
/// kept both characters. Upstream's table maps only `\n`/`\t` and drops the rest, so these are
/// the divergent-value rows (`\n` and `\t` agree and are not re-asserted here beyond one case).
#[test]
fn escape_table_is_the_686975d_one() {
    let cases = [
        // KV3 source inside the quotes -> expected string content.
        ("a\\rb", "a\rb"),            // `\r` -> carriage return (the writer emits `\r`)
        ("say \\\"hi\\\"", "say \"hi\""),
        ("C:\\\\Program Files", "C:\\Program Files"),
        ("a\\?b", "a\\?b"),           // unknown: backslash kept (upstream gives "ab")
        ("a\\vb", "a\\vb"),
        ("a\\0b", "a\\0b"),
    ];
    for (source, expected) in cases {
        let text = file(&[&format!("\tkey = \"{source}\"")]);
        let tree = root(&text);
        let got = get(&tree, "key").as_str().expect("a string");
        assert_eq!(got, expected, "source {source:?}");
    }
}

/// The whole restored `escapes.kv3` fixture (686975d bytes) against its tree, spelled out
/// rather than round-tripped, so an escape-table drift shows here and not only byte level.
#[test]
fn the_restored_escapes_fixture_decodes_the_old_way() {
    let text = include_str!("../fixtures/escapes.kv3");
    let root = root(text);
    assert_eq!(get(&root, "quote").as_str(), Some("say \"hi\""));
    assert_eq!(get(&root, "backslash").as_str(), Some("C:\\Program Files\\Game"));
    assert_eq!(get(&root, "backslash_at_end").as_str(), Some("trailing\\"));
    // `\t` tab, `\n` newline, `\r` carriage return: the old decode of this value.
    assert_eq!(get(&root, "tab_and_newline").as_str(), Some("col1\tcol2\nrow2\r\nrow3"));
    assert_eq!(get(&root, "mixed").as_str(), Some("\\\"x\\\" and \t\n"));
    assert_eq!(get(&root, "unicode").as_str(), Some("ünïcode ✓ 日本語"));
    // Keys too: an escaped quote in a quoted key is a plain quote in the key text.
    assert_eq!(root.keys().collect::<Vec<_>>(), vec![
        "_class", "empty", "quote", "backslash", "backslash_at_end", "tab_and_newline", "mixed",
        "unicode", "key with spaces", "key\"with\"quotes", "key\twith\ttab", "dotted.bare.key",
        "_leading_underscore", "trailing_dot.", "1_numeric_lead",
    ]);
}

// ----------------------------------------------------------------------------------------
// Round 2 item 4: any identifier is a flag name, preserved verbatim
// ----------------------------------------------------------------------------------------

#[test]
fn flag_names_are_any_identifier_kept_verbatim() {
    // `f:` in the old depth tests; `f` is deliberately NOT one of upstream's seven names.
    let cases = [
        ("\tkey = f:1", Value::Flagged {
            flag: "f".to_string(),
            value: Box::new(number("1")),
        }),
        ("\tkey = some_flag:\"x\"", Value::Flagged {
            flag: "some_flag".to_string(),
            value: Box::new(Value::String("x".to_string())),
        }),
        ("\tkey = Panorama:1.5", Value::Flagged {
            flag: "Panorama".to_string(),
            value: Box::new(number("1.5")),
        }),
    ];
    for (source, expected) in cases {
        assert_eq!(root(&file(&[source])).get("key"), Some(&expected), "source {source:?}");
    }
}

#[test]
fn flag_chains_nest_and_round_trip() {
    // 686975d nested a chain (`a:b:v` is `a` flagging `b:v`) and wrote every flag back; upstream
    // keeps only the last flag, which would drop `resource` from the file on a re-write.
    let source = "\tkey = resource:subclass:\"x\"";
    let expected = Value::Flagged {
        flag: "resource".to_string(),
        value: Box::new(Value::Flagged {
            flag: "subclass".to_string(),
            value: Box::new(Value::String("x".to_string())),
        }),
    };
    let text = file(&[source]);
    assert_eq!(root(&text).get("key"), Some(&expected));
    let written = cs2_kv3::write(&parse(&text).expect("chain parses"));
    assert!(written.contains("key = resource:subclass:\"x\""), "{written}");
}

// ----------------------------------------------------------------------------------------
// Round 2 item 5: header leniencies of the old reader
// ----------------------------------------------------------------------------------------

#[test]
fn version_braces_take_any_text() {
    // 686975d never looked inside `version{…}`: non-GUID spellings parse, `Header::raw` keeps
    // the line, and the body follows on the next line like it always did.
    let text = "<!-- kv3 encoding:text:version{x} format:generic:version{provisional} -->\r\n{ a = 1 }";
    let doc = parse(text).expect("686975d accepted version{{x}}");
    assert_eq!(doc.header.encoding.as_str(), "text");
    assert_eq!(doc.header.format.as_str(), "generic");
    assert_eq!(get(&doc.root, "a"), &number("1"));

    // The strict upstream reader still refuses it (kept available for the conformance tests).
    assert!(doc.header.validate_strict().is_err(), "upstream refuses version{{x}}");
}

#[test]
fn one_line_header_oddities_the_old_reader_accepted() {
    let cases = [
        // A `version` field without braces (`encoding:text:version`) — 686975d read the value
        // up to the next colon and stopped.
        "<!-- kv3 encoding:text:version:fmt format:generic:version -->\r\n{ }",
        // A yard of extra separators inside the encoding field.
        "<!-- kv3 encoding:text:123456:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\r\n{ }",
        // Fields swapped: the old reader found each field wherever it sat.
        "<!-- kv3 format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} -->\r\n{ }",
        // Extra tokens between the fields and the `-->`.
        "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} tool:hammer -->\r\n{ }",
        // The `<!--` glued to the first token (no space after the opener).
        "<!--kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} -->\r\n{ }",
    ];
    for text in cases {
        let doc = parse(text).unwrap_or_else(|error| panic!("{text:?}: {error}"));
        assert_eq!(doc.header.encoding, "text", "{text:?}");
        assert_eq!(doc.header.format, "generic", "{text:?}");
        assert!(doc.root.is_empty(), "{text:?}");
    }
}

// ----------------------------------------------------------------------------------------
// The number lexemes the old reader validated (`is_number_lexeme` of 686975d)
// ----------------------------------------------------------------------------------------

#[test]
fn the_old_number_lexemes_keep_their_spellings() {
    let body = file(&[
        "\tinteger = -1337",
        "\tzero_float = 0.0",
        "\tprecise = 74.56868",
        "\tnegative = -0.047012",
        "\texponent = 1e-05",
        "\tbignum = 2E+3",
        "\tleading_zeros = 007",
        "\tminus_zero = -0",
    ]);
    let root = root(&body);
    for (key, lexeme) in [
        ("integer", "-1337"),
        ("zero_float", "0.0"),
        ("precise", "74.56868"),
        ("negative", "-0.047012"),
        ("exponent", "1e-05"),
        ("bignum", "2E+3"),
        ("leading_zeros", "007"),
        ("minus_zero", "-0"),
    ] {
        assert_eq!(as_lexeme(get(&root, key)), lexeme, "{key}");
    }
}

// ----------------------------------------------------------------------------------------
// The restored parse_only_loose.kv3 fixture, tree written out
// ----------------------------------------------------------------------------------------

#[test]
fn the_restored_loose_fixture_tree() {
    let text = include_str!("../fixtures/parse_only_loose.kv3");
    let root = root(text);
    assert_eq!(get(&root, "a"), &number("1"));
    let b = get(&root, "b").as_array().expect("b is an array");
    assert_eq!(b, &vec![number("1"), number("2"), number("3")]);
    let c = get(&root, "c").as_object().expect("c is an object");
    assert_eq!(c.get("d"), Some(&number("2")));
    assert_eq!(c.get("e"), Some(&number("3")));
    assert_eq!(c.len(), 2);
    assert_eq!(get(&root, "f"), &Value::Null, "trailing comma after the entry");
    assert_eq!(get(&root, "g").as_str(), Some("x"));
    assert_eq!(root.len(), 5);
}

// ----------------------------------------------------------------------------------------
// Bugs of the old reader that are now documented acceptance, for the record
// ----------------------------------------------------------------------------------------

/// `#[ 0102 ]` (nibbles running together) and `#[ 4 1 ]` (nibbles split by trivia) read as the
/// bytes their nibbles make; upstream wants two-hex-digit tokens and refuses both.
#[test]
fn blob_nibbles_accumulate_across_trivia() {
    let cases: [(&str, &[u8]); 3] = [
        ("#[ 41 42 ]", &[0x41, 0x42]),
        ("#[ 0102 ]", &[0x01, 0x02]),
        ("#[ 4 1 ]", &[0x41]),
    ];
    for (source, expected) in cases {
        let text = file(&[&format!("\tbins = {source}")]);
        assert_eq!(get(&root(&text), "bins"), &Value::Blob(expected.to_vec()), "{source:?}");
    }
}

/// A single-flag `f:` value used through the model API survives a write→parse round trip with
/// the flag verbatim, exactly what hammer-mcp's `resource_name:` handling reads.
#[test]
fn one_flag_value_round_trips() {
    let text = file(&["\tname = resource_name:\"particles/a.vpcf\""]);
    let doc = parse(&text).expect("parses");
    assert_eq!(
        doc.root.get("name"),
        Some(&Value::Flagged {
            flag: "resource_name".to_string(),
            value: Box::new(Value::String("particles/a.vpcf".to_string())),
        }),
    );
    let again = parse(&cs2_kv3::write(&doc)).expect("re-parses");
    assert_eq!(again.header, doc.header);
    assert_eq!(again.root.get("name"), Some(&Value::Flagged {
        flag: "resource_name".to_string(),
        value: Box::new(Value::String("particles/a.vpcf".to_string())),
    }));
}

/// Hostile prefixes of these forgiving inputs still never panic (the fixtures cover the
/// strict-ish path; this covers the odd header and blob paths the fuzz-ish test in
/// `vkv_conformance.rs` reaches only through the canonical fixture).
#[test]
fn every_prefix_of_a_loose_file_parses_without_panicking() {
    let text = "<!-- kv3 format:generic:version{7412167c-06e9-4698-aff2-e63eb59037e7} encoding:text:version{x} -->\nbins = #[ 4 1 ]\n";
    for n in 0..=text.len() {
        if !text.is_char_boundary(n) {
            continue;
        }
        let Some(prefix) = text.get(..n) else { continue };
        let _ = parse(prefix);
    }
}

/// `Header::raw` of the fallback path stays the 686975d spelling: the whole first line minus
/// its CR/newline/space tail, with any leading whitespace kept.
#[test]
fn fallback_header_raw_is_the_686975d_line() {
    let text = "<!-- kv3 encoding:text:version{x} format:generic:version{y} -->\r\n{ }";
    let doc = parse(text).expect("parses");
    let expected_first_line = text.strip_suffix("\r\n{ }").expect("tail");
    assert_eq!(doc.header.raw, expected_first_line, "raw is the whole first line");
    assert_eq!(doc.header.encoding, "text");
    assert!(doc.header.validate_strict().is_err());

    assert_eq!(
        Header::parse(HDR).expect("canonical header").raw,
        HDR,
        "the canonical path is untouched (scan, not fallback)",
    );
}

// ----------------------------------------------------------------------------------------
// Round 3: the differential-fuzz reproducers (old reader vs this one)
// ----------------------------------------------------------------------------------------

fn flagged(flag: &str, value: Value) -> Value {
    Value::Flagged { flag: flag.to_string(), value: Box::new(value) }
}

fn string(text: &str) -> Value {
    Value::String(text.to_string())
}

/// Item 1: a `//` comment ending in `\` ends at the newline (upstream continues it).
#[test]
fn line_comment_ending_in_a_backslash_does_not_continue() {
    for newline in ["\n", "\r\n"] {
        let text = format!("{HDR}{newline}{{{newline}// see C:\\models\\{newline}x = 1{newline}}}");
        let tree = root(&text);
        assert_eq!(get(&tree, "x"), &number("1"), "{text:?}");
        assert_eq!(tree.len(), 1);
    }
    // After a value, inside an array, and as the very last thing before the `}`.
    let tree = root(&file(&["\tb = 5 //\\", "\ta = [ 1 //\\", "2 ]", "\tc = null // C:\\", ]));
    assert_eq!(get(&tree, "b"), &number("5"));
    assert_eq!(get(&tree, "a").as_array().expect("a"), &vec![number("1"), number("2")]);
    assert_eq!(get(&tree, "c"), &Value::Null);
    // A block comment is not a line comment: its backslash means nothing.
    let tree = root(&file(&["\t/* C:\\ */ x = 1"]));
    assert_eq!(get(&tree, "x"), &number("1"));
}

/// Item 2: an unquoted token ends where 686975d ended it, so comments, `#` and `-` after a
/// value are separate tokens.
#[test]
fn unquoted_tokens_end_at_comments_blobs_and_minus() {
    let tree = root(&file(&["\tx = 1// c"]));
    assert_eq!(get(&tree, "x"), &number("1"));
    let tree = root(&file(&["\tx/*c*/= 1"]));
    assert_eq!(get(&tree, "x"), &number("1"));
    let tree = root(&file(&["\tx = 1/*c*/", "\ty = true//c", "\tz = -2.5e3/**/"]));
    assert_eq!(get(&tree, "x"), &number("1"));
    assert_eq!(get(&tree, "y"), &Value::Bool(true));
    assert_eq!(get(&tree, "z"), &number("-2.5e3"));

    let tree = root(&file(&["\ta = [ true#[01] ]"]));
    assert_eq!(get(&tree, "a").as_array().expect("a"), &vec![Value::Bool(true), Value::Blob(vec![1])]);
    let tree = root(&file(&["\ta = [3/*;*/]"]));
    assert_eq!(get(&tree, "a").as_array().expect("a"), &vec![number("3")]);
    let tree = root(&file(&["\ta = [null-5]"]));
    assert_eq!(get(&tree, "a").as_array().expect("a"), &vec![Value::Null, number("-5")]);
}

/// Item 3: comments sit between a flag name and its `:`.
#[test]
fn comment_between_flag_and_colon() {
    let tree = root(&file(&["\tx = resource /*c*/ : \"m\""]));
    assert_eq!(get(&tree, "x"), &flagged("resource", string("m")));
    let tree = root(&file(&["\tx=e/**/:1"]));
    assert_eq!(get(&tree, "x"), &flagged("e", number("1")));
    let tree = root(&file(&["\tx = f // c", "\t: 2"]));
    assert_eq!(get(&tree, "x"), &flagged("f", number("2")));
}

/// Item 4: a `"""` string closes at the first `"""`; a lone CR is a newline; one newline is
/// dropped at each end. Every body here was accepted by 686975d.
#[test]
fn triple_quoted_strings_are_the_686975d_ones() {
    let cases: &[(&str, &str)] = &[
        ("{f=\"\"\"a\"\"\"}", "a"),
        ("{f=\"\"\"\nabc\"\"\"}", "abc"),
        ("{f=\"\"\"\n\tline\n\t\"\"\"}", "\tline\n\t"),
        ("{f=\"\"\"\na\\n\"\"\"}", "a\\n"),
        ("{f=\"\"\"\r\"\"\"}", ""),
        ("{f=\"\"\"\na\rb\n\"\"\"}", "a\nb"),
        ("{f=\"\"\"\r\nx\r\ny\r\n\"\"\"}", "x\ny"),
        ("{f=\"\"\"\n\n\n\"\"\"}", "\n"),
        ("{f=\"\"\"\"\"\"}", ""),
    ];
    for (body, expected) in cases {
        let tree = root(&format!("{HDR}\n{body}"));
        assert_eq!(get(&tree, "f"), &string(expected), "{body:?}");
    }
    // A raw string as a key (written back quoted).
    let tree = root(&format!("{HDR}\n{{\"\"=\"\"\"\"\"\"}}"));
    assert_eq!(get(&tree, ""), &string(""));
    // Both rules accept this and read it differently; the 686975d reading wins: `x` ends at the
    // first `"""`, and `b` is its own entry (upstream reads one string `x""" b = """\ny`).
    let tree = root(&format!("{HDR}\n{{a = \"\"\"\nx\"\"\" b = \"\"\"\ny\n\"\"\"}}"));
    assert_eq!(get(&tree, "a"), &string("x"));
    assert_eq!(get(&tree, "b"), &string("y"));
    // A string containing a mid-line `"""` that 686975d refuses (the file fails with its rule)
    // still reads with upstream's: the retry only ever turns a refusal into an acceptance.
    let tree = root(&format!("{HDR}\n{{a = \"\"\"\nu \\\"\"\" v\n\"\"\"}}"));
    assert_eq!(get(&tree, "a"), &string("u \"\"\" v"));
}

/// Items 5-7: the lenient header is the 686975d first-line reader whenever that accepts.
#[test]
fn lenient_header_keeps_the_686975d_answers() {
    let guid_t = "e21c7f3c-8a33-41c5-9977-a76d3a32aa0d";
    let guid_g = "7412167c-06e9-4698-aff2-e63eb59037e7";

    // A `-->` repeated on the line: the whole line is the header.
    let line = format!("<!-- kv3 encoding:text:version{{{guid_t}}} format:generic:version{{{guid_g}}} --> -->");
    let doc = parse(&format!("{line}\n{{}}")).expect("686975d accepted it");
    assert_eq!(doc.header.raw, line);
    assert_eq!(doc.header.format, "generic");

    // A stray BOM between two arrows.
    const BOM: char = '\u{feff}';
    let line = format!("<!-- kv3 encoding:text:version{{{guid_t}}} format:generic:version{{{guid_g}}} -->{BOM}-->");
    let doc = parse(&format!("{line}\n{{}}")).expect("686975d accepted it");
    assert_eq!(doc.header.raw, line);
    assert_eq!(doc.header.format, "generic");

    // `format: c:version{}`: the field is `format:` with an empty name.
    let line = "<!-- kv3 encoding:text:version{} format: c:version{}-->";
    let doc = parse(&format!("{line}\n{{}}")).expect("686975d accepted it");
    assert_eq!(doc.header.format, "");
    assert_eq!(doc.header.encoding, "text");
    assert_eq!(doc.header.raw, line);

    // Only the text encoding is readable, whichever reader looks (case included).
    for encoding in ["binary", "BINARY", "foo", "text2", ""] {
        let line = format!("<!-- kv3 encoding:{encoding}:version{{{guid_t}}} format:generic:version{{{guid_g}}} -->");
        assert!(parse(&format!("{line}\n{{}}")).is_err(), "{line}");
        assert!(Header::parse(&line).is_err(), "{line}");
    }
    // Upstream's case-insensitive spelling of the text encoding still reads (scan path).
    let line = format!("<!-- KV3 ENCODING:TEXT:VERSION{{{guid_t}}} FORMAT:generic:VERSION{{{guid_g}}} -->");
    let doc = parse(&format!("{line}\n{{}}")).expect("upstream accepts it");
    assert_eq!(doc.header.encoding, "TEXT");
}

/// Item 7: `Header::parse` is the lenient reader, trailing junk included.
#[test]
fn header_parse_is_lenient_like_686975d() {
    let loose = "<!-- kv3 encoding:text:version{x} format:modeldoc28:version{y} -->";
    let header = Header::parse(loose).expect("686975d accepted it");
    assert_eq!(header.raw, loose);
    assert_eq!(header.format, "modeldoc28");
    // The line ending and trailing blanks are tolerated.
    assert_eq!(Header::parse(&format!("{loose} \t\r\n")).expect("tail").raw, loose);
    // Junk after the arrow is refused (686975d: the line must END with `-->`).
    assert!(Header::parse(&format!("{loose} junk")).is_err());
    assert!(Header::parse(&format!("{loose} {{}}")).is_err());
    // A header followed by a body on later lines is fine (the parse() call shape).
    assert_eq!(Header::parse(&format!("{loose}\n{{ a = 1 }}")).expect("body follows").raw, loose);
    // The strict reader is the one that refuses `version{x}`.
    assert!(Header::parse_strict(loose).is_err());
    assert!(header.validate_strict().is_err());
}

/// Item 8: a key is written bare only when it matches `[A-Za-z_][A-Za-z0-9_.]*`.
#[test]
fn writer_quotes_a_leading_dot_key() {
    let mut doc = parse(&file(&["\ta = 1"])).expect("parses");
    doc.root.insert(".y".to_string(), number("2"));
    let written = cs2_kv3::write(&doc);
    assert!(written.contains("\t\".y\" = 2\r\n"), "{written}");
    // The reader takes a bare `.y` (upstream's identifier) and the writer still quotes it.
    let again = parse(&file(&["\t.y = 2"])).expect("a bare .y reads");
    assert_eq!(get(&again.root, ".y"), &number("2"));
    assert!(cs2_kv3::write(&again).contains("\t\".y\" = 2\r\n"));
}
