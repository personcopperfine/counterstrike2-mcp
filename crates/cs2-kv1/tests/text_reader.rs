//! KV1 text reading: the ported upstream test cases plus this port's lossesless/condition
//! and hostile-input cases.
//!
//! Upstream test files ported here (fixture names kept): `Text/AssignmentTestCase.cs`,
//! `Text/ConditionalTestCase.cs`, `Text/ConditionalsInMultipleObjects.cs`,
//! `Text/InvalidConditionalTestCase.cs`, `Text/Escaped*TestCase.cs`,
//! `Text/SingleLineCommentTestCase.cs`, `Text/CommentOnEndOfTheLine.cs`,
//! `Text/CommentWithCrOnlyTestCase.cs`, `Text/TrailingCommentTestCase.cs`,
//! `Text/UnquotedTextTestCase.cs`, `Text/Include*TestCase.cs`, `Text/BaseInclude*TestCase.cs`,
//! `Text/InvalidSyntaxTestCase.cs`, `MultipleRootObjectsTestCase.cs` (the KV1 text cases),
//! `Text/NumbersTestCase.cs` and `Text/Uint64HexTestCase.cs` (as raw-text assertions, see
//! `values_stay_text`), `Text/SteamDataTestCase.cs`, `Text/LegacyDepotDataSubsetTestCase.cs`,
//! `Text/DictionaryTestCase.cs`, `Text/List*TestCase.cs`, `RoundTripTestCase.cs` (the KV1
//! parts), `KVObjectDeepNestingTestCase.cs` (the KV1 text parts).

use cs2_kv1::text::{self, ReadOptions};
use cs2_kv1::{Document, Error, KeyValue, Object, Value};

/// A fixture, exactly as it is on disk (BOM included, like upstream's embedded resource).
fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/text").join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("cannot read fixture {}: {err}", path.display()))
}

fn parse(name: &str) -> Document {
    let text = fixture(name);
    text::parse(&text).unwrap_or_else(|err| panic!("{name} should parse: {err:?}"))
}

/// Upstream's `ParseResource(name, conditions)`: evaluate `[$COND]` against these symbols.
fn parse_with_conditions(name: &str, conditions: &[&str]) -> Document {
    let options = ReadOptions {
        conditions: Some(conditions.iter().map(|c| c.to_string()).collect()),
        ..Default::default()
    };
    let text = fixture(name);
    text::parse_with(&text, &options)
        .unwrap_or_else(|err| panic!("{name} should parse: {err:?}"))
}

fn body(doc: &Document) -> &Object {
    doc.value.as_object().expect("the root pair's value is an object")
}

fn value<'a>(doc: &'a Document, key: &str) -> &'a Value {
    body(doc).get(key).unwrap_or_else(|| panic!("{key} should be present"))
}

fn string(doc: &Document, key: &str) -> String {
    value(doc, key).as_str().unwrap_or_else(|| panic!("{key} should be a string")).to_string()
}

/// Every pair under the root, as `key=value` (upstream's
/// `data.Children.Select(c => $"{c.Key}={c.Value}")`).
fn pairs(doc: &Document) -> Vec<String> {
    body(doc)
        .entries
        .iter()
        .map(|kv| format!("{}={}", kv.key, kv.value.as_str().unwrap_or("<object>")))
        .collect()
}

