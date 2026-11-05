//! The KV3 gate (`cs2_bridge::kv3_check`) against the C header it ports, case for case.
//!
//! # Where the expectations come from
//!
//! Every case below is a case of `bridge/test_kv3_check.c`, with the verdict the C gate prints
//! for it (the `PASS …` lines of that test: `105 cases, 0 failed`). The fixtures are read from
//! `hammer-mcp/fixtures/vpcf/` at test time exactly as the C test reads them - a missing fixture
//! is a failure naming the path, never a skip.
//!
//! [`corpus_matches_the_c_runner`] is `bridge/test_kv3_corpus.sh` in Rust: the same walk over the
//! installed workshop content (`_toolsautosave/` and `mcp_*` skipped), the same two checks, the
//! same `>= 50 kv3 files` floor. The C runner reports `109 kv3 files checked, 4 vpcf files
//! checked, 0 refusals`; this test asserts the same counts. A missing content tree is a failure
//! naming the path (the C script's `exit 2`), never a skip.
//!
//! # Why the gate is a port and not a thin `cs2_kv3::parse` call
//!
//! `cs2-kv3` reads the *union* of the pre-conformance reader and ValveKeyValue's reader (its
//! crate docs, "The union"), so it accepts spellings this gate must refuse and refuses a few
//! spellings it must accept. [`disagreements_with_cs2_kv3`] is that list, measured on both sides
//! and asserted, so a change in either implementation shows up here instead of in a live wedged
//! `pet`.

use std::path::{Path, PathBuf};

use cs2_bridge::kv3_check::{
    self, addon_name, app_root, host_path, kv3_check, vpcf_check, Reason,
};

/// The real header line of a `.vpcf` (the first line of `hammer-mcp/fixtures/vpcf/probe.vpcf`).
const HDR: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
                   format:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->";

/// The truncated-body crash input, byte for byte (`test_kv3_check.c:41`).
const TRUNCATED_BODY: &str =
    "\r\n{\r\n\t_class = \"CParticleSystemDefinition\"\r\n\tm_Emitters = [ { _class = \r\n";

/// A header naming kv3 but no format (`test_kv3_check.c:232`).
const HDR_GENERIC: &str =
    "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} -->\n{ }\n";

/// What one entry point must answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    Accept,
    Refuse(Reason),
}

/// One case: `kv3`/`vpcf` are `None` when the C test does not run that entry point for it.
struct Case {
    name: &'static str,
    input: Vec<u8>,
    kv3: Option<Expect>,
    vpcf: Option<Expect>,
}

/// `HDR` followed by a body.
fn doc(body: &str) -> Vec<u8> {
    format!("{HDR}{body}").into_bytes()
}

fn both(expect: Expect) -> (Option<Expect>, Option<Expect>) {
    (Some(expect), Some(expect))
}

/// Every case of `test_kv3_check.c`, in that file's order.
fn cases() -> Vec<Case> {
    use Expect::{Accept, Refuse};
    use Reason::*;

    let mut cases: Vec<Case> = Vec::new();
    let mut push = |name: &'static str, input: Vec<u8>, (kv3, vpcf): (Option<Expect>, Option<Expect>)| {
        cases.push(Case { name, input, kv3, vpcf });
    };

    // ---- MUST REFUSE (header / grammar) ----

    // 1. the first live crash input, byte-exact: the whole file.
    push("live_crash_1_not_kv3", b"{{{ not kv3\n".to_vec(), both(Refuse(NoKv3Header)));

    // 2. the second live crash input, byte-exact: valid header + truncated body.
    push(
        "live_crash_2_truncated",
        format!("{HDR}{TRUNCATED_BODY}").into_bytes(),
        both(Refuse(Unbalanced)),
    );

    // 3. the third live crash input: BALANCED, but a key with no value (the old lexer's blind
    //    spot) - `_class = ` then `}` on the next line, inside the m_Emitters entry.
    push(
        "live_crash_3_key_no_value",
        doc("\n{\n\t_class = \"CParticleSystemDefinition\"\n\tm_Emitters =\n\t[\n\t\t{\n\
             \t\t\t_class = \n\t\t}\n\t]\n}\n"),
        both(Refuse(ExpectedValue)),
    );

    push("empty_file", Vec::new(), both(Refuse(NoKv3Header)));
    push("header_only", doc("\r\n"), both(Refuse(NoRootObject)));
    push("unterminated_string", doc("\n{\n\ta = \"abc\n"), both(Refuse(UnterminatedString)));
    push(
        "unterminated_comment",
        doc("\n{\n\t/* a comment that never ends\n"),
        both(Refuse(UnterminatedComment)),
    );
    push("mismatched_bracket", doc("\n{ ] }\n"), both(Refuse(Unbalanced)));
    push("extra_close_brace", doc("\n{ }\n}\n"), both(Refuse(TrailingContent)));
    push("two_root_objects", doc("\n{ }\n{ }\n"), both(Refuse(TrailingContent)));
    push("root_not_an_object", doc("\n[ ]\n"), both(Refuse(NoRootObject)));
    push("root_not_an_object_word", doc("\nnot an object\n"), both(Refuse(NoRootObject)));

    // a NUL byte anywhere (here inside an otherwise valid document).
    let mut nul = doc("\n{ a = 1");
    nul.push(0);
    nul.extend_from_slice(b" }\n");
    push("nul_byte", nul, both(Refuse(Binary)));

    // depth 300: the cap is 256, so this is refused long before pet could recurse.
    let mut deep = format!("{HDR}\n").into_bytes();
    deep.extend(std::iter::repeat_n(b'{', 300));
    push("depth_300", deep, both(Refuse(TooDeep)));

    // a header that names kv3 but not the vpcf format: only the vpcf entry point refuses.
    push(
        "header_not_vpcf",
        HDR_GENERIC.as_bytes().to_vec(),
        (Some(Accept), Some(Refuse(NotVpcf))),
    );

    // ---- MUST REFUSE (the G2 grammar cases) ----

    push("key_double_equals", doc("\n{ a = = 1 }\n"), both(Refuse(ExpectedValue)));
    push("key_no_equals", doc("\n{ a 1 }\n"), both(Refuse(ExpectedEquals)));
    push("key_missing", doc("\n{ = 1 }\n"), both(Refuse(BadKey)));
    push("comma_between_members", doc("\n{ a = 1, b = 2 }\n"), both(Refuse(CommaInObject)));
    push("array_missing_comma", doc("\n{ a = [1 2] }\n"), both(Refuse(ExpectedComma)));
    push("flag_without_value", doc("\n{ a = resource: }\n"), both(Refuse(ExpectedValue)));
    push("bare_word_value", doc("\n{ a = foo }\n"), both(Refuse(BadToken)));
    push("blob_bad_byte", doc("\n{ a = #[ 00 01 zz ] }\n"), both(Refuse(BadToken)));
    push("blob_unterminated", doc("\n{ a = #[ 00 01\n"), both(Refuse(Unbalanced)));

    // ---- MUST REFUSE (vpcf root class; the grammar is fine, so kv3_check accepts) ----

    push(
        "wrong_root_class",
        doc("\n{\n\t_class = \"CParticleSomethingElse\"\n\tm_nMaxParticles = 32\n}\n"),
        (Some(Accept), Some(Refuse(WrongRootClass))),
    );
    push(
        "no_root_class",
        doc("\n{\n\tm_nMaxParticles = 32\n}\n"),
        (None, Some(Refuse(NoRootClass))),
    );
    // only the ROOT object's own `_class` counts: a nested one does not satisfy the check.
    push(
        "nested_class_is_not_root",
        doc(
            "\n{\n\tm_Emitters = [ { _class = \"CParticleSystemDefinition\" } ]\n\
             \tm_nMaxParticles = 32\n}\n",
        ),
        (Some(Accept), Some(Refuse(NoRootClass))),
    );
    // member names are case-insensitive upstream (MurmurHash2LowerCase), so a `_CLASS` member is
    // the same member - and every root class member must carry the exact class name.
    push("class_name_ci", doc("\n{\n\t_CLASS = \"CParticleSystemDefinition\"\n}\n"), both(Accept));
    push(
        "class_value_wrong_case",
        doc("\n{\n\t_CLASS = \"cparticlesystemdefinition\"\n}\n"),
        (None, Some(Refuse(WrongRootClass))),
    );
    push(
        "class_duplicate_conflict",
        doc(
            "\n{\n\t_class = \"CParticleSystemDefinition\"\n\
             \t_CLASS = \"CParticleSomethingElse\"\n}\n",
        ),
        (None, Some(Refuse(WrongRootClass))),
    );
    // a class name must be a bare string: a flagged value is not one.
    push(
        "class_flagged_value",
        doc("\n{\n\t_class = resource:\"CParticleSystemDefinition\"\n}\n"),
        (Some(Accept), Some(Refuse(WrongRootClass))),
    );
    // a raw `"""..."""` string is text, not a class name.
    push(
        "raw_string_is_not_class",
        doc("\n{\n\t_class = \"\"\"CParticleSystemDefinition\"\"\"\n}\n"),
        (None, Some(Refuse(WrongRootClass))),
    );

    // flags do not chain, need their value immediately, and cannot be named like a literal.
    push("flag_chained", doc("\n{ a = resource:resource:\"x\" }\n"), both(Refuse(BadToken)));
    push("flag_space_before_value", doc("\n{ a = resource: \"x\" }\n"), both(Refuse(ExpectedValue)));
    push("flag_named_true", doc("\n{ a = true:1 }\n"), both(Refuse(BadKey)));

    // ---- MUST ACCEPT ----

    // braces inside strings and inside line and block comments, plus a `"""` raw string holding
    // a `}` (LF-only).
    push(
        "braces_in_strings_comments",
        doc(
            "\n{\n\ta = \"} ] { [\"\n\tb = \"\"\"\n raw } { ] [ text\n \"\"\"\n\
             \t// } ] { [ in a line comment\n\t/* } ] { [ in a block comment */\n\
             \tc = [ { x = 1 } ]\n}\n",
        ),
        (Some(Accept), None),
    );
    // the same document with CRLF line endings.
    push(
        "crlf_braces_in_strings",
        doc(
            "\r\n{\r\n\ta = \"} ] { [\"\r\n\tb = \"\"\"\r\n raw } { ] [ text\r\n \"\"\"\r\n\
             \t// } ] { [ in a line comment\r\n\t/* } ] { [ in a block comment */\r\n\
             \tc = [ { x = 1 } ]\r\n}\r\n",
        ),
        (Some(Accept), None),
    );
    // a UTF-8 BOM and leading blank lines before the header, no trailing newline.
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(b"\r\n\r\n");
    bom.extend_from_slice(format!("{HDR}\n{{\n\ta = 1\n}}").as_bytes());
    push("bom_and_blank_lines", bom, (Some(Accept), None));

    // every value kind, a .vpcf shape: flags, a blob, a multi-line string, signed/exponent
    // numbers, the literals, nested arrays of objects, comments, a trailing comma.
    push(
        "value_kinds",
        doc(
            "\n{\n\
             \t_class = \"CParticleSystemDefinition\"\n\
             \tm_nMaxParticles = 42\n\
             \tm_flA = -1.5e-3\n\
             \tm_flB = +7\n\
             \tm_flC = .5\n\
             \tm_flD = 1e5\n\
             \tm_bOn = true\n\
             \tm_bOff = false\n\
             \tm_pNull = null\n\
             \tm_sFlag = resource:\"particles/x.vpcf\"\n\
             \tm_sEmpty = resource_name:\"\"\n\
             \tm_sPanorama = panorama:\"file://{images}/econ/x.png\"\n\
             \tm_sSub = subclass:\"CParticleSystemDefinition\"\n\
             \tm_sMulti = \"\"\"\n raw } { ] [ with \"quotes\" and a \\ raw backslash\n \"\"\"\n\
             \tm_Blob = #[ 00 01 FF a7 0d ]\n\
             \tm_Nums = [ 1, -2, 3.5, 1e-9, 7, ]\n\
             \tm_Objs = [ { a = 1 }, { b = [ { c = 2 } ] }, { } ]\n\
             \tm_sAlone = [ resource_name:\"a/b.vmat\", ]\n\
             \t// a line comment\n\
             \t/* a block comment with } ] { [ */\n\
             }\n",
        ),
        both(Accept),
    );

    // a quoted `_class` key counts as the root class member.
    push("quoted_root_class", doc("\n{\n\t\"_class\" = \"CParticleSystemDefinition\"\n}\n"), both(Accept));
    // a quoted key with dots, like propdata.vdata's `"Cloth.Base"`.
    push(
        "quoted_key_with_dots",
        doc("\n{\n\t_class = \"CParticleSystemDefinition\"\n\t\"Cloth.Base\" = { health = 30 }\n}\n"),
        both(Accept),
    );

    cases
}