fn syntax_error(err: Error) -> (usize, usize, String) {
    match err {
        Error::Syntax { line, column, message } => (line, column, message),
        other => panic!("expected a syntax error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// AssignmentTestCase
// ---------------------------------------------------------------------------------------

#[test]
fn reads_key_value_pair() {
    for input in [
        "\"root\" { key=value }",
        "\"root\" { key = value }",
        "\"root\" { \"key\" = \"value\" }",
        "\"root\" { \"key\"=\"value\" }",
        "\"root\" { key\t=\nvalue }",
        "\"root\" { key = // comment\nvalue }",
        "\"root\" { key value}",
        "\"root\"{key\"value\"}",
    ] {
        let doc = text::parse(input).unwrap_or_else(|err| panic!("{input:?}: {err:?}"));
        assert_eq!(
            body(&doc).entries.iter().map(|kv| (kv.key.as_str(), kv.value.as_str())).collect::<Vec<_>>(),
            [("key", Some("value"))],
            "{input:?}"
        );
    }
}

#[test]
fn reads_unquoted_assignment_followed_by_quoted_pair() {
    let doc = text::parse("\"root\" { key=value \"b\" \"2\" }").unwrap();
    let root = body(&doc);
    assert_eq!(root.entries.len(), 2);
    assert_eq!(string(&doc, "key"), "value");
    assert_eq!(string(&doc, "b"), "2");
}

#[test]
fn reads_assignments_inside_object() {
    let doc = text::parse("\"root\" { \"key\" { a=1 b=2 } }").unwrap();
    let root = body(&doc);
    assert_eq!(root.entries.len(), 1);
    let nested = value(&doc, "key").as_object().unwrap();
    assert_eq!(nested.entries.len(), 2);
    assert_eq!(nested.get("a").and_then(Value::as_str), Some("1"));
    assert_eq!(nested.get("b").and_then(Value::as_str), Some("2"));
}

#[test]
fn reads_assignment_before_object() {
    for input in ["\"root\" { key = { a 1 } }", "\"root\" { key={a=1} }", "\"root\" { key{ a 1 } }"] {
        let doc = text::parse(input).unwrap_or_else(|err| panic!("{input:?}: {err:?}"));
        let root = body(&doc);
        assert_eq!(root.entries.len(), 1, "{input:?}");
        let nested = value(&doc, "key").as_object().unwrap();
        assert_eq!(nested.get("a").and_then(Value::as_str), Some("1"), "{input:?}");
    }
}

#[test]
fn quoted_special_characters_are_literal() {
    let doc = text::parse("\"root\" { \"a\" \"x=y{}\" \"=\" \"{\" \"b\" \"=\" }").unwrap();
    assert_eq!(string(&doc, "a"), "x=y{}");
    assert_eq!(string(&doc, "="), "{");
    assert_eq!(string(&doc, "b"), "=");
}

#[test]
fn assignment_outside_key_and_value_is_string() {
    let doc = text::parse("\"root\" { a=1=2 key value=x }").unwrap();
    assert_eq!(pairs(&doc), ["a=1", "==2", "key=value", "==x"]);
}

#[test]
fn invalid_assignment_throws() {
    let cases = [
        (
            "\"root\" = { a 1 }",
            (1, 8, "Found '=' after the root key; the root key must be followed by '{'."),
        ),
        (
            "\"root\" { a = }",
            (1, 14, "Attempted to finalize object while in state InObjectAfterAssignment."),
        ),
        (
            "\"root\" { a = = b }",
            (1, 18, "Attempted to finalize object while in state InObjectBetweenKeyAndValue."),
        ),
    ];

    for (input, (line, column, message)) in cases {
        let err = text::parse(input).expect_err(input);
        assert_eq!(syntax_error(err), (line, column, message.to_string()), "{input:?}");
    }
}

#[test]
fn last_conditional_decides() {
    let cases = [
        ("\"root\" { key = [$WIN32] value }", true),
        ("\"root\" { key = [$X360] value }", false),
        ("\"root\" { key [$WIN32] = value }", true),
        ("\"root\" { key [$X360] = value }", false),
        ("\"root\" { key [$X360] = [$WIN32] value }", true),
        ("\"root\" { key [$WIN32] = [$X360] value }", false),
        ("\"root\" { key = value [$X360] }", false),
        ("\"root\" { key [$X360] value [$WIN32] }", true),
        ("\"root\" { key [$WIN32] value [$X360] }", false),
        ("\"root\" { key [$WIN32]=value }", true),
    ];

    for (input, kept) in cases {
        let doc = parse_with_conditions_text(input, &["WIN32"]);
        assert_eq!(body(&doc).get("key").is_some(), kept, "{input:?}");
    }
}

/// A text input parsed with conditions evaluated, like upstream's `AssignmentTestCase.Parse`.
fn parse_with_conditions_text(input: &str, conditions: &[&str]) -> Document {
    let options = ReadOptions {
        conditions: Some(conditions.iter().map(|c| c.to_string()).collect()),
        ..Default::default()
    };
    text::parse_with(input, &options).unwrap_or_else(|err| panic!("{input:?}: {err:?}"))
}

#[test]
fn consecutive_conditionals_throw() {
    let cases = [
        ("\"root\" { key [$WIN32] [$WIN32] value }", 23),
        ("\"root\" { key = [$WIN32] [$WIN32] value }", 25),
        ("\"root\" { key value [$WIN32] [$WIN32] }", 29),
        ("\"root\" [$WIN32] [$WIN32] { }", 17),
    ];

    for (input, column) in cases {
        let options = ReadOptions {
            conditions: Some(vec!["WIN32".into()]),
            ..Default::default()
        };
        let err = text::parse_with(input, &options).expect_err(input);
        assert_eq!(
            syntax_error(err),
            (1, column, "Found a second consecutive conditional.".to_string()),
            "{input:?}"
        );
    }
}

#[test]
fn matching_conditional_after_assignment_replaces_earlier_key() {
    let cases = [
        ("\"root\" { xpos = 1 xpos = [$WIN32] 2 }", "xpos=2"),
        ("\"root\" { xpos = 1 xpos = [$X360] 2 }", "xpos=1"),
        ("\"root\" { xpos 1 xpos [$WIN32] 2 }", "xpos=1 xpos=2"),
        ("\"root\" { xpos 1 xpos = 2 }", "xpos=1 xpos=2"),
        ("\"root\" { a = 1 A = [$WIN32] 2 }", "A=2"),
        ("\"root\" { a 1 b 2 a = [$WIN32] 3 }", "b=2 a=3"),
        ("\"root\" { a 1 a 2 a = [$WIN32] 3 }", "a=2 a=3"),
        ("\"root\" { a = [$WIN32] 3 }", "a=3"),
        ("\"root\" { a 1 a = [$WIN32] 2 [$X360] }", ""),
        ("\"root\" { a [$X360] = [$WIN32] 2 }", "a=2"),
    ];

    for (input, expectation) in cases {
        let doc = parse_with_conditions_text(input, &["WIN32"]);
        assert_eq!(pairs(&doc).join(" "), expectation, "{input:?}");
    }
}

#[test]
fn matching_conditional_after_assignment_replaces_earlier_object() {
    let doc = parse_with_conditions_text("\"root\" { a = [$WIN32] { x 1 } a = [$WIN32] { y 2 } }", &["WIN32"]);

    assert_eq!(body(&doc).entries.len(), 1);
    let a = value(&doc, "a").as_object().unwrap();
    assert_eq!(a.get("y").and_then(Value::as_str), Some("2"));
    assert!(a.get("x").is_none());
}

// ---------------------------------------------------------------------------------------
// ConditionalTestCase / ConditionalsInMultipleObjects
// ---------------------------------------------------------------------------------------

#[test]
fn reads_value_when_conditional_equal() {
    let doc = parse_with_conditions("conditional.vdf", &["WIN32"]);
    assert_eq!(string(&doc, "operating system"), "windows 32-bit");
}

#[test]
fn reads_value_when_conditional_with_or_matches() {
    for condition in ["WIN32", "WIN64"] {
        let doc = parse_with_conditions("conditional.vdf", &[condition]);
        assert_eq!(string(&doc, "platform"), "windows", "{condition}");
    }
}

#[test]
fn reads_value_when_conditional_with_and_matches() {
    let doc = parse_with_conditions("conditional.vdf", &["X360", "X360WIDE"]);
    assert_eq!(string(&doc, "ui type"), "Widescreen Xbox 360");
}

#[test]
fn reads_value_when_conditional_with_and_matches_with_negated_side() {
    let doc = parse_with_conditions("conditional.vdf", &["X360"]);
    assert_eq!(string(&doc, "ui type"), "Xbox 360");
}

#[test]
fn reads_value_when_conditional_with_and_only_matches_one_side() {
    let doc = parse_with_conditions("conditional.vdf", &["X360WIDE"]);
    assert!(body(&doc).get("ui type").is_none());
}

#[test]
fn reads_value_when_both_sides_of_conditional_are_bracketed() {
    for condition in ["WIN32", "WIN64"] {
        let doc = parse_with_conditions("conditional.vdf", &[condition]);
        assert_eq!(string(&doc, "bracketed sides"), "windows", "{condition}");
    }
}

#[test]
fn evaluates_numeric_literals() {
    let doc = parse_with_conditions("conditional.vdf", &[]);

    assert_eq!(string(&doc, "literal true"), "yes");
    assert!(body(&doc).get("literal false").is_none());
    assert_eq!(string(&doc, "literal multi digit"), "yes");
    assert_eq!(string(&doc, "literal brackets"), "yes");
}

#[test]
fn reads_value_when_negated_group_matches() {
    let doc = parse_with_conditions("conditional.vdf", &[]);
    assert_eq!(string(&doc, "negated group"), "yes");
}

#[test]
fn discards_value_when_negated_group_does_not_match() {
    for condition in ["GERMAN", "FRENCH"] {
        let doc = parse_with_conditions("conditional.vdf", &[condition]);
        assert!(body(&doc).get("negated group").is_none(), "{condition}");
    }
}

#[test]
fn reads_value_from_nested_brackets() {
    let doc = parse_with_conditions("conditional.vdf", &["WIN32"]);
    assert_eq!(string(&doc, "nested brackets"), "yes");
}

#[test]
fn and_or_have_equal_precedence_and_are_left_associative() {
    // ($GERMAN && $FRENCH) || $POLISH, not $GERMAN && ($FRENCH || $POLISH).
    let doc = parse_with_conditions("conditional.vdf", &["POLISH"]);
    assert_eq!(string(&doc, "left associative"), "yes");
}

#[test]
fn or_before_and_evaluates_with_equal_precedence() {
    // ($GERMAN || $FRENCH) && $POLISH.
    let doc = parse_with_conditions("conditional.vdf", &["GERMAN"]);
    assert!(body(&doc).get("or before and").is_none());

    let doc = parse_with_conditions("conditional.vdf", &["GERMAN", "POLISH"]);
    assert_eq!(string(&doc, "or before and"), "yes");
}

#[test]
fn supports_double_negation() {
    let doc = parse_with_conditions("conditional.vdf", &["WIN32"]);
    assert_eq!(string(&doc, "double negation"), "yes");

    let doc = parse_with_conditions("conditional.vdf", &[]);
    assert!(body(&doc).get("double negation").is_none());
}

#[test]
fn bare_dollar_evaluates_to_false_without_error() {
    let doc = parse_with_conditions("conditional.vdf", &[]);
    assert!(body(&doc).get("dollar only").is_none());
}

#[test]
fn variable_made_of_digits_is_not_a_literal() {
    let doc = parse_with_conditions("conditional.vdf", &["0"]);
    assert_eq!(string(&doc, "digit variable"), "yes");

    let doc = parse_with_conditions("conditional.vdf", &[]);
    assert!(body(&doc).get("digit variable").is_none());
}

#[test]
fn variable_matching_is_case_insensitive() {
    let doc = parse_with_conditions("conditional.vdf", &["WIN32"]);
    assert_eq!(string(&doc, "lowercase variable"), "yes");

    let doc = parse_with_conditions("conditional.vdf", &["win32"]);
    assert_eq!(string(&doc, "operating system"), "windows 32-bit");

    let doc = parse_with_conditions("conditional.vdf", &[]);
    assert!(body(&doc).get("lowercase variable").is_none());
}

#[test]
fn supports_whitespace_padding_in_conditions() {
    let doc = parse_with_conditions("conditional.vdf", &["WIN32", "X360", "X360WIDE"]);
    assert_eq!(string(&doc, "padded condition"), "yes");
    assert_eq!(string(&doc, "tabbed condition"), "yes");
}

#[test]
fn supports_conditionals_with_underscores() {
    let doc = parse_with_conditions("conditional.vdf", &["SOMETHING_WITH_UNDERSCORE"]);
    assert_eq!(string(&doc, "underscore_condition"), "yes");
}

#[test]
fn reads_value_when_conditional_not_equal() {
    for conditions in [vec![], vec!["OSX"], vec!["LINUX"], vec!["PS3"]] {
        let doc = parse_with_conditions("conditional.vdf", &conditions);
        assert_eq!(string(&doc, "operating system"), "something else", "{conditions:?}");
    }
}

#[test]
fn reads_value_from_complex_bracketed_conditional() {
    let cases = [
        (vec!["X360"], "small"),
        (vec!["X360", "GERMAN"], "medium"),
        (vec!["X360", "FRENCH"], "medium"),
        (vec!["X360", "POLISH"], "large"),
    ];

    for (conditions, expectation) in cases {
        let doc = parse_with_conditions("conditional.vdf", &conditions);
        assert_eq!(string(&doc, "ui size"), expectation, "{conditions:?}");
    }
}

#[test]
fn conditional_in_key() {
    let doc = parse("conditional_in_key.vdf");
    assert_eq!(body(&doc).entries.len(), 1);

    let child = &body(&doc).entries[0];
    assert_eq!(child.key, "operating system [$WIN32]");
    assert_eq!(child.value.as_str(), Some("windows 32-bit"));
    // The brackets sit inside the quoted key, so they are not a conditional at all.
    assert_eq!(child.condition, None);
}

#[test]
fn conditional_before_object_value() {
    let doc = parse_with_conditions("conditional_before_object_value.vdf", &[]);
    assert_eq!(body(&doc).entries.len(), 0);

    let doc = parse_with_conditions("conditional_before_object_value.vdf", &["WIN32"]);
    assert_eq!(body(&doc).entries.len(), 1);
    let child = &body(&doc).entries[0];
    assert_eq!(child.key, "operating system");
    assert_eq!(child.value.as_str(), Some("windows 32-bit"));
}

#[test]
fn conditional_between_key_and_value() {
    let doc = parse_with_conditions("conditional_between_key_and_value.vdf", &[]);
    assert_eq!(body(&doc).entries.len(), 0);

    let doc = parse_with_conditions("conditional_between_key_and_value.vdf", &["WIN32"]);
    assert_eq!(body(&doc).entries.len(), 1);
    let child = &body(&doc).entries[0];
    assert_eq!(child.key, "operating system");
    assert_eq!(child.value.as_str(), Some("windows 32-bit"));
}

#[test]
fn conditional_before_key_throws() {
    let options = ReadOptions {
        conditions: Some(vec!["WIN32".into()]),
        ..Default::default()
    };
    let err = text::parse_with(&fixture("conditional_before_key.vdf"), &options).unwrap_err();
    assert_eq!(
        syntax_error(err),
        (3, 2, "Found conditional while in state InObjectBeforeKey.".to_string())
    );
}

#[test]
fn conditionals_discard_items_in_many_objects() {
    // MOBILE is not defined on any platform upstream supports, so all three pairs are dropped
    // and their objects are left empty.
    let options = ReadOptions { conditions: Some(vec![]), ..Default::default() };
    let doc = text::parse_with(&fixture("conditional_discard.vdf"), &options).unwrap();

    for (object, key) in [("One", "Key1"), ("Two", "Key2"), ("Three", "Key3")] {
        let nested = value(&doc, object).as_object().unwrap_or_else(|| panic!("{object}"));
        assert!(nested.get(key).is_none(), "{object}.{key}");
        assert_eq!(nested.entries.len(), 0, "{object}");
    }
}

#[test]
fn a_false_conditional_discards_what_its_object_was_building() {
    // `SetDiscardCurrent` marks the object of the *current* frame, and the discard is applied
    // when that frame is popped (`FinalizeCurrentObject`), which removes the last item the
    // builder added — upstream's own mechanism, and the reason a matching condition after a
    // pair "wins" over an earlier false one.
    //
    // The condition sits on the frame of `b`, the next key pops that frame, so `b 2` goes with
    // it while `a 1` and `c 3` stay.
    assert_eq!(pairs(&parse_with_conditions_text("\"root\" { a 1 b 2 [$X360] c 3 }", &[])), ["a=1", "c=3"]);
    assert_eq!(
        pairs(&parse_with_conditions_text("\"root\" { a 1 b 2 [$WIN32] c 3 }", &["WIN32"])),
        ["a=1", "b=2", "c=3"]
    );

    // The last pair in the object is dropped the same way.
    assert_eq!(pairs(&parse_with_conditions_text("\"root\" { key [$X360] value }", &[])), Vec::<String>::new());
    assert_eq!(pairs(&parse_with_conditions_text("\"root\" { key value [$X360] }", &[])), Vec::<String>::new());

    // The root pair's own condition discards the object's last item at the end of the document.
    let doc = parse_with_conditions_text("\"root\" [$X360] { key value }", &[]);
    assert_eq!(body(&doc).entries.len(), 0);
    let doc = parse_with_conditions_text("\"root\" [$WIN32] { key value }", &["WIN32"]);
    assert_eq!(pairs(&doc), ["key=value"]);
}

// ---------------------------------------------------------------------------------------
// InvalidConditionalTestCase
// ---------------------------------------------------------------------------------------

#[test]
fn invalid_conditional_throws() {
    let text_fixture = fixture("invalid_conditional.vdf");

    for conditional in [
        "$ABC | $DEF",
        "$ABC & $DEF",
        "$ABC &| $DEF",
        "$ABC |& $DEF",
        "$ABC ! $DEF",
        "!",
        "&&",
        "||",
        "()",
        "$ABC & ()",
        "$ABC && (!)",
        "$ABC && ($DEF!)",
        "(",
        ")",
        "$ABC && ($DEF || $GHI",
        "$ABC && $DEF)",
        "1 2",
        "1$ABC",
        "$ABC 1",
        "1 !",
        "",
        " ",
        "$ABC &&",
        "$ABC ||",
        "$ABC &",
        "$ABC |",
        "$ABC($DEF)",
        "$ABC &&&& $DEF",
    ] {
        let input = text_fixture.replace("{CONDITION}", conditional);
        let err = text::parse(&input).unwrap_err();
        let (line, column, message) = syntax_error(err);
        assert_eq!((line, column), (3, 14), "{conditional:?}");
        assert!(
            message.starts_with(&format!("Invalid conditional syntax \"{conditional}\" (")),
            "{conditional:?}: {message}"
        );
    }
}

#[test]
fn deeply_nested_conditional_is_an_error_not_a_stack_overflow() {
    for conditional in [
        format!("{}$ABC{}", "(".repeat(10_000), ")".repeat(10_000)),
        format!("{}$ABC", "!".repeat(10_000)),
    ] {
        let input = fixture("invalid_conditional.vdf").replace("{CONDITION}", &conditional);
        assert!(text::parse(&input).is_err(), "{conditional:.20}");
    }
}

// ---------------------------------------------------------------------------------------
// Escapes
// ---------------------------------------------------------------------------------------

#[test]
fn escaped_garbage_reads_raw_value_when_escapes_are_off() {
    let doc = parse("escaped_garbage.vdf");
    assert_eq!(string(&doc, "key"), r"abcd\7efg");
}

#[test]
fn escaped_garbage_throws_when_escapes_are_on() {
    let options = ReadOptions { escapes: true, ..Default::default() };
    let err = text::parse_with(&fixture("escaped_garbage.vdf"), &options).unwrap_err();
    assert_eq!(syntax_error(err), (3, 14, r"Unknown escape sequence '\7'.".to_string()));
}

#[test]
fn escaped_garbage_reads_value_with_null_byte_when_bug_compatibility_is_on() {
    let options = ReadOptions { escapes: true, valve_null_byte_bug: true, ..Default::default() };
    let doc = text::parse_with(&fixture("escaped_garbage.vdf"), &options).unwrap();
    assert_eq!(string(&doc, "key"), "abcd");
}

#[test]
fn escaped_whitespace_converts_to_real_characters() {
    let options = ReadOptions { escapes: true, ..Default::default() };
    let doc = text::parse_with(&fixture("escaped_whitespace.vdf"), &options).unwrap();
    assert_eq!(string(&doc, "key"), "line1\nline2\tline2pt2");
}

#[test]
fn escaped_backslash_converts_to_single_backslash() {
    let options = ReadOptions { escapes: true, ..Default::default() };
    let doc = text::parse_with(&fixture("escaped_backslash.vdf"), &options).unwrap();
    assert_eq!(string(&doc, "key"), r"back\slash");
    assert_eq!(string(&doc, "edge case"), r"this is fun\");
}

#[test]
fn escaped_backslashes_stay_literal_when_escapes_are_off() {
    let doc = parse("escaped_backslash.vdf");
    assert_eq!(string(&doc, "key"), r"back\\slash");
    assert_eq!(string(&doc, "edge case"), r"this is fun\\");

    let doc = parse("escaped_backslash_single_slash.vdf");
    assert_eq!(string(&doc, "BuildOutput"), r"..\output\");
}

#[test]
fn escaped_quotation_marks_round_trip_through_the_reader() {
    let options = ReadOptions { escapes: true, ..Default::default() };
    let doc = text::parse_with(&fixture("escaped_quotation_marks.vdf"), &options).unwrap();
    assert_eq!(string(&doc, "name \"of\" key"), "value \"of\" key");
}

#[test]
fn a_stray_escaped_quote_suggests_enabling_escapes() {
    // The value ends at the quote behind the backslash; the stray quote after it opens a
    // string that swallows the following lines and runs into the end of the file.
    let err = text::parse(&fixture("escaped_ending_quote.vdf")).unwrap_err();
    let (line, column, message) = syntax_error(err);
    assert_eq!((line, column), (4, 13));
    assert!(message.starts_with("Found end of file while trying to read the token."));
    assert!(message.contains("KVSerializerOptions.HasEscapeSequences"), "{message}");

    // No escaped quote was read, so the hint must not appear.
    let err = text::parse(&fixture("partial_partialvalue.vdf")).unwrap_err();
    let (_, _, message) = syntax_error(err);
    assert!(!message.contains("HasEscapeSequences"), "{message}");
}

#[test]
fn escaped_ending_quote_parses_with_escapes_on() {
    let options = ReadOptions { escapes: true, ..Default::default() };
    let doc = text::parse_with(&fixture("escaped_ending_quote.vdf"), &options).unwrap();
    assert_eq!(string(&doc, "key"), "some value\"");
    assert_eq!(string(&doc, "foo"), "bar");
}

#[test]
fn font_color_splits_into_two_pairs_when_escapes_are_disabled() {
    let doc = parse("escaped_quote_font_color.vdf");
    let tokens = value(&doc, "Tokens").as_object().unwrap();
    assert_eq!(tokens.entries.len(), 2);
    assert_eq!(tokens.get("leaderboard_region_abbr_Asia").and_then(Value::as_str), Some("<font color=\\"));
    assert_eq!(tokens.get("#fc8200\\").and_then(Value::as_str), Some(">AS</font>"));
}

#[test]
fn font_color_parses_when_escapes_are_enabled() {
    let options = ReadOptions { escapes: true, ..Default::default() };
    let doc = text::parse_with(&fixture("escaped_quote_font_color.vdf"), &options).unwrap();
    let tokens = value(&doc, "Tokens").as_object().unwrap();
    assert_eq!(
        tokens.get("leaderboard_region_abbr_Asia").and_then(Value::as_str),
        Some("<font color=\"#fc8200\">AS</font>")
    );
}

// ---------------------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------------------

#[test]
fn single_line_comments_are_accepted() {
    for name in [
        "comment_singleline.vdf",
        "comment_singleline_wholeline.vdf",
        "comment_singleline_singleslash.vdf",
        "comment_singleline_singleslash_wholeline.vdf",
    ] {
        let doc = parse(name);
        assert_eq!(string(&doc, "foo"), "bar", "{name}");
    }
}

#[test]
fn comment_on_the_end_of_a_line() {
    let input = "\"test_kv\"\n{\n//\n\"test\"\t\"hello\"\n}\n";
    let doc = text::parse(input).unwrap();
    assert_eq!(body(&doc).entries.len(), 1);
    assert_eq!(string(&doc, "test"), "hello");
}

#[test]
fn comment_with_a_carriage_return_is_one_line() {
    let input = "\"test_kv\"\r\n{\r\n// this is a comment that contains a carriage return: \r [$INVALID] which should continue parsing\r\n\"test\"\t\"hello\"\r\n}\r\n";
    let doc = text::parse(input).unwrap();
    assert_eq!(body(&doc).entries.len(), 1);
    assert_eq!(string(&doc, "test"), "hello");
}

#[test]
fn trailing_comments_after_the_root_object_are_accepted() {
    for trailer in ["/", "// foo"] {
        let input = format!(
            "\"vertexlitgeneric\" {{ \t\"$basetexture\" \"models/props_oil/doors/oil_door\" \t\"$surfaceprop\" \"metal\"\t\"%keywords\" \"tf\"\t}}\n\n{trailer}\n"
        );
        let doc = text::parse(&input).unwrap_or_else(|err| panic!("{trailer:?}: {err:?}"));
        assert_eq!(doc.key, "vertexlitgeneric");
        assert_eq!(string(&doc, "$basetexture"), "models/props_oil/doors/oil_door");
        assert_eq!(string(&doc, "$surfaceprop"), "metal");
        assert_eq!(string(&doc, "%keywords"), "tf");
    }
}

// ---------------------------------------------------------------------------------------
// UnquotedTextTestCase
// ---------------------------------------------------------------------------------------

#[test]
fn unquoted_document_parses() {
    let doc = parse("unquoted_document.vdf");
    assert_eq!(doc.key, "TestDocument");
    // With escapes off the backslash is literal.
    assert_eq!(string(&doc, "QuotedChild"), r"edge\ncase");

    let unquoted = value(&doc, "UnquotedChild").as_object().unwrap();
    assert_eq!(unquoted.get("Key1").and_then(Value::as_str), Some("Value1"));
    assert_eq!(unquoted.get("Key2").and_then(Value::as_str), Some("Value2"));
    assert_eq!(unquoted.get("Key3").and_then(Value::as_str), Some("Value3"));
}

// ---------------------------------------------------------------------------------------
// Includes: IncludeTestCase, BaseIncludeTestCase, BaseIncludeNestedTestCase,
// IncludeNestedTestCase, IncludeErrorsTestCase
// ---------------------------------------------------------------------------------------

/// A loader that answers from the fixture directory and records every path it was asked for.
/// `file.vdf` — the path upstream's stub loader asserts on — is answered with `included`.
fn fixture_loader<'a>(
    included: &'a str,
    calls: &'a std::cell::RefCell<Vec<String>>,
) -> impl Fn(&str) -> Result<String, String> + 'a {
    move |path: &str| {
        calls.borrow_mut().push(path.to_string());
        let name = if path == "file.vdf" { included } else { path };
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/text").join(name);
        std::fs::read_to_string(&file).map_err(|err| format!("{}: {err}", file.display()))
    }
}

fn parse_with_loader(name: &str, included: &str, calls: &std::cell::RefCell<Vec<String>>) -> Document {
    let loader = fixture_loader(included, calls);
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    text::parse_with(&fixture(name), &options).unwrap_or_else(|err| panic!("{name}: {err:?}"))
}

#[test]
fn include_appends_the_included_keys() {
    let calls = std::cell::RefCell::new(Vec::new());
    let doc = parse_with_loader("kv_with_include.vdf", "kv_included.vdf", &calls);

    assert_eq!(*calls.borrow(), ["file.vdf"]);
    let root = body(&doc);
    assert_eq!(root.entries.len(), 3);
    assert_eq!(root.get("foo").and_then(Value::as_str), Some("bar"));
    assert_eq!(root.get("bar").and_then(Value::as_str), Some("baz"));
    assert_eq!(root.get("baz").and_then(Value::as_str), Some("nada"));
}

#[test]
fn base_merges_and_the_document_wins() {
    let calls = std::cell::RefCell::new(Vec::new());
    let doc = parse_with_loader("kv_with_base.vdf", "kv_base_included.vdf", &calls);

    assert_eq!(*calls.borrow(), ["file.vdf"]);
    let root = body(&doc);
    assert_eq!(root.entries.len(), 3);
    // kv_base_included.vdf has foo=baz; the document's own foo=bar wins.
    assert_eq!(root.get("foo").and_then(Value::as_str), Some("bar"));
    assert_eq!(root.get("bar").and_then(Value::as_str), Some("baz"));
    assert_eq!(root.get("baz").and_then(Value::as_str), Some("nada"));
}

#[test]
fn base_includes_nest() {
    let calls = std::cell::RefCell::new(Vec::new());
    let doc = parse_with_loader("kv_with_base_nesting.vdf", "kv_base_nested_once.vdf", &calls);

    assert_eq!(*calls.borrow(), ["kv_base_nested_once.vdf", "kv_base_nested_twice.vdf"]);
    let root = body(&doc);
    assert_eq!(root.entries.len(), 4);
    assert_eq!(root.get("foo").and_then(Value::as_str), Some("bar"));
    assert_eq!(root.get("bar").and_then(Value::as_str), Some("baz"));
    assert_eq!(root.get("aaa").and_then(Value::as_str), Some("bbb"));
    assert_eq!(root.get("ccc").and_then(Value::as_str), Some("ddd"));
}

#[test]
fn includes_nest() {
    let calls = std::cell::RefCell::new(Vec::new());
    let doc = parse_with_loader("kv_with_include_nesting.vdf", "kv_include_nested_once.vdf", &calls);

    assert_eq!(*calls.borrow(), ["kv_include_nested_once.vdf", "kv_include_nested_twice.vdf"]);
    let root = body(&doc);
    assert_eq!(root.entries.len(), 3);
    assert_eq!(root.get("foo").and_then(Value::as_str), Some("bar"));
    assert_eq!(root.get("aaa").and_then(Value::as_str), Some("bbb"));
    assert_eq!(root.get("ccc").and_then(Value::as_str), Some("ddd"));
}

#[test]
fn base_merges_nested_objects_recursively() {
    // The upstream fixtures only merge flat keys (`kv_base_included.vdf`); this pins
    // `KVMergingObjectBuilder.Merge(KVObject, KVObject)`, which walks into collections: the
    // owner's value wins for a key both sides have, and the base's own keys are appended after
    // the owner's.
    let loader = |_: &str| -> Result<String, String> {
        Ok("\"base\"\n{\n\t\"a\"\n\t{\n\t\t\"x\"\t\"base\"\n\t\t\"y\"\t\"base\"\n\t}\n}\n".to_string())
    };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    let document = text::parse_with(
        "#base \"b.vdf\"\n\"main\"\n{\n\t\"a\"\n\t{\n\t\t\"y\"\t\"main\"\n\t\t\"z\"\t\"main\"\n\t}\n}\n",
        &options,
    )
    .unwrap();

    let a = body(&document).get("a").and_then(Value::as_object).unwrap();
    assert_eq!(
        a.entries.iter().map(|kv| (kv.key.as_str(), kv.value.as_str())).collect::<Vec<_>>(),
        [("y", Some("main")), ("z", Some("main")), ("x", Some("base"))]
    );
}

#[test]
fn a_base_can_include_and_an_include_can_base() {
    // The included file's own directives run first (merging, then appending), and only then does
    // its result land in the document: `KVObjectBuilder.GetObject` finalizes the whole chain.
    let loader = |path: &str| -> Result<String, String> {
        match path {
            "b.vdf" => Ok("#include \"c.vdf\"\n\"base\"\n{\n\t\"from base\"\t\"yes\"\n}\n".to_string()),
            "c.vdf" => Ok("\"appended\"\n{\n\t\"from include\"\t\"yes\"\n}\n".to_string()),
            other => Err(format!("no such file: {other}")),
        }
    };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    let document =
        text::parse_with("#base \"b.vdf\"\n\"main\"\n{\n\t\"mine\"\t\"yes\"\n}\n", &options).unwrap();

    assert_eq!(pairs(&document), ["mine=yes", "from base=yes", "from include=yes"]);
}

#[test]
fn include_inside_an_object_is_an_ordinary_pair() {
    let doc = text::parse("\"root\"\n{\n#include \"foo.txt\"\n}\n").unwrap();
    assert_eq!(string(&doc, "#include"), "foo.txt");

    let doc = text::parse("\"root\"\n{\n    #base \"foo.txt\"\n}\n").unwrap();
    assert_eq!(string(&doc, "#base"), "foo.txt");

    let doc = text::parse("\"root\" { labelText #DOTA_Label }").unwrap();
    assert_eq!(string(&doc, "labelText"), "#DOTA_Label");
}

#[test]
fn include_outside_the_root_object_is_a_directive() {
    let calls = std::cell::RefCell::new(Vec::new());
    let loader = move |path: &str| -> Result<String, String> {
        calls.borrow_mut().push(path.to_string());
        assert_eq!(path, "foo.txt");
        Ok("\"foo\" { \"included\" \"yes\" }".to_string())
    };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };

    for input in [
        "#include \"foo.txt\" \"root\" { }",
        "#INCLUDE \"foo.txt\" \"root\" { }",
        "\"#include\" \"foo.txt\" \"root\" { }",
        "#include\"foo.txt\" \"root\" { }",
        "\"root\" { } #include \"foo.txt\"",
    ] {
        let doc = text::parse_with(input, &options).unwrap_or_else(|err| panic!("{input:?}: {err:?}"));
        assert_eq!(string(&doc, "included"), "yes", "{input:?}");

        // #base goes through the same state, but merges instead.
        let base_input = input.replace("#include", "#base").replace("#INCLUDE", "#Base").replace("\"#include\"", "\"#base\"");
        let doc = text::parse_with(&base_input, &options).unwrap_or_else(|err| panic!("{base_input:?}: {err:?}"));
        assert_eq!(string(&doc, "included"), "yes", "{base_input:?}");
    }
}

#[test]
fn invalid_directive_throws() {
    let cases = [
        ("#include", (1, 9, "Found end of file when another token type was expected.")),
        (
            "#include { }",
            (1, 10, "Attempted to begin new object while in state InDocumentBeforeIncludePath."),
        ),
        ("#base \"\" \"root\" { }", (1, 7, "Found an inclusion directive with an empty file path.")),
    ];

    for (input, expectation) in cases {
        let err = text::parse(input).expect_err(input);
        assert_eq!(
            syntax_error(err),
            (expectation.0, expectation.1, expectation.2.to_string()),
            "{input:?}"
        );
    }
}

#[test]
fn an_include_without_a_loader_is_an_error() {
    let err = text::parse("#base \"file.vdf\"\n\"root\" { }\n").unwrap_err();
    assert_eq!(
        err,
        Error::Include {
            path: "file.vdf".to_string(),
            message: "inclusions require ReadOptions::include_loader to be set".to_string(),
        }
    );
}

#[test]
fn a_failing_loader_is_reported_with_its_path() {
    let loader = |path: &str| -> Result<String, String> { Err(format!("no such file: {path}")) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    let err = text::parse_with("#include \"missing.vdf\"\n\"root\" { }\n", &options).unwrap_err();
    assert_eq!(
        err,
        Error::Include { path: "missing.vdf".to_string(), message: "no such file: missing.vdf".to_string() }
    );
}

#[test]
fn a_self_including_chain_stops_at_the_depth_cap() {
    let loader = |_: &str| -> Result<String, String> { Ok("#include \"self.vdf\"\n\"root\" { }\n".to_string()) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };

    let err = text::parse_with("#include \"self.vdf\"\n\"root\" { }\n", &options).unwrap_err();
    match err {
        Error::Include { message, .. } => {
            assert!(message.contains("deeper than the supported maximum of 64"), "{message}")
        }
        other => panic!("expected an include error, got {other:?}"),
    }
}

#[test]
fn errors_from_an_included_file_name_that_file() {
    // A loader that answers with a BOM-only file: the included document has no root object, and
    // the error has to say which file that was — a bare position would read as line 1 of the
    // document that asked for it.
    let loader = |_: &str| -> Result<String, String> { Ok("\u{feff}".to_string()) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    let err = text::parse_with("#base \"empty.vdf\"\n\"root\" { }\n", &options).unwrap_err();
    match err {
        Error::Include { path, message } => {
            assert_eq!(path, "empty.vdf");
            assert!(message.starts_with("line 1, column 1: "), "{message}");
            assert!(message.contains("not in a fully completed state"), "{message}");
        }
        other => panic!("expected an include error, got {other:?}"),
    }

    // The same for a plain syntax error inside the included file.
    let loader = |_: &str| -> Result<String, String> { Ok("\"x\" {\n".to_string()) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    let err = text::parse_with("#include \"broken.vdf\"\n\"root\" { }\n", &options).unwrap_err();
    match err {
        Error::Include { path, message } => {
            assert_eq!(path, "broken.vdf");
            assert_eq!(message, "line 2, column 1: Found end of file when another token type was expected.");
        }
        other => panic!("expected an include error, got {other:?}"),
    }

    // And the including document's own missing root object stays a syntax error at its end.
    let loader = |_: &str| -> Result<String, String> { Ok("\"b\" { }\n".to_string()) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    let err = text::parse_with("#base \"b.vdf\"\n", &options).unwrap_err();
    match err {
        Error::Syntax { line, column, message } => {
            // The end of the including document (its `#base` line plus the newline).
            assert_eq!((line, column), (2, 1));
            assert!(message.contains("not in a fully completed state"), "{message}");
        }
        other => panic!("expected a syntax error, got {other:?}"),
    }
}

#[test]
fn a_fan_out_of_includes_hits_the_file_cap_instead_of_allocating() {
    // A finite but exponentially wide chain: `n0` names `n1` twice, `n1` names `n2` twice, and so
    // on, so the tree is 2^40 documents that are all within the depth cap. Upstream builds one
    // reader and one builder per file (this port was not measured against a built upstream), so
    // by reasoning it would exhaust memory long before 2^40 documents; the per-parse cap turns
    // it into an error instead.
    // The same shape with a depth that still fits (2^10 - 1 = 1023 files, the leaf level
    // included) parses, which also pins that the fan-out really is exponential: the loader is
    // asked for exactly that many files.
    let calls = std::cell::RefCell::new(0usize);
    let loader = |depth_limit: usize, calls: &std::cell::RefCell<usize>, path: &str| {
        let depth: usize = path
            .trim_start_matches('n')
            .trim_end_matches(".vdf")
            .parse()
            .map_err(|_| format!("unexpected include path: {path}"))?;
        *calls.borrow_mut() += 1;
        if depth >= depth_limit {
            return Ok("\"root\" { }\n".to_string());
        }
        Ok(format!(
            "#base \"n{0}.vdf\"\n#base \"n{0}.vdf\"\n\"root\" {{ }}\n",
            depth + 1
        ))
    };

    {
        let loader = |path: &str| loader(9, &calls, path);
        let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
        let document = text::parse_with("#base \"n0.vdf\"\n\"root\" { }\n", &options).unwrap();
        assert_eq!(*calls.borrow(), 1023);
        assert_eq!(body(&document).entries.len(), 0);
    }

    {
        let loader = |path: &str| loader(40, &calls, path);
        let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
        let err = text::parse_with("#base \"n0.vdf\"\n\"root\" { }\n", &options).unwrap_err();
        match err {
            Error::Include { message, .. } => {
                assert!(message.contains("more than 1024 files"), "{message}");
            }
            other => panic!("expected an include error, got {other:?}"),
        }
    }
}

#[test]
fn a_level_of_many_includes_hits_the_file_cap() {
    // The same cap, reached the shallow way: one document naming two thousand files.
    let loader = |_: &str| -> Result<String, String> { Ok("\"root\" { }\n".to_string()) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };

    let mut text = String::new();
    for index in 0..2000 {
        text.push_str(&format!("#base \"leaf{index}.vdf\"\n"));
    }
    text.push_str("\"root\" { }\n");

    let err = text::parse_with(&text, &options).unwrap_err();
    match err {
        Error::Include { message, .. } => {
            assert!(message.contains("more than 1024 files"), "{message}");
        }
        other => panic!("expected an include error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// InvalidSyntaxTestCase
// ---------------------------------------------------------------------------------------

#[test]
fn invalid_text_syntax_throws() {
    for name in [
        "empty.vdf",
        "quoteonly.vdf",
        "partialname.vdf",
        "nameonly.vdf",
        "partial_nodata.vdf",
        "partial_opening_key.vdf",
        "partial_partialkey.vdf",
        "partial_novalue.vdf",
        "partial_opening_value.vdf",
        "partial_partialvalue.vdf",
        "partial_noclose.vdf",
        "invalid_zerobracerepeated.vdf",
        "multiple_root_objects.vdf",
    ] {
        let input = fixture(name);
        assert!(text::parse(&input).is_err(), "{name} must be rejected");
    }
}

// ---------------------------------------------------------------------------------------
// MultipleRootObjectsTestCase (KV1 text)
// ---------------------------------------------------------------------------------------

#[test]
fn second_root_object_in_text_throws() {
    let err = text::parse(&fixture("multiple_root_objects.vdf")).unwrap_err();
    let (line, column, message) = syntax_error(err);
    assert_eq!((line, column), (6, 1));
    assert_eq!(
        message,
        "Found data after the root object; documents with multiple root objects are not supported."
    );
}

#[test]
fn second_top_level_pair_in_text_throws() {
    assert!(text::parse("\"a\"\t\"1\"\n\"b\"\t\"2\"\n").is_err());
}

#[test]
fn stray_object_end_in_text_throws() {
    let err = text::parse("\"a\"\n{\n}\n}\n").unwrap_err();
    let (line, column, _) = syntax_error(err);
    assert_eq!((line, column), (4, 1));
}

#[test]
fn second_root_object_after_stray_object_end_in_text_throws() {
    assert!(text::parse("\"a\"\n{\n}\n}\n\"b\"\n{\n}\n").is_err());
}

#[test]
fn trailing_conditional_after_the_root_in_text_throws() {
    for conditional in ["[$NEVERDEFINED]", "[!$NEVERDEFINED]"] {
        let input = format!("\"a\"\n{{\n\t\"k\" \"v\"\n}}\n{conditional}\n");
        let err = text::parse(&input).unwrap_err();
        let (line, column, _) = syntax_error(err);
        assert_eq!((line, column), (5, 1), "{conditional}");
    }
}

// ---------------------------------------------------------------------------------------
// Values are text (the difference from upstream's ParseValue type guessing)
// ---------------------------------------------------------------------------------------

#[test]
fn numbers_keep_their_source_text() {
    let doc = parse("numbers.vdf");
    let root = body(&doc);

    let cases = [
        ("0", "12,34"),
        ("1", "12, 34"),
        ("2", "12 ,34"),
        ("3", "12.34"),
        ("4", "12. 34"),
        ("5", "12 .34"),
        ("6", "ab,34"),
        ("7", "12,cd"),
        ("8", "+12.34"),
        ("9", "-12.34"),
        ("10", "-12.34+"),
        ("11", "+12.34+"),
        ("12", "+12.34-"),
        ("13", "-12.34-"),
        ("14", "        \t-12.34"),
        ("15", "-12.34   \t"),
        ("16", "0"),
        ("17", "2147483647"),
        ("18", "2147483648"),
        ("19", "   \t  123456789"),
        ("20", "6404082971767543753"),
        ("21", "10e6"),
        ("22", "-9223372036854775808"),
        ("23", "9223372036854775807"),
        ("24", "18446744073709551615"),
        ("25", "4294967295"),
        ("26", "-2147483648"),
        ("27", "  -123456789012345"),
        ("28", "  +123456789012345"),
        ("29", "  -123456789012345.67"),
        ("30", "  +123456789012345e1"),
    ];

    assert_eq!(root.entries.len(), cases.len());
    for (key, expectation) in cases {
        assert_eq!(root.get(key).and_then(Value::as_str), Some(expectation), "{key}");
    }
}

#[test]
fn type_guessing_fixtures_stay_text() {
    // Upstream turns these into Int32/Float32/UInt64; this port keeps the source text (and so
    // does not lose the exact spelling of a `0x` value).
    let guesses = [
        ("int", "1234"),
        ("negint", "-1234"),
        ("float", "123.456"),
        ("float_exp", "1.23456E+2"),
        ("bigint", "0x0000000000000001"),
        ("gaben_steamid", "0x01100001000056bA"),
        ("string", "123foo"),
    ];

    let doc = parse("type_guessing.vdf");
    let root = body(&doc);
    for (key, expectation) in guesses {
        let value = root.get(key).unwrap_or_else(|| panic!("{key}"));
        assert!(matches!(value, Value::String(_)), "{key} must stay a string");
        assert_eq!(value.as_str(), Some(expectation), "{key}");
    }

    let doc = parse("uint64_hex.vdf");
    let root = body(&doc);
    for (key, expectation) in [
        ("valid_lowercase", "0x1122334455667788"),
        ("valid_uppercase_digits", "0x1122AABBCCDDEEFF"),
        ("valid_mixed_case_digits", "0xAaBbCcDdEeFf0011"),
        ("garbage_brackets", "0x11223344556677[]"),
        ("garbage_above_f", "0xzzzzzzzzzzzzzzzz"),
        ("garbage_minus", "0x-122334455667788"),
        ("garbage_overflow", "0xg000000000000000"),
        ("garbage_below_zero", "0x////////////////"),
        ("uppercase_x", "0X1122334455667788"),
        ("too_short", "0x112233445566778"),
        ("too_long", "0x11223344556677889"),
        ("garbage_second_x", "0xx000000000000000"),
    ] {
        let value = root.get(key).unwrap_or_else(|| panic!("{key}"));
        assert!(matches!(value, Value::String(_)), "{key} must stay a string");
        assert_eq!(value.as_str(), Some(expectation), "{key}");
    }
}

#[test]
fn boolean_values_are_text() {
    // upstream BooleanTestCase.DynamicDeserialization and StronglyTypedDeserialization convert
    // these three values to `bool` (`0` false, anything else true). This value model has no
    // boolean — a KV1 text value is its source text — so what is pinned here is the text the
    // conversions upstream runs on.
    let doc = parse("boolean.vdf");

    assert_eq!(string(&doc, "test1_false"), "0");
    assert_eq!(string(&doc, "test2_true"), "1");
    assert_eq!(string(&doc, "test3_oob"), "2");
    assert!(body(&doc).entries.iter().all(|kv| matches!(kv.value, Value::String(_))));
}

#[test]
fn lossless_mode_cannot_reproduce_the_after_assignment_replacement() {
    // Documented as a known lossy case: a matching conditional after '=' removes the earlier pair
    // with the same key upstream, and a tree that keeps only the condition *text* per pair cannot
    // say where that condition sat.
    let input = "\"root\" { \"a\" \"1\" \"a\" = [$X] \"2\" }";
    assert_eq!(pairs(&parse_with_conditions_text(input, &["X"])), ["a=2"]);

    let doc = text::parse(input).unwrap();
    assert_eq!(pairs(&doc), ["a=1", "a=2"]);
    assert_eq!(body(&doc).entries[1].condition.as_deref(), Some("$X"));

    // Evaluating the written document keeps both pairs: the replacement is not recoverable.
    let written = text::write(&doc);
    assert_eq!(pairs(&parse_with_conditions_text(&written, &["X"])), ["a=1", "a=2"]);
}

#[test]
fn a_base_merge_compares_keys_not_conditions() {
    // The other known lossy case: the merge runs on the trees as read, so an unconditional pair
    // from a base loses to a conditional one in the including file — and with the condition false
    // the base's value is the one that survives when the read evaluates conditions.
    let base = "\"r\"\n{\n\t\"a\"\t\"2\"\n}\n";
    let main = "#base \"b.vdf\"\n\"r\"\n{\n\t\"a\"\t\"1\"\t[$X]\n}\n";

    let loader = |_: &str| -> Result<String, String> { Ok(base.to_string()) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    let doc = text::parse_with(main, &options).unwrap();
    assert_eq!(pairs(&doc), ["a=1"]);
    assert_eq!(condition_of(&doc, "a"), Some("$X"));

    let loader = |_: &str| -> Result<String, String> { Ok(base.to_string()) };
    let options = ReadOptions {
        conditions: Some(vec![]),
        include_loader: Some(&loader),
        ..Default::default()
    };
    let doc = text::parse_with(main, &options).unwrap();
    assert_eq!(pairs(&doc), ["a=2"]);
}

// ---------------------------------------------------------------------------------------
// Structure: lists, duplicates, nesting
// ---------------------------------------------------------------------------------------

#[test]
fn list_of_values() {
    let doc = parse("list_of_values.vdf");
    let numbers = value(&doc, "Numbers").as_object().unwrap();
    let expected: Vec<String> = (0..14).map(|i| i.to_string()).collect();
    assert_eq!(numbers.entries.iter().map(|kv| kv.key.clone()).collect::<Vec<_>>(), expected);

    for (index, name) in ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve", "thirteen"].iter().enumerate() {
        assert_eq!(numbers.get(&index.to_string()).and_then(Value::as_str), Some(*name));
    }
}

#[test]
fn top_level_list_of_values() {
    let doc = parse("top_level_list_of_values.vdf");
    let root = body(&doc);
    assert_eq!(root.entries.len(), 14);
    assert_eq!(root.get("0").and_then(Value::as_str), Some("zero"));
    assert_eq!(root.get("13").and_then(Value::as_str), Some("thirteen"));
}

#[test]
fn list_of_lists() {
    let doc = parse("list_of_lists.vdf");
    let values = value(&doc, "Values").as_object().unwrap();
    assert_eq!(values.entries.len(), 2);
    assert_eq!(
        values.get("0").and_then(Value::as_object).and_then(|o| o.get("1")).and_then(Value::as_str),
        Some("second")
    );
    assert_eq!(
        values.get("1").and_then(Value::as_object).and_then(|o| o.get("1")).and_then(Value::as_str),
        Some("fourth")
    );
}

#[test]
fn duplicate_lists_keep_both_values() {
    let doc = parse("duplicate_lists.vdf");
    let root = body(&doc);
    assert_eq!(root.entries.len(), 2);
    assert!(root.entries.iter().all(|kv| kv.key == "Values"));

    let first = root.entries[0].value.as_object().unwrap();
    assert_eq!(first.get("0").and_then(Value::as_object).and_then(|o| o.get("name")).and_then(Value::as_str), Some("first"));
    let second = root.entries[1].value.as_object().unwrap();
    assert_eq!(second.get("3").and_then(Value::as_object).and_then(|o| o.get("name")).and_then(Value::as_str), Some("fourth"));
}

#[test]
fn duplicate_keys_keep_their_order() {
    let doc = parse("duplicate_keys.vdf");
    assert_eq!(pairs(&doc), ["foo=foo", "foo=bar", "foo=baz"]);

    let doc = parse("duplicate_keys_object.vdf");
    let lookup = value(&doc, "FooLookup").as_object().unwrap();
    assert_eq!(
        lookup.get_all("Bar").filter_map(Value::as_str).collect::<Vec<_>>(),
        ["First Bar", "Second Bar", "Third Bar"]
    );
}

#[test]
fn nested_object_graph() {
    let doc = parse("nested_object_graph.vdf");
    assert_eq!(string(&doc, "OGInt"), "3");

    let foo = value(&doc, "FooObj").as_object().unwrap();
    assert_eq!(foo.get("FooStr").and_then(Value::as_str), Some("blah"));
    let bar = foo.get("BarObj").and_then(Value::as_object).unwrap();
    assert_eq!(bar.get("Baz").and_then(Value::as_str), Some("blahdiladila"));
}

#[test]
fn non_circular_list_and_dictionary_with_array_values() {
    let doc = parse("non_circular_list.vdf");
    assert_eq!(value(&doc, "Strings").as_object().unwrap().entries.len(), 2);
    assert_eq!(value(&doc, "Ints").as_object().unwrap().entries.len(), 3);

    let doc = parse("dictionary_with_array_values.vdf");
    let nested = value(&doc, "test").as_object().unwrap();
    assert_eq!(nested.entries.len(), 2);
    assert_eq!(
        nested.get("test2").and_then(Value::as_object).and_then(|o| o.get("2")).and_then(Value::as_str),
        Some("3.54677")
    );
}

#[test]
fn object_person_and_attributes() {
    let doc = parse("object_person.vdf");
    assert_eq!(string(&doc, "FirstName"), "Bob");
    assert_eq!(string(&doc, "LastName"), "Builder");
    assert_eq!(string(&doc, "CanFixIt"), "1");

    let doc = parse("object_person_attributes.vdf");
    assert_eq!(doc.key, "object");
    assert_eq!(string(&doc, "First Name"), "Bob");
    assert_eq!(string(&doc, "Can Fix It"), "1");
}

#[test]
fn legacy_depot_data_subset() {
    let doc = parse("legacydepotdata_subset.vdf");
    assert_eq!(doc.key, "depots");
    let root = body(&doc);
    assert_eq!(root.entries.len(), 4);
    assert_eq!(root.get("0").and_then(Value::as_str), Some("A10D0BCD94CE6105D0E2256FE06B2B22"));
    assert_eq!(root.get("1").and_then(Value::as_str), Some("60EF870FB4B9A2EBB5E511BC8CEC8858"));
    assert_eq!(root.get("3").and_then(Value::as_str), Some("AA4CA0B6300E96774BC3C2B55C3388C9"));
    assert_eq!(root.get("7").and_then(Value::as_str), Some("636DC4B351732EDC6022021B89124CC9"));
    for garbage in ["2", "asdasd", "123", "hello there"] {
        assert!(root.get(garbage).is_none(), "{garbage}");
    }
}

#[test]
fn vrf_settings_sample() {
    let doc = parse("vrf_settings_sample.vdf");
    assert_eq!(doc.key, "ValveResourceFormat");
    assert_eq!(string(&doc, "backgroundColor"), "#3C3C3C");
    assert_eq!(string(&doc, "openDirectory"), r"D:\SteamLibrary\steamapps\common\The Lab\RobotRepair\vr");
    assert_eq!(string(&doc, "saveDirectory"), r"D:\SteamLibrary\steamapps\common\The Lab\RobotRepair\vr");
    assert_eq!(value(&doc, "GameSearchPaths").as_object().unwrap().entries.len(), 0);
}

#[test]
fn nullable_types_and_required_init_person() {
    let doc = parse("nullable_types.vdf");
    assert_eq!(string(&doc, "Name"), "TestName");
    assert_eq!(string(&doc, "Description"), "TestDescription");
    assert_eq!(value(&doc, "Numbers").as_object().unwrap().entries.len(), 3);

    let doc = parse("required_init_person.vdf");
    assert_eq!(string(&doc, "FirstName"), "Alice");
    assert_eq!(string(&doc, "Age"), "30");
}

#[test]
fn random_case_object_preserves_key_spelling() {
    let doc = parse("random_case_object.vdf");
    assert_eq!(pairs(&doc).len(), 3);
    assert_eq!(string(&doc, "DESCRIPTION"), "Dota 2 is a complex game where you get sworn at\nin Russian all the time.");
    assert_eq!(string(&doc, "developer"), "Valve Software");
    assert_eq!(string(&doc, "nAME"), "Dota 2");
}

#[test]
fn steam_440_is_read_the_way_valve_tier1_reads_it() {
    let doc = parse("steam_440.vdf");
    assert_eq!(doc.key, "appinfo");

    let config = value(&doc, "config").as_object().unwrap();
    assert_eq!(
        config.get("cleanupcmds").and_then(Value::as_str),
        Some(r"if not exist hl2.exe goto EOF;ren \")
    );
    assert_eq!(config.get(r"..\..\common\Team").and_then(Value::as_str), Some("Fortress"));
    assert_eq!(
        config.get(r"2\").and_then(Value::as_str),
        Some(r" TF2_bak;del /q /s bin\*;cd tf;del /q /s *.cache media\*.mov;mkdir download;cd download;move ..\maps .\;move ..\materials .\;move ..\models .\;move ..\particles .\;move ..\resource .\;move ..\sound .\;move ..\scripts .\;cd resource;del game.ico tf.ttf tf2.ttf tf2build.ttf tf2professor.ttf tf2secondary.ttf tfd.ttf;cd ..\sound\ui;del gamestartup*.mp3 tv_tune*.mp3 holiday\gamestartup_*.mp3;:EOF")
    );
}

// ---------------------------------------------------------------------------------------
// Conditionals kept: the default (lossless) mode
// ---------------------------------------------------------------------------------------

#[test]
fn every_pair_keeps_its_condition() {
    let doc = parse("conditional.vdf");
    let root = body(&doc);

    // Both "operating system" pairs survive, each with the condition it was written under.
    let operating_systems: Vec<(&str, Option<&str>)> = root
        .entries
        .iter()
        .filter(|kv| kv.key == "operating system")
        .map(|kv| (kv.value.as_str().unwrap(), kv.condition.as_deref()))
        .collect();
    assert_eq!(
        operating_systems,
        [("windows 32-bit", Some("$WIN32")), ("something else", Some("!$WIN32"))]
    );

    assert_eq!(condition_of(&doc, "platform"), Some("$WIN32||$WIN64"));
    assert_eq!(condition_of(&doc, "padded condition"), Some(" $WIN32 "));
    assert_eq!(condition_of(&doc, "tabbed condition"), Some("\t$X360\t&&\t$X360WIDE\t"));
    assert_eq!(condition_of(&doc, "literal true"), Some("1"));
    assert_eq!(condition_of(&doc, "double negation"), Some("!!$WIN32"));
    assert_eq!(condition_of(&doc, "dollar only"), Some("$"));
    assert_eq!(condition_of(&doc, "digit variable"), Some("$0"));

    // Every pair in this fixture is conditional.
    assert!(root.entries.iter().all(|kv| kv.condition.is_some()));
}

fn condition_of<'a>(doc: &'a Document, key: &str) -> Option<&'a str> {
    body(doc)
        .entries
        .iter()
        .find(|kv| kv.key == key)
        .unwrap_or_else(|| panic!("{key}"))
        .condition
        .as_deref()
}

#[test]
fn the_condition_sits_on_the_pair_in_every_position() {
    for input in [
        // after the value
        "\"root\" { \"key\" \"value\" [$WIN32] }",
        // between the key and the value
        "\"root\" { \"key\" [$WIN32] \"value\" }",
        // after the '='
        "\"root\" { \"key\" = [$WIN32] \"value\" }",
        // before the '='
        "\"root\" { \"key\" [$WIN32] = \"value\" }",
        // before an object value
        "\"root\" { \"key\" [$WIN32] { \"a\" \"1\" } }",
        // after an object value
        "\"root\" { \"key\" { \"a\" \"1\" } [$WIN32] }",
    ] {
        let doc = text::parse(input).unwrap_or_else(|err| panic!("{input:?}: {err:?}"));
        let pair = body(&doc).entries.first().unwrap_or_else(|| panic!("{input:?}"));
        assert_eq!(pair.key, "key", "{input:?}");
        assert_eq!(pair.condition.as_deref(), Some("$WIN32"), "{input:?}");
    }
}

#[test]
fn the_root_pairs_condition_survives() {
    let doc = parse("conditional_before_object_value.vdf");
    assert_eq!(doc.key, "test case");
    assert_eq!(doc.condition.as_deref(), Some("$WIN32"));
    // Nothing is discarded: the object's pair is still there.
    assert_eq!(body(&doc).entries.len(), 1);

    // The same position for a scalar root pair.
    let doc = text::parse("\"a\" [$WIN32] \"1\"").unwrap();
    assert_eq!(doc.key, "a");
    assert_eq!(doc.condition.as_deref(), Some("$WIN32"));
    assert_eq!(doc.value.as_str(), Some("1"));
}

#[test]
fn the_last_condition_wins_when_a_pair_has_several() {
    for input in [
        "\"root\" { key [$A] = [$B] value }",
        "\"root\" { key [$A] value [$B] }",
        "\"root\" { key = [$A] value [$B] }",
    ] {
        let doc = text::parse(input).unwrap_or_else(|err| panic!("{input:?}: {err:?}"));
        assert_eq!(condition_of(&doc, "key"), Some("$B"), "{input:?}");
    }
}

#[test]
fn lossless_mode_keeps_pairs_a_conditional_would_replace() {
    let doc = text::parse("\"root\" { xpos = 1 xpos = [$WIN32] 2 }").unwrap();
    assert_eq!(pairs(&doc), ["xpos=1", "xpos=2"]);
    assert_eq!(body(&doc).entries[0].condition, None);
    assert_eq!(body(&doc).entries[1].condition.as_deref(), Some("$WIN32"));

    let doc = text::parse("\"root\" { a 1 a = [$WIN32] 2 [$X360] }").unwrap();
    assert_eq!(pairs(&doc), ["a=1", "a=2"]);
    assert_eq!(body(&doc).entries[0].condition, None);
    assert_eq!(body(&doc).entries[1].condition.as_deref(), Some("$X360"));
}

#[test]
fn conditions_survive_an_include() {
    let loader = |_: &str| -> Result<String, String> {
        Ok("\"base\"\n{\n\t\"from base\"\t\"yes\"\t[$WIN32]\n}\n".to_string())
    };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    let doc = text::parse_with("#base \"b.vdf\"\n\"root\"\n{\n\t\"from base\"\t\"no\"\n}\n", &options).unwrap();

    // The owner's pair has no condition; the base's pair lost the merge (the owner wins) and
    // so its condition went with it.
    assert_eq!(condition_of(&doc, "from base"), None);

    let doc = text::parse_with("#include \"b.vdf\"\n\"root\"\n{\n\t\"mine\"\t\"yes\"\n}\n", &options).unwrap();
    assert_eq!(pairs(&doc), ["mine=yes", "from base=yes"]);
    assert_eq!(condition_of(&doc, "from base"), Some("$WIN32"));
}

// ---------------------------------------------------------------------------------------
// Depth and hostile input
// ---------------------------------------------------------------------------------------

/// `depth` nested objects, as a document.
fn nested_document(depth: usize) -> String {
    let mut text = String::from("\"root\"\n{\n");
    for _ in 1..depth {
        text.push_str("\"k\"\n{\n");
    }
    for _ in 0..depth {
        text.push_str("}\n");
    }
    text
}

#[test]
fn nesting_at_the_cap_is_accepted() {
    let text = nested_document(text::MAX_DEPTH);
    let doc = text::parse(&text).unwrap_or_else(|err| panic!("{err:?}"));

    let mut depth = 0;
    let mut current = &doc.value;
    while let Value::Object(object) = current {
        depth += 1;
        match object.entries.first() {
            Some(kv) => current = &kv.value,
            None => break,
        }
    }
    assert_eq!(depth, text::MAX_DEPTH);
}

#[test]
fn nesting_past_the_cap_is_an_error() {
    let text = nested_document(text::MAX_DEPTH + 1);
    let err = text::parse(&text).unwrap_err();
    let (_, _, message) = syntax_error(err);
    assert!(message.contains("deeper than the supported maximum of 256"), "{message}");
}

#[test]
fn ten_thousand_levels_is_an_error_not_a_stack_overflow() {
    let text = nested_document(10_000);
    assert!(text::parse(&text).is_err());
}

#[test]
fn a_very_long_token_is_read_without_panicking() {
    let value = "x".repeat(1 << 20);
    let text = format!("\"root\" {{ \"key\" \"{value}\" }}");
    let doc = text::parse(&text).unwrap();
    assert_eq!(string(&doc, "key"), value);
}

#[test]
fn no_prefix_of_a_valid_document_panics() {
    for name in ["unquoted_document.vdf", "conditional.vdf", "numbers.vdf", "escaped_ending_quote.vdf"] {
        let input = fixture(name);
        // Every prefix that is a `&str` boundary, plus the whole file. Whatever parses has to
        // survive a write and a re-read as well.
        for (index, _) in input.char_indices() {
            if let Ok(doc) = text::parse(&input[..index]) {
                let _ = text::parse(&text::write(&doc));
            }
        }
        // `escaped_ending_quote.vdf` does not parse with default options (the reader test above
        // pins that), so the whole-file round trip is only asserted where there is a document.
        if let Ok(doc) = text::parse(&input) {
            assert_eq!(text::parse(&text::write(&doc)).unwrap(), doc);
        }
    }
}

#[test]
fn garbage_input_is_rejected_without_panicking() {
    let hostile = [
        "",
        " ",
        "\u{feff}",
        "{",
        "}",
        "[]",
        "[",
        "[$",
        "[$ABC",
        "\"",
        "\"\"\"",
        "=",
        "\"a\" =",
        "\"a\" {",
        "\"a\" { \"b\"",
        "\"a\" { \"b\" \"c\"",
        "\"a\" { \"b\" { } } \"c\" { }",
        "#base",
        "#include",
        "\"a\" { #include }",
        "\"a\" { \"b\" \"c\" [$] }",
        "\"a\" { \"b\" \"c\" [()] }",
        "\"a\" { \"b\" \"c\" [$A][$B] }",
        "\0",
        "\u{feff}\"a\"\u{feff}",
        "a",
        "0{0{",
        "= = = =",
        "\"a\"\n{\n\t\"b\"\t\"c\"\n}\n\n\n\n",
        "//",
        "/",
        "\"a\" { \"b\" \"c\" } extra",
    ];

    for input in hostile {
        // The only requirement is that this returns rather than panics; a few of these are
        // valid documents, most are not.
        let _ = text::parse(input);
    }

    // And the ones that must be errors are errors, not successes.
    for input in ["", " ", "{", "[", "\"a\" =", "#base", "= = = =", "\"a\" { \"b\" \"c\" } extra"] {
        assert!(text::parse(input).is_err(), "{input:?} must be rejected");
    }
}

#[test]
fn a_document_of_only_whitespace_is_an_error() {
    let err = text::parse("   \n\t ").unwrap_err();
    let (_, _, message) = syntax_error(err);
    assert_eq!(message, "Builder is not in a fully completed state (stack count is 0).");
}

// ---------------------------------------------------------------------------------------
// Object helpers used by consumers
// ---------------------------------------------------------------------------------------

#[test]
fn the_data_model_keeps_order_and_duplicates() {
    let doc = text::parse("\"root\"\n{\n\t\"b\"\t\"1\"\n\t\"a\"\t\"2\"\n\t\"b\"\t\"3\"\n}\n").unwrap();
    let root = body(&doc);

    assert_eq!(root.entries.iter().map(|kv| kv.key.as_str()).collect::<Vec<_>>(), ["b", "a", "b"]);
    assert_eq!(root.get("b").and_then(Value::as_str), Some("1"));
    assert_eq!(root.get_all("b").filter_map(Value::as_str).collect::<Vec<_>>(), ["1", "3"]);
    assert_eq!(root.get("B").and_then(Value::as_str), Some("1"), "lookup is case-insensitive");

    let mut copy = root.clone();
    copy.push("c", Value::String("4".into()));
    assert_eq!(copy.entries.len(), 4);
    assert_eq!(copy.entries[3], KeyValue { key: "c".into(), value: Value::String("4".into()), condition: None });
}

// ---------------------------------------------------------------------------------------
// Round 3 review fixes

/// Parses `main` with every `#base`/`#include` answered by `included`.
fn parse_with_included(main: &str, included: &'static str) -> Result<Document, Error> {
    let loader = move |_: &str| -> Result<String, String> { Ok(included.to_string()) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    text::parse_with(main, &options)
}

#[test]
fn base_merging_compares_keys_case_sensitively() {
    // Upstream's merge is `FindIndex(i => i.Key == item.Key)` / an ordinal `==` in the nested
    // walk. The tree's own lookups ignore case, so a merge that did too would fold `key` into
    // `Key` and the base's value would vanish.
    let doc = parse_with_included(
        "#base \"b\"\n\"root\" { \"Key\" \"1\" }\n",
        "\"r\" { \"key\" \"2\" }\n",
    )
    .unwrap();
    assert_eq!(pairs(&doc), ["Key=1", "key=2"]);

    // The same one level down: `Obj` and `obj` are two objects, not one with `a` and `b`.
    let doc = parse_with_included(
        "#base \"b\"\n\"root\" { \"Obj\" { a 1 } }\n",
        "\"r\" { \"obj\" { b 2 } }\n",
    )
    .unwrap();
    let keys: Vec<_> = body(&doc).entries.iter().map(|kv| kv.key.as_str()).collect();
    assert_eq!(keys, ["Obj", "obj"]);
    let first = body(&doc).entries[0].value.as_object().unwrap();
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.entries[0].key, "a");

    // And the same-case control: an identical key does merge (owner wins).
    let doc = parse_with_included(
        "#base \"b\"\n\"root\" { \"Key\" \"1\" }\n",
        "\"r\" { \"Key\" \"2\" }\n",
    )
    .unwrap();
    assert_eq!(pairs(&doc), ["Key=1"]);
}

#[test]
fn nested_objects_with_different_case_do_not_merge() {
    // Needs a two-level difference: the outer key matches exactly, the inner one differs by case.
    let doc = parse_with_included(
        "#base \"b\"\n\"root\" { \"O\" { \"K\" 1 } }\n",
        "\"r\" { \"O\" { \"k\" 2 } }\n",
    )
    .unwrap();
    let o = value(&doc, "O").as_object().unwrap();
    let inner: Vec<_> = o.entries.iter().map(|kv| format!("{}={}", kv.key, kv.value.as_str().unwrap())).collect();
    assert_eq!(inner, ["K=1", "k=2"]);
}

#[test]
fn null_byte_bug_leaves_a_raw_nul_in_an_unquoted_token_and_the_round_trip_cuts_it() {
    // The bug-compatible cut applies to quoted strings only, so this value keeps its NUL ...
    let options = ReadOptions { valve_null_byte_bug: true, ..Default::default() };
    let doc = text::parse_with("r { k a\0b }", &options).unwrap();
    assert_eq!(string(&doc, "k"), "a\0b");

    // ... the writer quotes it, and reading the quoted form cuts at the NUL: the documented
    // lossy case (default options round-trip everything that parses).
    let again = text::parse_with(&text::write(&doc), &options).unwrap();
    assert_eq!(string(&again, "k"), "a");

    let plain = text::parse("r { k ab }").unwrap();
    assert_eq!(text::parse(&text::write(&plain)).unwrap(), plain);
}

#[test]
fn a_combining_mark_ends_a_condition_variable_name() {
    // U+093E is a Devanagari vowel sign (category Mc): `char.IsLetterOrDigit` rejects it, so
    // upstream reads `$a` and then fails on the mark. `char::is_alphanumeric` would have taken it
    // into the name and silently matched a symbol called "a\u{93e}".
    let input = "\"root\" { key value [$a\u{93e}] }";
    for defined in [&["a\u{93e}"][..], &["a"][..], &[][..]] {
        let options = ReadOptions {
            conditions: Some(defined.iter().map(|c| c.to_string()).collect()),
            ..Default::default()
        };
        let (_, _, message) = syntax_error(text::parse_with(input, &options).unwrap_err());
        assert!(message.contains("Unexpected '\u{93e}' after end of expression."), "{message}");
    }

    // The ASCII control still names a symbol.
    let options = ReadOptions { conditions: Some(vec!["a_1".to_string()]), ..Default::default() };
    assert!(text::parse_with("\"root\" { key value [$a_1] }", &options).is_ok());
}

#[test]
fn a_zero_run_condition_is_false() {
    let options = ReadOptions { conditions: Some(vec![]), ..Default::default() };
    let kept = |input: &str| body(&text::parse_with(input, &options).unwrap()).get("key").is_some();
    assert!(!kept("\"root\" { key value [00] }"));
    assert!(kept("\"root\" { key value [10] }"));
}

#[test]
fn a_missing_loader_is_reported_before_a_missing_root_object() {
    // Upstream loads a document's inclusions before `GetObject` counts the builder's stack.
    assert_eq!(
        text::parse("#base \"x.vdf\"\n").unwrap_err(),
        Error::Include {
            path: "x.vdf".to_string(),
            message: "inclusions require ReadOptions::include_loader to be set".to_string(),
        }
    );

    // A failing loader likewise wins over the count ...
    let loader = |path: &str| -> Result<String, String> { Err(format!("no such file: {path}")) };
    let options = ReadOptions { include_loader: Some(&loader), ..Default::default() };
    assert_eq!(
        text::parse_with("#include \"x.vdf\"\n", &options).unwrap_err(),
        Error::Include { path: "x.vdf".to_string(), message: "no such file: x.vdf".to_string() }
    );

    // ... and with nothing to include the count error is still what you get.
    let (_, _, message) = syntax_error(text::parse("").unwrap_err());
    assert_eq!(message, "Builder is not in a fully completed state (stack count is 0).");
}