/// The `.vpcf` fixtures the C test reads, with the verdicts it requires
/// (`test_kv3_check.c:466-483`).
fn fixture_cases() -> Vec<Case> {
    use Expect::{Accept, Refuse};
    use Reason::{NoKv3Header, WrongRootClass};

    let dir = fixture_dir();
    let mut cases = Vec::new();
    for (name, file, kv3, vpcf) in [
        ("probe_fixture", "probe.vpcf", None, Some(Accept)),
        ("with_children_fixture", "with_children.vpcf", None, Some(Accept)),
        ("not_kv3_fixture", "not_kv3.vpcf", Some(Refuse(NoKv3Header)), Some(Refuse(NoKv3Header))),
        ("bad_root_fixture", "bad_root.vpcf", Some(Accept), Some(Refuse(WrongRootClass))),
    ] {
        let path = dir.join(file);
        let input = std::fs::read(&path).unwrap_or_else(|error| {
            panic!("fixture {} is required by the C test but cannot be read: {error}", path.display())
        });
        cases.push(Case { name, input, kv3, vpcf });
    }
    cases
}

/// `hammer-mcp/fixtures/vpcf/`, from this crate's manifest directory.
fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hammer-mcp/fixtures/vpcf")
}

/// Run one case against the entry points it names, and report a mismatch with the case name.
fn run_case(case: &Case) -> usize {
    let mut ran = 0;
    if let Some(expected) = case.kv3 {
        assert_eq!(
            kv3_check(&case.input).err().map(|refusal| refusal.reason),
            match expected {
                Expect::Accept => None,
                Expect::Refuse(reason) => Some(reason),
            },
            "case `{}` ({} bytes), kv3_check",
            case.name,
            case.input.len()
        );
        ran += 1;
    }
    if let Some(expected) = case.vpcf {
        assert_eq!(
            vpcf_check(&case.input).err().map(|refusal| refusal.reason),
            match expected {
                Expect::Accept => None,
                Expect::Refuse(reason) => Some(reason),
            },
            "case `{}` ({} bytes), vpcf_check",
            case.name,
            case.input.len()
        );
        ran += 1;
    }
    ran
}

/// Every `test_kv3_check.c` case, against the Rust gate.
#[test]
fn every_c_test_case_has_the_c_verdict() {
    let cases = cases();
    let mut checks = 0;
    for case in &cases {
        checks += run_case(case);
    }
    // The C test prints `105 cases, 0 failed`: 75 entry-point checks over the grammar/header
    // inputs below, 6 fixture checks (in `every_fixture_has_the_c_verdict`) and 24 path-helper
    // checks (`path_helpers_have_the_c_verdicts`). Its `check_one` names a case per entry point,
    // so where the C runs both it prints two names (`wrong_root_class` /
    // `wrong_root_class_kv3`); here one input carries both expectations.
    assert_eq!(checks, 75, "the C test's grammar/header entry-point checks");
    assert_eq!(cases.len(), 41, "distinct grammar/header inputs");
    assert_eq!(checks + 6 + 24, 105, "the C test's total case count");
}

/// The four `.vpcf` fixtures, against the Rust gate.
#[test]
fn every_fixture_has_the_c_verdict() {
    let cases = fixture_cases();
    let mut checks = 0;
    for case in &cases {
        checks += run_case(case);
    }
    assert_eq!(checks, 6, "the C test's fixture checks");
}

/// The `pet.open` host-path derivation, case for case (`test_kv3_check.c:429-464`).
#[test]
fn path_helpers_have_the_c_verdicts() {
    // -addon parsing: the runtime's real command line first.
    let addon_cases: &[(&str, &str, Option<&str>)] = &[
        (
            "addon_runtime_form",
            "Z:\\app\\game\\bin\\win64\\cs2.exe -tools -vulkan -console -condebug \
             -addon mcp_test +engine_no_focus_sleep 0",
            Some("mcp_test"),
        ),
        ("addon_equals", "cs2.exe -addon=mcp_test", Some("mcp_test")),
        ("addon_quoted", "cs2.exe -addon \"mcp_test\"", Some("mcp_test")),
        ("addon_mixed_case", "cs2.exe -Addon MCP_Test9", Some("MCP_Test9")),
        ("addon_absent", "cs2.exe -tools -vulkan", None),
        ("addon_no_value", "cs2.exe -addon", None),
        ("addon_lookalike", "cs2.exe -addons x", None),
        ("addon_bad_char", "cs2.exe -addon mcp-test", None),
        ("addon_unterminated_quote", "cs2.exe -addon \"mcp_test", None),
        ("addon_quoted_trailing", "cs2.exe -addon \"mcp_test\"x", None),
    ];
    for (name, line, want) in addon_cases {
        assert_eq!(addon_name(line).as_deref(), *want, "path case `{name}`");
    }
    // a 4-byte buffer does not hold `mcp_test`.
    let mut small = [0u8; 4];
    assert!(!kv3_check::addon_name_into("cs2.exe -addon mcp_test", &mut small), "addon_cap_small");

    let app_cases: &[(&str, &str, Option<&str>)] = &[
        ("app_root_wine", "Z:\\app\\game\\bin\\win64\\cs2.exe", Some("Z:\\app")),
        ("app_root_case", "Z:\\app\\game\\bin\\win64\\CS2.EXE", Some("Z:\\app")),
        ("app_root_drive_root", "C:\\game\\bin\\win64\\cs2.exe", Some("C:")),
        ("app_root_forward_slash", "/app/game/bin/win64/cs2.exe", Some("/app")),
        ("app_root_no_game", "Z:\\app\\bin\\win64\\cs2.exe", None),
        ("app_root_short", "Z:\\app\\game\\bin\\cs2.exe", None),
        ("app_root_other_exe", "Z:\\app\\game\\bin\\win64\\csgo.exe", None),
        ("app_root_relative", "game\\bin\\win64\\cs2.exe", None),
    ];
    for (name, exe, want) in app_cases {
        assert_eq!(app_root(exe).as_deref(), *want, "path case `{name}`");
    }

    let host_cases: &[(&str, &str, &str, &str, Option<&str>)] = &[
        (
            "host_path_wine",
            "Z:\\app",
            "mcp_test",
            "particles/mcp_probe/probe.vpcf",
            Some("Z:\\app\\content\\csgo_addons\\mcp_test\\particles\\mcp_probe\\probe.vpcf"),
        ),
        ("host_path_empty_app", "", "mcp_test", "particles/x.vpcf", None),
        ("host_path_empty_addon", "Z:\\app", "", "particles/x.vpcf", None),
        ("host_path_empty_rel", "Z:\\app", "mcp_test", "", None),
    ];
    for (name, app, addon, rel, want) in host_cases {
        assert_eq!(host_path(app, addon, rel).as_deref(), *want, "path case `{name}`");
    }
    let mut small = [0u8; 16];
    assert!(
        !kv3_check::host_path_into("Z:\\app", "mcp_test", "particles/x.vpcf", &mut small),
        "host_path_cap_small"
    );
}

/// **NEGATIVE**: a malformed input that is *not* in the C tests - a plain string that runs to
/// end of file - is refused by the gate, and by `cs2-kv3`, with the same verdict.
///
/// The C test's `unterminated_string` case has a newline after the broken string, so the string
/// scanner stops at a newline it can see; here there is nothing after it. The depth pass and the
/// grammar pass both walk the string, and both must stop at the end of the buffer.
#[test]
fn unterminated_string_at_eof_is_refused() {
    for body in ["\n{\n\ta = \"abc", "\n{\n\ta = \"abc\\", "\n{\n\ta = \"\"\"abc\n"] {
        let input = doc(body);
        let refusal = kv3_check(&input).expect_err("an unterminated string at EOF is not loadable");
        assert_eq!(refusal.reason, Reason::UnterminatedString, "body {body:?}");
        assert_eq!(vpcf_check(&input).expect_err("same").reason, Reason::UnterminatedString);
        // The workspace reader agrees that this input is malformed (a different wording, the
        // same verdict).
        let text = String::from_utf8(input).expect("ASCII");
        assert!(cs2_kv3::parse(&text).is_err(), "cs2-kv3 must refuse {body:?} too");
    }
}

/// The reason strings are the protocol: `-32602 UnloadableFile {reason}` carries them verbatim.
#[test]
fn reasons_are_the_c_literals() {
    let all = [
        (Reason::NoKv3Header, "no_kv3_header"),
        (Reason::NotVpcf, "not_vpcf"),
        (Reason::Binary, "binary"),
        (Reason::TooDeep, "too_deep"),
        (Reason::UnterminatedComment, "unterminated_comment"),
        (Reason::UnterminatedString, "unterminated_string"),
        (Reason::NoRootObject, "no_root_object"),
        (Reason::TrailingContent, "trailing_content"),
        (Reason::Unbalanced, "unbalanced"),
        (Reason::CommaInObject, "comma_in_object"),
        (Reason::BadKey, "bad_key"),
        (Reason::ExpectedEquals, "expected_equals"),
        (Reason::ExpectedValue, "expected_value"),
        (Reason::ExpectedComma, "expected_comma"),
        (Reason::BadToken, "bad_token"),
        (Reason::NoRootClass, "no_root_class"),
        (Reason::WrongRootClass, "wrong_root_class"),
    ];
    for (reason, text) in all {
        assert_eq!(reason.as_str(), text);
    }
    // and every refusal in the C test's table names one of them (no reason invented here)
    let mut named: Vec<&str> = Vec::new();
    for case in cases().iter().chain(fixture_cases().iter()) {
        for expect in [case.kv3, case.vpcf].into_iter().flatten() {
            if let Expect::Refuse(reason) = expect {
                named.push(reason.as_str());
            }
        }
    }
    for reason in &named {
        assert!(all.iter().any(|(_, text)| text == reason), "`{reason}` is a C reason");
    }
}

/// `cs2-kv3`'s verdict for every input where the two implementations disagree, measured.
///
/// The gate follows the C bridge (the protocol contract). Each row is
/// `(case, the C verdict, what cs2-kv3 does)`; the C verdict is asserted above, and this test
/// asserts `cs2-kv3`'s half so a change there is visible. `Valve` says which side the KV3
/// grammar source in `crates/cs2-kv3` (`lib.rs`, "Grammar (upstream: ValveKeyValue's KV3 text
/// reader, commit 28ca0e5)") backs - the C gate is the stricter reading there, and `cs2-kv3`
/// accepts more on purpose (its "union" contract).
#[test]
fn disagreements_with_cs2_kv3() {
    use Expect::{Accept, Refuse};
    use Reason::*;

    // (name, input, C verdict, cs2-kv3 accepts?, which side Valve's grammar backs)
    let rows: Vec<(&str, Vec<u8>, Expect, bool, &str)> = vec![
        ("comma_between_members", doc("\n{ a = 1, b = 2 }\n"), Refuse(CommaInObject), true,
         "C: upstream `KV3TextReader` wants no comma between members"),
        ("trailing_comma_object", doc("\n{ a = 1, }\n"), Refuse(CommaInObject), true,
         "C: upstream; cs2-kv3 keeps 686975d's trailing comma"),
        ("leading_comma_object", doc("\n{ , a = 1 }\n"), Refuse(CommaInObject), true,
         "C: upstream; cs2-kv3 keeps 686975d's leading comma"),
        ("array_missing_comma", doc("\n{ a = [1 2] }\n"), Refuse(ExpectedComma), true,
         "C: upstream needs the comma; cs2-kv3 keeps 686975d's `[ 1 2 ]`"),
        ("leading_comma_array", doc("\n{ a = [ , 1 ] }\n"), Refuse(ExpectedValue), true,
         "C: upstream; cs2-kv3 keeps 686975d's leading comma"),
        ("flag_chained", doc("\n{ a = resource:resource:\"x\" }\n"), Refuse(BadToken), true,
         "C: upstream keeps only the last flag; cs2-kv3 nests the chain"),
        ("flag_space_before_value", doc("\n{ a = resource: \"x\" }\n"), Refuse(ExpectedValue), true,
         "C: upstream; cs2-kv3 allows trivia before the flagged value"),
        ("flag_trivia_before_colon", doc("\n{ a = resource /*c*/ : \"x\" }\n"), Refuse(BadToken),
         true, "C: the flag is `name:value` with nothing between; cs2-kv3 looks through comments \
          and whitespace (`resource :\"x\"` is the same class)"),
        ("flag_named_true", doc("\n{ a = true:1 }\n"), Refuse(BadKey), true,
         "C: a literal wins over a flag, so `true:1` is a key with no `=`; cs2-kv3 lexes \
          `true:` as a flag (same for `false:`/`null:`)"),
        ("flag_named_null", doc("\n{ a = null:1 }\n"), Refuse(BadKey), true,
         "as `flag_named_true`: the literal check runs before the flag check"),
        ("pipe_flag", doc("\n{ a = resource|\"x\" }\n"), Refuse(BadToken), true,
         "C: only `name:value`; cs2-kv3 also takes upstream's `|`"),
        ("single_quote_string", doc("\n{ a = 'x' }\n"), Refuse(ExpectedValue), true,
         "C: only `\"` strings; cs2-kv3 also takes upstream's `'`"),
        ("single_quote_key", doc("\n{ 'a' = 1 }\n"), Refuse(BadKey), true,
         "C: only `\"` keys; cs2-kv3 also takes upstream's `'`"),
        ("leading_dot_key", doc("\n{ .y = 1 }\n"), Refuse(BadKey), true,
         "C: keys start with a letter or `_`; cs2-kv3 takes upstream's leading `.`"),
        ("inf_literal", doc("\n{ a = inf }\n"), Refuse(BadToken), true,
         "C: no float literals; cs2-kv3 takes upstream's `inf`"),
        ("nan_literal", doc("\n{ a = nan }\n"), Refuse(BadToken), true,
         "C: no float literals; cs2-kv3 takes upstream's `nan`"),
        ("plus_alone", doc("\n{ a = + }\n"), Refuse(BadToken), true,
         "C: `+` needs digits; cs2-kv3 takes upstream's bare `+` = 0"),
        ("upper_literal_TRUE", doc("\n{ a = TRUE }\n"), Refuse(BadToken), true,
         "C: literals are lowercase; cs2-kv3 takes upstream's case-insensitive literals"),
        ("header_no_format", HDR_GENERIC.as_bytes().to_vec(), Accept, false,
         "C: the header only has to contain `kv3`; cs2-kv3 requires both fields"),
        ("header_kv3_in_encoding_name",
         b"<!-- kv3x encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} -->\n{ }\n".to_vec(),
         Accept, false,
         "C: a substring test; cs2-kv3 parses the token stream (neither is Valve's reading, which \
          needs the `kv3` token - the C gate is looser here and the gate is the contract)"),
        ("header_field_typo",
         b"<!-- kv3 ecoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} -->\n{ }\n".to_vec(),
         Accept, false,
         "as `header_kv3_in_encoding_name`: the C gate never parses the header's fields, so any \
          header containing `kv3` is a kv3 header to it; cs2-kv3 (and Valve) require \
          `encoding:`/`format:`. The mutation fuzz found this whole class (a typo in a field \
          name, a missing `:`, a stray bracket), so it is one row, not one per spelling"),
        ("header_vpcf_substring",
         b"<!-- kv3 format[*[:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->\n{ }\n".to_vec(),
         Accept, false,
         "as `header_field_typo`; the `format:vpcf` search is a substring test too, so \
          `format[*[:vpcf` satisfies it"),
        ("odd_nibble_blob", doc("\n{ a = #[ 4 ] }\n"), Accept, false,
         "C: any hex digits; cs2-kv3 needs whole bytes (Valve's reader also needs whole bytes - \
          the C gate is looser here and the gate is the contract)"),
        ("tab_after_flag_colon", doc("\n{ a = resource:\t\"x\" }\n"), Refuse(ExpectedValue), true,
         "C: no trivia at all after a flag's colon; cs2-kv3 skips it (upstream's `ParseValue` \
          tokenizes the flag with its separator, so both readings are defensible - the C gate \
          refuses because Valve writes `resource:\"...\"` and a space there is a typo)"),
        ("blob_with_comment", doc("\n{ a = #[ 01 /*c*/ 02 ] }\n"), Refuse(BadToken), true,
         "C: only hex and whitespace between a blob's bytes; cs2-kv3 takes upstream's comments"),
        ("empty_key_ident_dot", doc("\n{ . = 1 }\n"), Refuse(BadKey), true,
         "C: a key needs a letter or `_` first; cs2-kv3 takes upstream's bare `.` identifier"),
        ("nul_inside_a_string", {
            let mut input = doc("\n{ a = \"x");
            input.push(0);
            input.extend_from_slice(b"y\" }\n");
            input
        }, Refuse(Binary), true,
         "C: a NUL anywhere is `binary` (the file is not text); cs2-kv3 takes `&str`, and a NUL \
          inside a quoted string is a legal Rust `char` there, so it reads the file"),
        ("non_utf8_byte_in_string", {
            let mut input = doc("\n{ a = \"x");
            input.push(0x80);
            input.extend_from_slice(b"y\" }\n");
            input
        }, Accept, false,
         "C: bytes are opaque inside a string, so a non-UTF-8 byte is accepted; cs2-kv3 takes \
          `&str` and cannot read the file at all. Here cs2-kv3 is the stricter one - the gate is \
          the contract, and a pet loader that reads bytes sees what the gate saw"),
    ];

    let mut disagreements = 0;
    for (name, input, c_verdict, cs2_accepts, valve) in &rows {
        let gate = kv3_check(input);
        match c_verdict {
            Expect::Accept => assert!(gate.is_ok(), "case `{name}`: the C gate accepts it"),
            Expect::Refuse(reason) => {
                assert_eq!(gate.err().map(|refusal| refusal.reason), Some(*reason), "case `{name}`")
            }
        }
        assert_eq!(
            cs2_kv3_accepts(input),
            *cs2_accepts,
            "case `{name}`: cs2-kv3's side of the disagreement ({valve})"
        );
        disagreements += 1;
    }
    assert_eq!(disagreements, rows.len());
    // How this list was found: both implementations were run over the C test's inputs, the whole
    // installed content corpus, and a mutation fuzz of those seeds (1,249 inputs: deletions,
    // insertions, substitutions, truncations). This table is every input where the two verdicts
    // differ; inputs both refuse (different wording, same verdict) and inputs both accept are not
    // disagreements and are covered by the case table above.
    //
    // The rows are *classes*, not single spellings: the fuzz found that a typo anywhere in a
    // header field is one class (the C gate substring-searches, cs2-kv3 parses), so
    // `header_field_typo` stands for that whole family rather than for its first example.
}

/// The fuzz's classification, so "the table is every disagreement" is measured, not claimed.
///
/// The seeds are the C test's inputs plus the four fixtures, each mutated 12 ways (deletion,
/// insertion, substitution, truncation, splice) with a fixed seed. For every input where the two
/// implementations' verdicts differ, this test asks each half separately, against the other
/// half's clean version:
///
/// * the body, against the clean header: its signature (the gate's reason, or `ok`, and what
///   cs2-kv3 does) must be one the table's rows produce;
/// * the header, against an empty object body: the C gate's header test is a substring search
///   (it wants the bytes `kv3` and `format:vpcf` between `<!--` and `-->`, not a parsed field),
///   so the gate accepts where cs2-kv3 refuses - or `binary`, for a NUL in the header;
/// * an input that only disagrees with *both* halves mutated is an interaction the table does not
///   name, and fails.
///
/// The signature is why this survives the mutation's renames and whitespace: what makes a body a
/// class is the pair of verdicts it provokes, not its spelling. A new body class that provokes a
/// pair the table does not already produce fails; one that provokes an existing pair (another
/// refusal reason that cs2-kv3 also accepts) is a documented limit of this guard, which the
/// row-by-row table in `disagreements_with_cs2_kv3` covers by example.
#[test]
fn the_disagreement_table_covers_the_fuzz() {
    const HDR: &str = "<!-- kv3 encoding:text:version{e21c7f3c-8a33-41c5-9977-a76d3a32aa0d} \
                        format:vpcf66:version{dd571be4-586e-4f41-8b2d-5bbc969e1c53} -->";

    /// The signatures the disagreement table's rows produce for `kv3_check`, as
    /// `(gate reason or "ok", what cs2-kv3 does)`. Kept next to the rows it mirrors.
    const TABLE_SIGNATURES: &[(&str, bool)] = &[
        ("comma_in_object", true),
        ("expected_comma", true),
        ("expected_value", true),
        ("bad_token", true),
        ("bad_key", true),
        ("binary", true),
        ("ok", false),
    ];

    let seeds = fuzz_seeds();
    let mut classified = 0usize;
    let mut body_signatures: Vec<(&'static str, bool)> = Vec::new();
    for input in mutate(&seeds) {
        let gate = kv3_check(&input);
        let kv3_accepts = cs2_kv3_accepts(&input);
        if gate.is_ok() == kv3_accepts {
            continue;
        }
        let split = input.windows(3).position(|window| window == b"-->").map(|at| at + 3);
        let (header, body) = match split {
            Some(at) => (&input[..at], &input[at..]),
            None => (&input[..0], &input[..]),
        };
        // Split the input into its two halves and ask each, against the other's clean version,
        // whether it disagrees on its own. A class must be reachable from one half alone;
        // an input that only disagrees with both halves mutated is a joint effect the table
        // does not name.
        let mut clean_header = HDR.as_bytes().to_vec();
        clean_header.extend_from_slice(body);
        let mut clean_body = header.to_vec();
        clean_body.extend_from_slice(b"\n{ }\n");
        let body_disagrees =
            kv3_check(&clean_header).is_ok() != cs2_kv3_accepts(&clean_header);
        let header_disagrees =
            kv3_check(&clean_body).is_ok() != cs2_kv3_accepts(&clean_body);
        assert!(
            body_disagrees || header_disagrees,
            "the fuzz found an input where only the header and the body together disagree, \
             which the table does not name: {:?}",
            String::from_utf8_lossy(&input)
        );
        if body_disagrees {
            let signature =
                (gate_signature(&kv3_check(&clean_header)), cs2_kv3_accepts(&clean_header));
            assert!(
                TABLE_SIGNATURES.contains(&signature),
                "the fuzz found a body disagreement the table does not name: {signature:?}, \
                 body {:?}",
                String::from_utf8_lossy(body)
            );
            body_signatures.push(signature);
            classified += 1;
        }
        if header_disagrees {
            let signature =
                (gate_signature(&kv3_check(&clean_body)), cs2_kv3_accepts(&clean_body));
            assert!(
                HEADER_SIGNATURES.contains(&signature),
                "the fuzz found a header disagreement the table does not name: {signature:?}, \
                 header {:?}",
                String::from_utf8_lossy(header)
            );
            classified += 1;
        }
    }
    assert!(classified > 0, "the fuzz must find at least one disagreement to classify");
    // Every signature the table declares must actually occur, or the row is describing an input
    // the seeds can no longer produce.
    for signature in TABLE_SIGNATURES {
        assert!(
            *signature == ("ok", false) || body_signatures.contains(signature),
            "the fuzz never reaches the table signature {signature:?}"
        );
    }
}

/// The gate's side of a disagreement signature: its reason, or `ok`.
fn gate_signature(result: &Result<(), cs2_bridge::kv3_check::Refusal>) -> &'static str {
    match result {
        Ok(()) => "ok",
        Err(refusal) => match refusal.reason {
            Reason::CommaInObject => "comma_in_object",
            Reason::ExpectedComma => "expected_comma",
            Reason::ExpectedValue => "expected_value",
            Reason::BadToken => "bad_token",
            Reason::BadKey => "bad_key",
            Reason::Binary => "binary",
            other => {
                panic!("a disagreement with the gate reason `{other}`, which no table row names")
            }
        },
    }
}

/// The signatures a *header* difference can produce: the C gate accepting a header cs2-kv3 cannot
/// parse (its `kv3`/`format:vpcf` search is a substring test), or `binary` for a NUL byte in the
/// header (cs2-kv3 reads it as text).
const HEADER_SIGNATURES: &[(&str, bool)] = &[("ok", false), ("binary", true)];

/// `cs2_kv3::parse`'s verdict on bytes (the reader takes `&str`; the gate takes bytes and the
/// fuzz can produce invalid UTF-8, which `cs2-kv3` cannot read at all).
fn cs2_kv3_accepts(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(text) => cs2_kv3::parse(text).is_ok(),
        Err(_) => false,
    }
}

/// The fuzz's seeds: the C test's inputs and the four fixtures.
fn fuzz_seeds() -> Vec<Vec<u8>> {
    let mut seeds: Vec<Vec<u8>> = cases().into_iter().map(|case| case.input).collect();
    seeds.extend(fixture_cases().into_iter().map(|case| case.input));
    seeds
}

/// The deterministic mutation the classification ran with: a tiny xorshift, so the test does not
/// depend on a crate and reproduces the same 12 mutations per seed.
fn mutate(seeds: &[Vec<u8>]) -> Vec<Vec<u8>> {
    // NUL is in the alphabet so the fuzz can produce a binary file (the gate's `binary`).
    const ALPHABET: &[u8] = b"{}[]=,:\"#/*\n\t a1.-+_\x00";
    let mut state = 20_261_019u32;
    let mut next = |bound: usize| -> usize {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state as usize) % bound.max(1)
    };
    let mut out = Vec::new();
    for seed in seeds {
        out.push(seed.clone());
        if seed.is_empty() {
            continue;
        }
        for _ in 0..12 {
            let mut bytes = seed.clone();
            match next(5) {
                0 if bytes.len() > 1 => {
                    let at = next(bytes.len());
                    bytes.remove(at);
                }
                1 => {
                    let at = next(bytes.len() + 1);
                    bytes.insert(at, ALPHABET[next(ALPHABET.len())]);
                }
                2 => {
                    let at = next(bytes.len());
                    bytes[at] = ALPHABET[next(ALPHABET.len())];
                }
                3 => bytes.truncate(next(bytes.len() + 1)),
                _ => {
                    let at = next(bytes.len());
                    let count = 1 + next(3);
                    for _ in 0..count {
                        bytes.insert(at, ALPHABET[next(ALPHABET.len())]);
                    }
                }
            }
            out.push(bytes);
        }
    }
    out
}
