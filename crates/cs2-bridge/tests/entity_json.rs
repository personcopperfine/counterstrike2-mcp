//! `bridge/test_entity_json.c` ported to Rust, case for case, plus the differential corpus the
//! C region and this port must agree on.
//!
//! Three checks live here:
//!
//! 1. [`c_test_cases_ported_one_for_one`]: every case of `bridge/test_entity_json.c` (the C
//!    prints `125 cases, 0 failures`; one of them is its own `region_in_source` check), ported to
//!    the Rust API. The count is asserted on both sides: the C case count is read back from the
//!    rebuilt C test by [`c_region_test_passes`], and this test asserts its own.
//! 2. [`c_region_test_passes`]: the C test is rebuilt exactly as `bridge/build.sh:58-78` builds
//!    it (the region is lifted verbatim out of `bridge/mcp_bridge.c` into a generated header) and
//!    run; `0 failures` is required, and a missing marker or missing `cc` FAILS naming what is
//!    missing.
//! 3. [`differential_corpus_matches_the_c`]: `tests/data/entity_json_cases.txt` is a case list
//!    driven through the *compiled C region* (`tests/data/entity_json_gen.c`, rebuilt with `cc`
//!    against the same extracted header) and through this port; both outputs must equal the
//!    committed `tests/data/entity_json_corpus.txt` byte for byte.
//!
//! [`one_byte_corruption_is_detected`] is the negative half: a single flipped byte in the
//! committed corpus must break both comparisons.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use cs2_bridge::entity_json::{
    batch_has_create, batch_node_recorded, batch_op_next, batch_record_decision, conn_array_parse,
    conn_text_ok, f_finite, json_bal_end, json_estr, json_find, json_find_top, json_float1,
    json_float1_at, json_float1_top, json_float3, json_float3_at, json_float3_top, json_int,
    json_int_at, json_int_top, json_num, json_string, json_string_at, json_string_top,
    json_value_end, kv_next, rm_indices_parse, rm_mode_ok, rm_sort_desc, utf8_prefix, utf8_seq,
    xform_any, xform_origin_ok, xform_origin_reason, xform_scales_ok, ConnSpec, Sb,
    CONN_MAX, CONN_STR_MAX, KV_BOOL, KV_FLOAT, KV_INT, KV_STRING, KV_VALUE_MAX, RM_INDEX_MAX,
    JSON_INT_MAX,
};

const MARK_BEGIN: &str = "/* ---- entity JSON region begin ---- */";
const MARK_END: &str = "/* ---- entity JSON region end ---- */";

/// The repository root (`.../crates/cs2-bridge/../..`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root must exist")
}

/// `bridge/mcp_bridge.c`, the source of truth for the region.
fn bridge_source() -> PathBuf {
    repo_root().join("bridge/mcp_bridge.c")
}

/// `crates/cs2-bridge/tests/data`.
fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

/// The region, lifted out of `bridge/mcp_bridge.c` exactly as `bridge/build.sh:58-76` lifts it.
fn extract_region() -> String {
    let path = bridge_source();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the C region source {} must be readable: {error}", path.display()));
    extract_region_from(&text, &path.display().to_string())
}

/// The extraction itself (`bridge/build.sh:64-74`): exactly one of each marker, hex-literal free,
/// and carrying the scanners. Every failure names what is missing - there is no skip.
fn extract_region_from(text: &str, what: &str) -> String {
    assert_eq!(
        text.matches(MARK_BEGIN).count(),
        1,
        "{what} must carry exactly one `{MARK_BEGIN}` (build.sh:64)"
    );
    assert_eq!(
        text.matches(MARK_END).count(),
        1,
        "{what} must carry exactly one `{MARK_END}` (build.sh:64)"
    );
    let body = text
        .split_once(MARK_BEGIN)
        .expect("the begin marker is present")
        .1
        .split_once(MARK_END)
        .expect("the end marker is present")
        .0;
    for needed in ["json_find_top", "kv_next", "batch_op_next", "sb_addjson"] {
        assert!(body.contains(needed), "the region must carry {needed} (build.sh:71)");
    }
    assert!(
        !has_hex_literal(body),
        "the region must not carry a hex literal (build.sh:67)"
    );
    body.to_string()
}

/// The `bridge/build.sh:67-70` check: a `0x`/`0X` followed by a hex digit and not preceded by an
/// identifier byte (the build's `\b0[xX][0-9a-fA-F]+`). The region's comments may say the words
/// "a 0x literal"; a real literal fails.
fn has_hex_literal(body: &str) -> bool {
    let bytes = body.as_bytes();
    for i in 0..bytes.len().saturating_sub(2) {
        let boundary = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if bytes[i] == b'0'
            && (bytes[i + 1] | 0x20) == b'x'
            && boundary
            && bytes[i + 2].is_ascii_hexdigit()
        {
            return true;
        }
    }
    false
}

/// Writes the extracted region into the cargo target tmpdir and returns the header path.
fn write_region_header() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("the cargo target tmpdir is writable");
    let header = dir.join("entity_json_region.h");
    std::fs::write(&header, extract_region()).expect("the extracted region header is writable");
    header
}

/// `cc`, or a failure naming `cc` (AGENTS.md: a gate that cannot run fails loudly).
fn cc() -> &'static str {
    "cc"
}

/// Compiles `source` with `cc` and returns the binary path. `-std=c11 -O1 -Wall -Wextra` is what
/// `bridge/build.sh:77` uses.
fn compile(source: &Path, name: &str, include: &Path) -> PathBuf {
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let result = Command::new(cc())
        .arg("-std=c11")
        .arg("-O1")
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-I")
        .arg(include)
        .arg("-o")
        .arg(&out)
        .arg(source)
        .output();
    let output = match result {
        Ok(output) => output,
        Err(error) => panic!("cc must be available to build {}: {error}", source.display()),
    };
    assert!(
        output.status.success(),
        "cc failed for {}:\n{}",
        source.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    out
}

// =====================================================================================
// 1. the C test's cases, ported one for one
// =====================================================================================

/// One ported case: a name and a verdict, like the C test's `step` (`test_entity_json.c:54`).
struct Steps {
    cases: usize,
}

impl Steps {
    fn check(&mut self, name: &str, ok: bool, detail: &str) {
        self.cases += 1;
        assert!(ok, "case `{name}` failed: {detail}");
    }
}

fn hexdump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes.iter().take(64) {
        let _ = write!(out, "{byte:02x} ");
    }
    out
}

/// What hammer-mcp's `edit.batch` sends for `entity.create` (`test_entity_json.c:82-85`).
const K_CREATE: &[u8] = b"{\"angles\":[0,0,0],\"class\":\"info_target\",\
\"keyValues\":{\"origin\":\"10 20 30\",\"targetname\":\"mcp_ent\"},\
\"origin\":[512,1024,0]}";

fn top_level_cases(steps: &mut Steps) {
    let old = json_find(K_CREATE, b"origin");
    let top = json_find_top(K_CREATE, b"origin");
    let got = json_float3_top(K_CREATE, b"origin");
    let detail = format!(
        "old->{:?} top->{:?} float3={:?}",
        old.map(|i| K_CREATE[i] as char),
        top.map(|i| K_CREATE[i] as char),
        got
    );
    steps.check(
        "json_find_top_origin_beats_nested",
        old.is_some_and(|i| K_CREATE[i] == b'"')
            && top.is_some_and(|i| K_CREATE[i] == b'[')
            && got == Some([512.0, 1024.0, 0.0]),
        &detail,
    );

    let mut class = [0u8; 64];
    steps.check(
        "json_find_top_class",
        json_string_top(K_CREATE, b"class", &mut class) && cstr(&class) == b"info_target",
        &format!("class={:?}", String::from_utf8_lossy(cstr(&class))),
    );

    let kv = json_find_top(K_CREATE, b"keyValues");
    steps.check(
        "json_find_top_keyvalues_object",
        kv.is_some_and(|i| K_CREATE[i..].starts_with(b"{\"origin\":\"10 20 30\"")),
        &format!("{kv:?}"),
    );

    steps.check(
        "json_find_top_nested_only_is_absent",
        json_find_top(K_CREATE, b"targetname").is_none() && json_find(K_CREATE, b"targetname").is_some(),
        "targetname lives in keyValues only",
    );

    let inside = b"{\"note\":\"has \\\"origin\\\": 5 inside\",\"origin\":[7,8,9]}";
    let got = json_float3_top(inside, b"origin");
    steps.check(
        "json_find_top_skips_string_contents",
        got == Some([7.0, 8.0, 9.0]),
        "a quoted key inside a string is not a member",
    );

    let setkv = b"{\"expectDoc\":{\"id\":\"d1\",\"epoch\":4},\"values\":{\"nodeID\":7},\"nodeID\":2000000000}";
    let id = json_int_top(setkv, b"nodeID");
    let oldid = json_int(setkv, b"nodeID");
    steps.check(
        "json_find_top_nodeid_beats_nested",
        id == Some(2000000000) && oldid == Some(7),
        &format!("top={id:?} old={oldid:?}"),
    );

    let alias = b"{\"values\":{\"nodeID\":7},\"nodeID\":4294967326}";
    steps.check(
        "json_find_top_nodeid_out_of_range_refused",
        json_find_top(alias, b"nodeID").is_some()
            && json_int_top(alias, b"nodeID").is_none()
            && json_int(alias, b"nodeID") == Some(7),
        "the top-level 4294967326 is refused, not wrapped onto node 30",
    );

    let two = b"{\"expectDoc\":{\"id\":\"d1\",\"epoch\":4},\"values\":{}}";
    steps.check(
        "json_find_top_expectdoc",
        json_int_top(two, b"epoch").is_none() && json_int(two, b"epoch") == Some(4),
        "epoch is inside expectDoc, not top level",
    );

    steps.check(
        "json_int_at_rejects_fraction_and_exponent",
        json_int_at(b"1.5", 0).is_none()
            && json_int_at(b"1e5", 0).is_none()
            && json_int_at(b"abc", 0).is_none()
            && json_int_at(b"true", 0).is_none(),
        "a nodeID/limit must be a plain decimal integer",
    );
}

fn int_bound_cases(steps: &mut Steps) {
    let max_ok = json_int_at(b"2147483647", 0) == Some(JSON_INT_MAX);
    let over = json_int_at(b"2147483648", 0);
    let twenty = json_int_at(b"18446744073709551646", 0);
    let huge = json_int_at(b"99999999999999999999999999", 0);
    let zero = json_int_at(b"0", 0) == Some(0);
    let neg = json_int_at(b"-1", 0) == Some(-1);
    let negmin = json_int_at(b"-2147483648", 0);
    steps.check(
        "json_int_at_bounded_to_int32",
        max_ok && over.is_none() && twenty.is_none() && huge.is_none() && zero && neg && negmin.is_none(),
        &format!("max={max_ok} over={over:?} 20digit={twenty:?} huge={huge:?} zero={zero} neg={neg} min={negmin:?}"),
    );

    let epoch = json_int(b"{\"epoch\":4}", b"epoch") == Some(4);
    let width = json_int(b"{\"width\":1920}", b"width") == Some(1920);
    let deadline = json_int(b"{\"deadlineMs\":0}", b"deadlineMs") == Some(0);
    let e_over = json_int(b"{\"epoch\":18446744073709551646}", b"epoch");
    let e_huge = json_int(b"{\"epoch\":99999999999999999999999999}", b"epoch");
    steps.check(
        "json_int_no_signed_overflow",
        epoch && width && deadline && e_over.is_none() && e_huge.is_none(),
        &format!("epoch={epoch} width={width} deadlineMs={deadline} over={e_over:?} huge={e_huge:?}"),
    );

    let mut digits = vec![b'0'; 401];
    digits[0] = b'1';
    let big = json_num(&digits, 0);
    let exp400 = json_num(b"1e400", 0);
    let exp20 = json_num(b"1e20", 0);
    steps.check(
        "json_num_rejects_non_finite",
        big.is_none()
            && exp400.is_none()
            && exp20.is_some_and(|(_, v)| v > 1e19 && v < 1e21),
        &format!("1e400={exp400:?} 400-digit={big:?} 1e20={exp20:?}"),
    );

    let narrow = json_float3_at(b"[1e39,0,0]", 0);
    let narrow1 = json_float1_at(b"1e39", 0);
    let normal = json_float3_at(b"[1,2.5,3]", 0);
    let radius = json_float1_at(b"1e20", 0);
    steps.check(
        "json_float_rejects_narrowed_inf",
        narrow.is_none()
            && narrow1.is_none()
            && normal == Some([1.0, 2.5, 3.0])
            && radius.is_some_and(|r| r > 1e19f32),
        &format!("1e39_3={narrow:?} 1e39_1={narrow1:?} normal={normal:?} 1e20={radius:?}"),
    );
}

const K_OPS: &[u8] = b"[{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\",\
\"keyValues\":{\"message\":\"a}b{c\",\"targetname\":\"x\"},\"origin\":[1,2,3]}},\
{\"method\":\"entity.setKeyValues\",\"params\":{\"handle\":\"$0\",\"values\":{\"k\":\"v\"}}}]";

const K_WANT_OP: &[u8] = b"{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\",\
\"keyValues\":{\"message\":\"a}b{c\",\"targetname\":\"x\"},\"origin\":[1,2,3]}}";

const K_WANT_PARAMS: &[u8] = b"{\"class\":\"info_target\",\"keyValues\":{\"message\":\"a}b{c\",\
\"targetname\":\"x\"},\"origin\":[1,2,3]}";

fn batch_cases(steps: &mut Steps) {
    let mut opbuf = [0u8; 8192];
    let mut pbuf = [0u8; 8192];
    let mut pos = 1usize;
    let mut count = 0usize;
    let mut ok = true;
    let mut first_op: Vec<u8> = Vec::new();
    let mut first_params: Vec<u8> = Vec::new();
    loop {
        let step = batch_op_next(K_OPS, pos, &mut opbuf, &mut pbuf);
        if !step.copied {
            pos = step.next;
            break;
        }
        pos = step.next;
        if count == 0 {
            first_op = cstr(&opbuf).to_vec();
            first_params = cstr(&pbuf).to_vec();
        }
        count += 1;
        if count > 8 {
            ok = false; // a splitter that does not advance would hang
            break;
        }
    }
    steps.check(
        "batch_op_next_two_ops",
        ok && count == 2 && K_OPS.get(pos) == Some(&b']'),
        &format!("ops={count} pos={pos} next={:?}", K_OPS.get(pos)),
    );
    steps.check(
        "batch_op_next_braces_in_string",
        first_op == K_WANT_OP,
        &hexdump(&first_op),
    );
    steps.check(
        "batch_op_next_params_complete",
        first_params == K_WANT_PARAMS,
        &hexdump(&first_params),
    );

    let truncated = b"{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\"";
    let mut b1 = [0u8; 256];
    let mut b2 = [0u8; 256];
    steps.check(
        "batch_op_next_unterminated_stops",
        !batch_op_next(truncated, 0, &mut b1, &mut b2).copied,
        "no closing brace -> no op, never a cut one",
    );
}

/// `batch_label` (`test_entity_json.c:281-283`).
fn batch_label(ops: &[u8]) -> &'static str {
    if batch_has_create(ops) {
        "MCP: batch"
    } else {
        "MCP: set keyvalues"
    }
}

fn label_cases(steps: &mut Steps) {
    let set_only = b"[{\"method\":\"entity.setKeyValues\",\"params\":{\"handle\":\"$0\",\"values\":{\"k\":\"v\"}}},\
{\"method\":\"entity.setKeyValues\",\"params\":{\"nodeID\":30,\"values\":{\"k2\":\"v2\"}}}]";
    let create_only = b"[{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\"}}]";
    let create_then_set = b"[{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\"}},\
{\"method\":\"entity.setKeyValues\",\"params\":{\"handle\":\"$0\",\"values\":{\"k\":\"v\"}}}]";
    let set_then_create = b"[{\"method\":\"entity.setKeyValues\",\"params\":{\"handle\":\"$0\",\"values\":{\"k\":\"v\"}}},\
{\"method\":\"entity.create\",\"params\":{\"class\":\"info_target\"}}]";
    let detail = format!(
        "set-only={} create-only={} create-first={} create-last={} empty={}",
        batch_label(set_only),
        batch_label(create_only),
        batch_label(create_then_set),
        batch_label(set_then_create),
        batch_label(b"[]")
    );
    steps.check(
        "batch_label_set_only_is_keyvalues",
        batch_label(set_only) == "MCP: set keyvalues" && batch_label(b"[]") == "MCP: set keyvalues",
        &detail,
    );
    steps.check(
        "batch_label_with_create_is_batch",
        batch_label(create_only) == "MCP: batch"
            && batch_label(create_then_set) == "MCP: batch"
            && batch_label(set_then_create) == "MCP: batch",
        &detail,
    );
    steps.check(
        "batch_label_reads_the_method_member",
        !batch_has_create(
            b"[{\"method\":\"entity.setKeyValues\",\"params\":{\"values\":\
{\"note\":\"entity.create\",\"class\":\"entity.create\"}}}]",
        ) && !batch_has_create(b"[{\"method\":\"entity.createX\",\"params\":{}}]"),
        "a create-named value or a longer method name is not a create op",
    );
}

fn recorded_cases(steps: &mut Steps) {
    let ids: Vec<i32> = Vec::new();
    steps.check(
        "batch_recorded_empty",
        !batch_node_recorded(&ids, 30),
        "an empty set holds nothing",
    );
    let ids = vec![30, 45];
    steps.check(
        "batch_recorded_hit",
        batch_node_recorded(&ids, 30) && batch_node_recorded(&ids, 45),
        "both ids are recorded",
    );
    steps.check(
        "batch_recorded_miss",
        !batch_node_recorded(&ids, 31),
        "an id the group never saw is not recorded",
    );
    let zero = [0];
    steps.check(
        "batch_recorded_zero_id",
        batch_node_recorded(&zero, 0),
        "nodeID 0 is matched like any other",
    );
    steps.check(
        "batch_recorded_negative_miss",
        !batch_node_recorded(&ids, -1),
        "-1 is the walk's own 'no id', never a recorded node",
    );

    let mut set = [0i32; 32];
    let mut m = 0usize;
    steps.check(
        "batch_record_decision_first_records",
        batch_record_decision(&mut set, &mut m, 32, 30) && m == 1 && set[0] == 30,
        "the first setKeyValues on a node records it",
    );
    steps.check(
        "batch_record_decision_second_skips",
        !batch_record_decision(&mut set, &mut m, 32, 30) && m == 1,
        "a second op on the SAME node in the same batch does not record it again (W2 #7)",
    );
    steps.check(
        "batch_record_decision_other_node",
        batch_record_decision(&mut set, &mut m, 32, 45) && m == 2,
        "another node is recorded on its own first write",
    );
    steps.check(
        "batch_record_decision_negative_never",
        !batch_record_decision(&mut set, &mut m, 32, -1) && m == 2,
        "-1 (an unresolved node) is never recorded and never appended",
    );

    let mut full = [0i32; 2];
    let mut f = 0usize;
    batch_record_decision(&mut full, &mut f, 2, 1);
    batch_record_decision(&mut full, &mut f, 2, 2);
    let third = batch_record_decision(&mut full, &mut f, 2, 3);
    steps.check(
        "batch_record_decision_cap",
        f == 2 && third && !batch_record_decision(&mut full, &mut f, 2, 1),
        "the table stops growing at cap; a recorded node is still recognized",
    );
}

/// One keyvalue row, like the C test's `KvRow`.
struct KvRow {
    name: Vec<u8>,
    kind: i32,
    sval: Vec<u8>,
    nval: Vec<u8>,
    over: i32,
}

/// `kv_all` (`test_entity_json.c:372-388`): the caller skips the opening brace, exactly like
/// `apply_keyvalues` does.
fn kv_all(obj: &[u8], max: usize) -> Option<Vec<KvRow>> {
    let mut rows = Vec::new();
    let mut pos = 1usize;
    loop {
        if rows.len() >= max {
            return None;
        }
        let mut name = [0u8; 64];
        let mut sval = [0u8; 1200];
        let mut nval = [0u8; 64];
        let Some(value) = kv_next(obj, pos, &mut name, &mut sval, &mut nval) else {
            break;
        };
        pos = value.next;
        rows.push(KvRow {
            name: cstr(&name).to_vec(),
            kind: value.kind,
            sval: cstr(&sval).to_vec(),
            nval: cstr(&nval).to_vec(),
            over: value.over,
        });
    }
    Some(rows)
}

fn kv_cases(steps: &mut Steps) {
    let rows = kv_all(b"{\"a\":1e20,\"b\":-1.5E-7,\"c\":4,\"d\":-2.25}", 16).expect("4 rows");
    steps.check(
        "kv_next_exponent_tokens",
        rows.len() == 4
            && rows[0].kind == KV_FLOAT
            && rows[0].nval == b"1e20"
            && rows[1].kind == KV_FLOAT
            && rows[1].nval == b"-1.5E-7"
            && rows[2].kind == KV_INT
            && rows[2].nval == b"4"
            && rows[3].kind == KV_FLOAT,
        &format!(
            "keys={} a={} b={} c={}",
            rows.len(),
            String::from_utf8_lossy(&rows[0].nval),
            String::from_utf8_lossy(&rows[1].nval),
            String::from_utf8_lossy(&rows[2].nval)
        ),
    );
    {
        let q = json_num(b"1e20", 0);
        let q2 = json_num(b"-1.5E-7", 0);
        let q3 = json_num(b"5.5e-17", 0);
        let q4 = json_num(b"1e", 0);
        let ok = q.is_some_and(|(e, v)| e == 4 && v > 1e19 && v < 1e21)
            && q2.is_some_and(|(e, v)| e == 7 && v < 0.0 && v > -1e-6)
            && q3.is_some_and(|(e, v)| e == 7 && v > 0.0 && v < 1e-16)
            && q4.is_some_and(|(e, v)| e == 1 && v == 1.0);
        steps.check(
            "json_num_exponents",
            ok,
            &format!("1e20={q:?} -1.5E-7={q2:?} 5.5e-17={q3:?} 1e={q4:?}"),
        );
    }

    let rows = kv_all(b"{\"s\":\"\\u0041\\u00e9\\ud83d\\ude00\\b\\f\\/\"}", 16).expect("1 row");
    let want_esc: &[u8] = &[b'A', 0xC3, 0xA9, 0xF0, 0x9F, 0x98, 0x80, b'\x08', b'\x0c', b'/'];
    steps.check(
        "kv_next_unicode_escapes",
        rows.len() == 1 && rows[0].kind == KV_STRING && rows[0].sval == want_esc,
        &format!("keys={} bytes={}", rows.len(), hexdump(&rows[0].sval)),
    );
    let rows = kv_all(b"{\"classname\\u0000x\":\"a\\u0000b\"}", 16).expect("1 row");
    steps.check(
        "kv_next_nul_escape_replaced",
        rows.len() == 1
            && rows[0].name != b"classname"
            && rows[0].sval == [b'a', 0xEF, 0xBF, 0xBD, b'b'],
        &hexdump(&rows[0].sval),
    );
    let rows = kv_all(b"{\"s\":\"\\ud83d\"}", 16).expect("1 row");
    steps.check(
        "kv_next_lone_surrogate",
        rows.len() == 1 && rows[0].sval == [0xEF, 0xBF, 0xBD],
        &hexdump(&rows[0].sval),
    );
    let rows = kv_all(b"{\"a\\u0062\":1,\"x\":\"\\x\"}", 16).expect("2 rows");
    steps.check(
        "kv_next_name_and_unknown_escape",
        rows.len() == 2 && rows[0].name == b"ab" && rows[1].sval == b"x",
        &String::from_utf8_lossy(&rows[0].name),
    );
    let rows = kv_all(b"{\"k\":\"a\\\"b\"}", 16).expect("1 row");
    steps.check(
        "kv_next_escaped_quote",
        rows.len() == 1 && rows[0].sval == b"a\"b",
        &String::from_utf8_lossy(&rows[0].sval),
    );

    {
        let value = vec![b'a'; KV_VALUE_MAX];
        let obj = [b"{\"long\":\"".as_slice(), &value, b"\",\"after\":42}"].concat();
        let mut name = [0u8; 64];
        let mut sval = vec![0u8; KV_VALUE_MAX + 1];
        let mut nval = [0u8; 64];
        let mut ok = false;
        if let Some(first) = kv_next(&obj, 1, &mut name, &mut sval, &mut nval) {
            ok = first.over == 0 && cstr(&sval).len() == KV_VALUE_MAX;
            if let Some(second) = kv_next(&obj, first.next, &mut name, &mut sval, &mut nval) {
                ok = ok && name.starts_with(b"after") && second.kind == KV_INT && second.ival == 42;
            } else {
                ok = false;
            }
        }
        steps.check(
            "kv_next_value_fits_cap",
            ok,
            "KV_VALUE_MAX bytes decode whole, the next key parses",
        );

        let value = vec![b'a'; KV_VALUE_MAX + 1];
        let obj = [b"{\"long\":\"".as_slice(), &value, b"\",\"after\":\"ok\"}"].concat();
        let mut small = [0u8; 16];
        let mut ok = false;
        if let Some(first) = kv_next(&obj, 1, &mut name, &mut small, &mut nval) {
            ok = first.over == 1;
            if let Some(second) = kv_next(&obj, first.next, &mut name, &mut sval, &mut nval) {
                ok = ok && name.starts_with(b"after") && cstr(&sval) == b"ok" && second.over == 0;
            } else {
                ok = false;
            }
        }
        steps.check(
            "kv_next_overlong_reports_and_continues",
            ok,
            "overlong value -> over=1, later keys are not lost",
        );
    }

    let rows = kv_all(b"{\"a\":null,\"b\":true,\"c\":false}", 16).expect("3 rows");
    steps.check(
        "kv_next_null_and_bools",
        rows.len() == 3
            && rows[0].kind == KV_STRING
            && rows[0].sval.is_empty()
            && rows[1].kind == KV_BOOL
            && rows[2].kind == KV_BOOL,
        "null/true/false",
    );

    let rows = kv_all(b"{\"a\":\"\",\"b\":\"x\"}", 16).expect("2 rows");
    steps.check(
        "kv_next_empty_string_value",
        rows.len() == 2
            && rows[0].kind == KV_STRING
            && rows[0].sval.is_empty()
            && rows[0].over == 0
            && rows[1].kind == KV_STRING
            && rows[1].sval == b"x",
        "an empty value decodes whole and the next key still parses",
    );

    let rows = kv_all(b"{\"a\":18446744073709551646,\"b\":\"x\"}", 16).expect("2 rows");
    let big_int = rows.len() == 2
        && rows[0].over == 2
        && rows[0].kind == KV_INT
        && rows[0].nval == b"18446744073709551646"
        && rows[1].over == 0
        && rows[1].name == b"b"
        && rows[1].sval == b"x";
    let rows = kv_all(b"{\"a\":1e400,\"b\":\"x\"}", 16).expect("2 rows");
    let big_exp = rows.len() == 2
        && rows[0].over == 2
        && rows[0].kind == KV_FLOAT
        && rows[0].nval == b"1e400"
        && rows[1].over == 0
        && rows[1].sval == b"x";
    let rows = kv_all(b"{\"a\":1e39,\"b\":\"x\"}", 16).expect("2 rows");
    let big_f32 = rows.len() == 2
        && rows[0].over == 2
        && rows[0].kind == KV_FLOAT
        && rows[0].nval == b"1e39"
        && rows[1].over == 0
        && rows[1].sval == b"x";
    let rows = kv_all(b"{\"a\":4294967296,\"b\":8000000000000000000,\"c\":-2147483648}", 16)
        .expect("3 rows");
    let ok_nums = rows.len() == 3
        && rows[0].over == 0
        && rows[0].kind == KV_INT
        && rows[0].nval == b"4294967296"
        && rows[1].over == 0
        && rows[1].kind == KV_INT
        && rows[1].nval == b"8000000000000000000"
        && rows[2].over == 0
        && rows[2].kind == KV_INT;
    steps.check(
        "kv_next_flags_unrepresentable_numbers",
        big_int && big_exp && big_f32 && ok_nums,
        &format!("20digit={big_int} 1e400={big_exp} 1e39={big_f32} representable={ok_nums}"),
    );
}

/// `literal_cases` (`test_entity_json.c:547-605`). The C runs the truncated literals on a
/// guard-page buffer so a read past the terminator faults; the Rust port cannot read past a
/// slice at all, so the same inputs must simply be malformed.
fn literal_cases(steps: &mut Steps) {
    {
        let obj = b"{\"k\":true";
        let end = obj.len();
        let mut name = [0u8; 64];
        let mut sval = [0u8; 1200];
        let mut nval = [0u8; 64];
        let got = kv_next(obj, 1, &mut name, &mut sval, &mut nval);
        steps.check(
            "kv_next_literal_at_frame_end",
            got.is_some_and(|v| v.next == end && v.kind == KV_BOOL && v.ival == 1),
            &format!("{got:?} end={end}"),
        );
    }

    {
        let frames: &[&[u8]] = &[
            b"{\"k\":t",
            b"{\"k\":tr",
            b"{\"k\":tru",
            b"{\"k\":n",
            b"{\"k\":nu",
            b"{\"k\":nul",
            b"{\"k\":f",
            b"{\"k\":fa",
            b"{\"k\":fal",
            b"{\"k\":fals",
        ];
        let mut parsed = 0usize;
        for frame in frames {
            let mut name = [0u8; 64];
            let mut sval = [0u8; 1200];
            let mut nval = [0u8; 64];
            if kv_next(frame, 1, &mut name, &mut sval, &mut nval).is_some() {
                parsed += 1; // a value would mean a jump past the terminator
            }
        }
        steps.check(
            "kv_next_truncated_literal_is_malformed",
            parsed == 0,
            &format!("{} frames: {parsed} not malformed", frames.len()),
        );
    }

    {
        let obj = b"{\"k\":null}";
        let mut name = [0u8; 64];
        let mut sval = [0u8; 1200];
        let mut nval = [0u8; 64];
        let got = kv_next(obj, 1, &mut name, &mut sval, &mut nval);
        steps.check(
            "kv_next_null_complete",
            got.is_some_and(|v| obj[v.next] == b'}' && v.kind == KV_STRING),
            "a complete null still reads as the empty string",
        );
    }
}

/// The C string `s` denotes (its first NUL byte ends it).
fn cstr(s: &[u8]) -> &[u8] {
    match s.iter().position(|&byte| byte == 0) {
        Some(nul) => &s[..nul],
        None => s,
    }
}

fn addjson_case(steps: &mut Steps, name: &str, input: &[u8], want: &[u8]) {
    let mut buf = [0u8; 512];
    let mut builder = Sb::new(&mut buf);
    builder.addjson(input);
    steps.check(
        name,
        !builder.ovf() && builder.bytes() == want,
        &format!("ovf={} bytes={}", builder.ovf(), hexdump(builder.bytes())),
    );
}

fn addjson_cases(steps: &mut Steps) {
    addjson_case(steps, "sb_addjson_plain", b"hi", b"\"hi\"");
    addjson_case(
        steps,
        "sb_addjson_escapes",
        b"\"\\\n\t\r\x01",
        b"\"\\\"\\\\\\n\\t\\r\\u0001\"",
    );
    addjson_case(
        steps,
        "sb_addjson_valid_utf8",
        b"caf\xC3\xA9",
        &[b'"', b'c', b'a', b'f', 0xC3, 0xA9, b'"'],
    );
    addjson_case(
        steps,
        "sb_addjson_four_byte_utf8",
        b"\xF0\x9F\x98\x80",
        &[b'"', 0xF0, 0x9F, 0x98, 0x80, b'"'],
    );
    addjson_case(
        steps,
        "sb_addjson_invalid_bytes",
        b"a\xFF\xFEb",
        &[b'"', b'a', 0xEF, 0xBF, 0xBD, 0xEF, 0xBF, 0xBD, b'b', b'"'],
    );
    addjson_case(
        steps,
        "sb_addjson_cut_sequence",
        b"a\xE2\x82",
        &[b'"', b'a', 0xEF, 0xBF, 0xBD, b'"'],
    );
    addjson_case(
        steps,
        "sb_addjson_lone_continuation",
        b"\x80",
        &[b'"', 0xEF, 0xBF, 0xBD, b'"'],
    );
    addjson_case(
        steps,
        "sb_addjson_overlong_sequence",
        b"\xC0\xAF",
        &[b'"', 0xEF, 0xBF, 0xBD, 0xEF, 0xBF, 0xBD, b'"'],
    );
    addjson_case(
        steps,
        "sb_addjson_raw_surrogate",
        b"\xED\xA0\x80",
        &[b'"', 0xEF, 0xBF, 0xBD, b'"'],
    );

    {
        let mut small = [0u8; 8];
        let mut builder = Sb::new(&mut small);
        builder.addjson(b"0123456789");
        steps.check(
            "sb_addjson_overflow_flag",
            builder.ovf() && small[0] == b'"',
            "a long string sets ovf",
        );
    }
}

fn utf8_cases(steps: &mut Steps) {
    let a = utf8_prefix(b"abc", 3);
    let b = utf8_prefix(b"caf\xC3\xA9", 5);
    let c = utf8_prefix(b"caf\xC3\xA9", 4);
    let d = utf8_prefix(b"caf\xC3", 4);
    let e = utf8_prefix(b"\xF0\x9F\x98", 3);
    let f = utf8_prefix(b"\xF0\x9F\x98\x80", 4);
    let g = utf8_prefix(b"caf\xC3\xA9", 0);
    steps.check(
        "utf8_prefix_cuts_on_boundary",
        a == 3 && b == 5 && c == 3 && d == 3 && e == 0 && f == 4 && g == 0,
        &format!("abc={a} eacute={b} cut={c} lead={d} 3of4={e} 4of4={f} 0={g}"),
    );
}

fn scanner_cases(steps: &mut Steps) {
    let o = b"{\"a\":{\"b\":\"}\"},\"c\":[1,2]}tail";
    let e = json_bal_end(o, 0);
    steps.check(
        "json_bal_end_nested",
        e == Some(25) && o.get(25..) == Some(&b"tail"[..]),
        "the outer } closes the span",
    );

    let s = b"\"a\\\"b\"x";
    steps.check(
        "json_value_end_string_escape",
        json_value_end(s, 0) == 6,
        "an escaped quote does not end it",
    );
    steps.check(
        "json_value_end_scalar",
        json_value_end(b"12.5,rest", 0) == 4,
        "a scalar stops at the comma",
    );
    steps.check(
        "json_bal_end_unterminated",
        json_bal_end(b"{\"a\":[1", 0).is_none(),
        "an open span -> None",
    );

    {
        let two = b"{\"a\":{\"class\":\"nested\"},\"class\":\"top\"}";
        let mut out = [0u8; 32];
        let ok = json_string(two, b"class", &mut out) && cstr(&out) == b"nested"
            && json_string_top(two, b"class", &mut out)
            && cstr(&out) == b"top";
        steps.check("json_string_top_vs_nested", ok, &String::from_utf8_lossy(cstr(&out)));
    }
    {
        let wp = b"{\"origin\":[0,0,0],\"radius\":1e20}";
        let ok = json_float1_top(wp, b"radius").is_some_and(|r| r > 1e19f32)
            && json_float3_top(wp, b"origin") == Some([0.0, 0.0, 0.0]);
        steps.check(
            "json_float_radius_exponent",
            ok,
            &format!("radius={:?} origin={:?}", json_float1_top(wp, b"radius"), json_float3_top(wp, b"origin")),
        );
    }
    {
        let ok = json_float3(K_CREATE, b"origin").is_none()
            && json_float1(b"{\"fov\":90}", b"fov") == Some(90.0);
        steps.check(
            "json_float3_find_is_depth_blind",
            ok,
            "nested origin is a string -> None",
        );
    }
    {
        let mut buf = [0u8; 32];
        let mut builder = Sb::new(&mut buf);
        builder.addi(-32602);
        builder.add(b"/");
        builder.addf(1.5);
        builder.add(b"/");
        builder.addf(f32::NAN);
        steps.check(
            "sb_addi_addf",
            cstr(&buf) == b"-32602/1.500/null",
            &String::from_utf8_lossy(cstr(&buf)),
        );
    }
}

fn xform_cases(steps: &mut Steps) {
    steps.check("xform_scales_accept", xform_scales_ok(&[1.0, 2.0, 3.0]), "1,2,3 is a legal scale");
    steps.check(
        "xform_scales_accept_fraction",
        xform_scales_ok(&[0.5, 1.0, 1.0]),
        "0.5 is inside (0, 1000]",
    );
    steps.check(
        "xform_scales_accept_max",
        xform_scales_ok(&[1000.0, 1000.0, 1000.0]),
        "1000 is the inclusive upper bound",
    );
    steps.check(
        "xform_scales_refuse_zero",
        !xform_scales_ok(&[1.0, 0.0, 1.0]),
        "a 0 scale degenerates the transform",
    );
    steps.check(
        "xform_scales_refuse_negative",
        !xform_scales_ok(&[1.0, -1.0, 1.0]),
        "a negative scale is refused",
    );
    steps.check(
        "xform_scales_refuse_over_max",
        !xform_scales_ok(&[1.0, 1000.5, 1.0]),
        "1000.5 is above the bound",
    );
    steps.check(
        "xform_scales_refuse_nan_inf",
        !xform_scales_ok(&[1.0, f32::NAN, 1.0]) && !xform_scales_ok(&[1.0, f32::INFINITY, 1.0]),
        "NaN and +inf are not scales (NaN also compares false against 0)",
    );
    steps.check(
        "xform_origin_accept_edges",
        xform_origin_ok(&[1048576.0, -1048576.0, 0.0]),
        "the bound is inclusive per component",
    );
    steps.check(
        "xform_origin_refuse_over",
        !xform_origin_ok(&[0.0, 0.0, 1048577.0]),
        "1048577 is above the bound",
    );
    steps.check(
        "xform_origin_refuse_nan",
        !xform_origin_ok(&[f32::NAN, 0.0, 0.0]),
        "a NaN component is refused",
    );

    let mut why = [0u8; 96];
    xform_origin_reason(&mut why);
    steps.check(
        "xform_origin_text_names_the_bound",
        cstr(&why) == b"origin: each |component| <= 1048576",
        &String::from_utf8_lossy(cstr(&why)),
    );
    steps.check(
        "xform_origin_text_has_no_old_bound",
        !cstr(&why).windows(5).any(|w| w == b"65536"),
        &String::from_utf8_lossy(cstr(&why)),
    );

    steps.check(
        "xform_any_truth",
        xform_any(true, false, false)
            && xform_any(false, true, false)
            && xform_any(false, false, true)
            && xform_any(true, true, true)
            && !xform_any(false, false, false),
        "any non-zero presence flag passes, all-zero is refused",
    );
}

/// `conn_parse` (`test_entity_json.c:782-785`): the count, or `None` for a refusal.
fn conn_parse(json: &[u8]) -> Option<usize> {
    let mut specs = vec![ConnSpec::default(); CONN_MAX];
    conn_array_parse(json, &mut specs, CONN_MAX)
}

fn conn_cases(steps: &mut Steps) {
    steps.check(
        "conn_parse_minimal",
        conn_parse(b"[{\"output\":\"OnUser1\",\"target\":\"t\",\"input\":\"Kill\"}]") == Some(1),
        "one entry with only the three required names",
    );

    let mut specs = vec![ConnSpec::default(); CONN_MAX];
    conn_array_parse(
        b"[{\"output\":\"O\",\"target\":\"T\",\"input\":\"I\"}]",
        &mut specs,
        CONN_MAX,
    );
    steps.check(
        "conn_parse_defaults",
        specs[0].param().is_empty() && specs[0].delay == 0.0 && specs[0].times_to_fire == -1,
        "param/delay/timesToFire defaults",
    );

    {
        let mut big = String::from("[");
        for i in 0..CONN_MAX {
            if i != 0 {
                big.push(',');
            }
            big.push_str("{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\"}");
        }
        big.push(']');
        steps.check(
            "conn_parse_64_ok",
            conn_parse(big.as_bytes()) == Some(CONN_MAX),
            "64 entries is the documented maximum",
        );
        let big2 = format!("{},{{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\"}}]", &big[..big.len() - 1]);
        steps.check(
            "conn_parse_65_refused",
            conn_parse(big2.as_bytes()).is_none(),
            "65 entries is above the maximum",
        );
    }

    steps.check("conn_parse_empty_refused", conn_parse(b"[]").is_none(), "at least one connection is required");
    steps.check(
        "conn_parse_missing_input_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\"}]").is_none(),
        "input is required",
    );
    steps.check(
        "conn_parse_empty_name_refused",
        conn_parse(b"[{\"output\":\"\",\"target\":\"t\",\"input\":\"i\"}]").is_none(),
        "an empty output is refused",
    );
    steps.check(
        "conn_parse_unknown_member_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"targt\":\"typo\"}]").is_none(),
        "an unknown member is refused instead of dropped (a typo must not lose a field)",
    );
    steps.check(
        "conn_parse_number_output_refused",
        conn_parse(b"[{\"output\":5,\"target\":\"t\",\"input\":\"i\"}]").is_none(),
        "a number where a name belongs is refused",
    );
    steps.check(
        "conn_parse_param_optional_empty_ok",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"param\":\"\"}]") == Some(1),
        "an empty param is legal (it is the default)",
    );

    steps.check(
        "conn_parse_delay_int_ok",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":3600}]") == Some(1),
        "3600 is the inclusive upper bound",
    );
    conn_array_parse(
        b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":0.5}]",
        &mut specs,
        CONN_MAX,
    );
    steps.check(
        "conn_parse_delay_decimal",
        specs[0].delay == 0.5,
        "a decimal delay is kept",
    );
    steps.check(
        "conn_parse_delay_negative_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":-0.5}]").is_none(),
        "a negative delay is refused",
    );
    steps.check(
        "conn_parse_delay_over_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":3600.5}]").is_none(),
        "3600.5 is above the bound",
    );
    steps.check(
        "conn_parse_delay_string_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"delay\":\"1\"}]").is_none(),
        "a string delay is refused",
    );

    conn_array_parse(
        b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":-1}]",
        &mut specs,
        CONN_MAX,
    );
    steps.check(
        "conn_parse_times_always",
        specs[0].times_to_fire == -1,
        "-1 means always",
    );
    conn_array_parse(
        b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":3}]",
        &mut specs,
        CONN_MAX,
    );
    steps.check(
        "conn_parse_times_count",
        specs[0].times_to_fire == 3,
        "a positive count is kept",
    );
    steps.check(
        "conn_parse_times_zero_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":0}]").is_none(),
        "0 is not a legal firing count",
    );
    steps.check(
        "conn_parse_times_float_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":1.5}]").is_none(),
        "a float count is refused",
    );
    steps.check(
        "conn_parse_times_below_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\",\"timesToFire\":-2}]").is_none(),
        "-2 is below the always sentinel",
    );

    let s255 = vec![b'x'; CONN_STR_MAX];
    let s256 = vec![b'x'; CONN_STR_MAX + 1];
    steps.check("conn_text_255_ok", conn_text_ok(&s255, true), "255 bytes is the maximum");
    steps.check("conn_text_256_refused", !conn_text_ok(&s256, true), "256 bytes is above it");
    steps.check("conn_text_empty_required", !conn_text_ok(b"", true), "an empty required string is refused");
    steps.check("conn_text_empty_optional", conn_text_ok(b"", false), "an empty optional (param) is fine");
    steps.check("conn_text_newline_refused", !conn_text_ok(b"a\nb", true), "a newline is a control character");
    steps.check("conn_text_del_refused", !conn_text_ok(b"a\x7fb", true), "DEL is refused");
    steps.check("conn_text_utf8_ok", conn_text_ok(b"caf\xc3\xa9", true), "high bytes are not controls");
    let huge = [b"[{\"output\":\"".as_slice(), &s256, b"\",\"target\":\"t\",\"input\":\"i\"}]"].concat();
    steps.check(
        "conn_parse_long_name_refused",
        conn_parse(&huge).is_none(),
        "a 256-byte name is refused",
    );
    let huge = [b"[{\"output\":\"".as_slice(), &s255, b"\",\"target\":\"t\",\"input\":\"i\"}]"].concat();
    steps.check("conn_parse_255_name_ok", conn_parse(&huge) == Some(1), "a 255-byte name is accepted");
    steps.check(
        "conn_parse_control_refused",
        conn_parse(b"[{\"output\":\"a\tb\",\"target\":\"t\",\"input\":\"i\"}]").is_none(),
        "an escaped tab is still a control character",
    );
    steps.check(
        "conn_parse_malformed_refused",
        conn_parse(b"[{\"output\":\"o\",\"target\":\"t\",\"input\":\"i\"").is_none(),
        "an unterminated entry is refused",
    );
}

fn rm_cases(steps: &mut Steps) {
    steps.check(
        "rm_mode_exclusive",
        rm_mode_ok(true, false) && rm_mode_ok(false, true) && !rm_mode_ok(true, true) && !rm_mode_ok(false, false),
        "exactly one of indices/match",
    );

    let mut v = vec![0i32; RM_INDEX_MAX];
    let mut single = vec![0i32; RM_INDEX_MAX];
    steps.check(
        "rm_parse_single",
        rm_indices_parse(b"[3]", &mut single, RM_INDEX_MAX) == Some(1) && single[0] == 3,
        "[3]",
    );
    let mut multi = vec![0i32; RM_INDEX_MAX];
    steps.check(
        "rm_parse_multi",
        rm_indices_parse(b"[0, 2, 5]", &mut multi, RM_INDEX_MAX) == Some(3)
            && multi[..3] == [0, 2, 5],
        "[0, 2, 5]",
    );
    steps.check(
        "rm_parse_duplicate_refused",
        rm_indices_parse(b"[2,2]", &mut v, RM_INDEX_MAX).is_none(),
        "a duplicate index is refused (the set must be the set that is removed)",
    );
    steps.check("rm_parse_empty_refused", rm_indices_parse(b"[]", &mut v, RM_INDEX_MAX).is_none(), "[] removes nothing");
    steps.check("rm_parse_negative_refused", rm_indices_parse(b"[-1]", &mut v, RM_INDEX_MAX).is_none(), "a negative index");
    steps.check(
        "rm_parse_fraction_refused",
        rm_indices_parse(b"[1.5]", &mut v, RM_INDEX_MAX).is_none(),
        "an index is an integer, not a number token",
    );
    steps.check(
        "rm_parse_exponent_refused",
        rm_indices_parse(b"[1e2]", &mut v, RM_INDEX_MAX).is_none(),
        "an exponent lexeme is refused (the nodeID rule)",
    );
    steps.check(
        "rm_parse_over_i32_refused",
        rm_indices_parse(b"[2147483648]", &mut v, RM_INDEX_MAX).is_none(),
        "above INT32_MAX: the (int) cast would alias another entry",
    );
    steps.check(
        "rm_parse_missing_comma_refused",
        rm_indices_parse(b"[1 2]", &mut v, RM_INDEX_MAX).is_none(),
        "a missing comma is malformed",
    );
    steps.check(
        "rm_parse_not_array_refused",
        rm_indices_parse(b"3", &mut v, RM_INDEX_MAX).is_none(),
        "not an array",
    );

    {
        let mut big = String::from("[");
        for i in 0..RM_INDEX_MAX {
            if i != 0 {
                big.push(',');
            }
            let _ = write!(big, "{i}");
        }
        big.push(']');
        steps.check(
            "rm_parse_512_ok",
            rm_indices_parse(big.as_bytes(), &mut v, RM_INDEX_MAX) == Some(RM_INDEX_MAX),
            "512 indices is the maximum",
        );
        let mut big = String::from("[");
        for i in 0..=RM_INDEX_MAX {
            if i != 0 {
                big.push(',');
            }
            let _ = write!(big, "{i}");
        }
        big.push(']');
        steps.check(
            "rm_parse_513_refused",
            rm_indices_parse(big.as_bytes(), &mut v, RM_INDEX_MAX).is_none(),
            "513 is above it",
        );
    }

    {
        let mut d = [2, 7, 0, 7, 3];
        rm_sort_desc(&mut d);
        steps.check(
            "rm_sort_desc_order",
            d == [7, 7, 3, 2, 0],
            "descending, descending-stable for equal keys",
        );
        rm_sort_desc(&mut d[..1]);
        steps.check("rm_sort_desc_single", d[0] == 7, "a one-element set is already ordered");
    }
    steps.check(
        "rm_index_shift",
        3u32.wrapping_shl(16) == 196608 && 0u32.wrapping_shl(16) == 0,
        "idx << 16 is the id form (low word 0)",
    );
}

/// Every case of `bridge/test_entity_json.c` except its own `region_in_source` check (which
/// [`extract_region`] covers): `125 cases, 0 failures` in the C, 124 here.
#[test]
fn c_test_cases_ported_one_for_one() {
    let mut steps = Steps { cases: 0 };
    top_level_cases(&mut steps);
    int_bound_cases(&mut steps);
    batch_cases(&mut steps);
    label_cases(&mut steps);
    recorded_cases(&mut steps);
    kv_cases(&mut steps);
    literal_cases(&mut steps);
    addjson_cases(&mut steps);
    utf8_cases(&mut steps);
    scanner_cases(&mut steps);
    xform_cases(&mut steps);
    conn_cases(&mut steps);
    rm_cases(&mut steps);
    assert_eq!(
        steps.cases, 124,
        "the C test prints `125 cases` including its region_in_source check (test_entity_json.c:961)"
    );
}

/// The C test itself: rebuilt exactly as `bridge/build.sh:58-78` builds it and run. `0 failures`
/// is required, and the case count is returned so the ported test's count can be held against it.
#[test]
fn c_region_test_passes() {
    let include = write_region_header()
        .parent()
        .expect("the header has a parent")
        .to_path_buf();
    let test_src = repo_root().join("bridge/test_entity_json.c");
    assert!(test_src.is_file(), "{} must exist", test_src.display());
    let binary = compile(&test_src, "test_entity_json", &include);
    let output = Command::new(&binary)
        .arg(bridge_source())
        .output()
        .expect("the C region test runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let last = stdout.lines().last().unwrap_or("");
    assert!(
        output.status.success(),
        "the C entity JSON region test must pass:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        last == "125 cases, 0 failures",
        "the C test's summary must be `125 cases, 0 failures`, got `{last}`"
    );
}

// =====================================================================================
// 3. the differential corpus
// =====================================================================================

fn put_hex(out: &mut String, bytes: &[u8]) {
    if bytes.is_empty() {
        out.push('-');
        return;
    }
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
}

fn parse_hex(token: &str) -> Vec<u8> {
    if token == "-" {
        return Vec::new();
    }
    assert!(token.len().is_multiple_of(2), "hex token must be even: {token}");
    (0..token.len() / 2)
        .map(|i| u8::from_str_radix(&token[i * 2..i * 2 + 2], 16).expect("hex token"))
        .collect()
}

fn arg_i64(token: &str) -> i64 {
    token.parse().expect("decimal arg")
}
fn arg_i32(token: &str) -> i32 {
    token.parse().expect("decimal arg")
}
fn arg_u32(token: &str) -> u32 {
    token.parse().expect("decimal arg")
}
fn arg_f32(token: &str) -> f32 {
    f32::from_bits(u32::from_str_radix(token, 16).expect("f32 bit pattern"))
}
fn bits32(value: f32) -> String {
    format!("{:08x}", value.to_bits())
}
fn bits64(value: f64) -> String {
    format!("{:016x}", value.to_bits())
}
fn inline_ids(ids: &[i32]) -> String {
    if ids.is_empty() {
        "-".into()
    } else {
        ids.iter().map(i32::to_string).collect::<Vec<_>>().join(",")
    }
}
fn parse_ids(token: &str) -> Vec<i32> {
    if token == "-" {
        return Vec::new();
    }
    token.split(',').map(|v| v.parse().expect("id")).collect()
}

/// Drive one case line through the Rust port, producing the same line the C generator prints.
///
/// The C generator (`tests/data/entity_json_gen.c`) is the mirror image of this match: a new op
/// must be added to both, and the corpus test fails if they disagree.
fn run_case(op: &str, args: &[&str]) -> String {
    let mut out = String::new();
    let _ = write!(out, "{op}");
    for arg in args {
        out.push('|');
        out.push_str(arg);
    }
    out.push('|');

    match op {
        "utf8_seq" => {
            let _ = write!(out, "{}", utf8_seq(&parse_hex(args[0]), arg_u32(args[1])));
        }
        "utf8_prefix" => {
            let _ = write!(out, "{}", utf8_prefix(&parse_hex(args[0]), arg_u32(args[1])));
        }
        "json_find" | "json_find_top" => {
            let json = parse_hex(args[0]);
            let key = parse_hex(args[1]);
            let found = if op == "json_find" {
                json_find(&json, &key)
            } else {
                json_find_top(&json, &key)
            };
            match found {
                Some(pos) => {
                    let _ = write!(out, "{pos}");
                }
                None => out.push_str("-1"),
            }
        }
        "json_string" | "json_string_top" => {
            let json = parse_hex(args[0]);
            let key = parse_hex(args[1]);
            let mut buf = [0u8; 64];
            let ok = if op == "json_string" {
                json_string(&json, &key, &mut buf)
            } else {
                json_string_top(&json, &key, &mut buf)
            };
            let _ = write!(out, "{}|", i32::from(ok));
            if ok {
                put_hex(&mut out, cstr(&buf));
            } else {
                out.push('-');
            }
        }
        "json_int" | "json_int_top" => {
            let json = parse_hex(args[0]);
            let key = parse_hex(args[1]);
            let value = if op == "json_int" {
                json_int(&json, &key)
            } else {
                json_int_top(&json, &key)
            };
            match value {
                Some(v) => {
                    let _ = write!(out, "{v}");
                }
                None => out.push('-'),
            }
        }
        "json_float1" | "json_float3" | "json_float1_top" | "json_float3_top" => {
            let json = parse_hex(args[0]);
            let key = parse_hex(args[1]);
            match op {
                "json_float1" => match json_float1(&json, &key) {
                    Some(v) => out.push_str(&bits32(v)),
                    None => out.push('-'),
                },
                "json_float3" => match json_float3(&json, &key) {
                    Some(v) => {
                        let _ = write!(out, "{},{},{}", bits32(v[0]), bits32(v[1]), bits32(v[2]));
                    }
                    None => out.push('-'),
                },
                "json_float1_top" => match json_float1_top(&json, &key) {
                    Some(v) => out.push_str(&bits32(v)),
                    None => out.push('-'),
                },
                _ => match json_float3_top(&json, &key) {
                    Some(v) => {
                        let _ = write!(out, "{},{},{}", bits32(v[0]), bits32(v[1]), bits32(v[2]));
                    }
                    None => out.push('-'),
                },
            }
        }
        "json_int_at" => {
            let bytes = parse_hex(args[0]);
            let start = arg_u32(args[1]) as usize;
            match json_int_at(&bytes, start) {
                Some(v) => {
                    let _ = write!(out, "{v}");
                }
                None => out.push('-'),
            }
        }
        "json_float1_at" => {
            let bytes = parse_hex(args[0]);
            let start = arg_u32(args[1]) as usize;
            match json_float1_at(&bytes, start) {
                Some(v) => out.push_str(&bits32(v)),
                None => out.push('-'),
            }
        }
        "json_float3_at" => {
            let bytes = parse_hex(args[0]);
            let start = arg_u32(args[1]) as usize;
            match json_float3_at(&bytes, start) {
                Some(v) => {
                    let _ = write!(out, "{},{},{}", bits32(v[0]), bits32(v[1]), bits32(v[2]));
                }
                None => out.push('-'),
            }
        }
        "json_num" => {
            let bytes = parse_hex(args[0]);
            let start = arg_u32(args[1]) as usize;
            match json_num(&bytes, start) {
                Some((end, value)) => {
                    let _ = write!(out, "{end}|{}", bits64(value));
                }
                None => out.push_str("-|-"),
            }
        }
        "json_bal_end" => {
            let bytes = parse_hex(args[0]);
            let start = arg_u32(args[1]) as usize;
            match json_bal_end(&bytes, start) {
                Some(end) => {
                    let _ = write!(out, "{end}");
                }
                None => out.push_str("-1"),
            }
        }
        "json_value_end" => {
            let bytes = parse_hex(args[0]);
            let start = arg_u32(args[1]) as usize;
            let _ = write!(out, "{}", json_value_end(&bytes, start));
        }
        "json_string_at" => {
            let bytes = parse_hex(args[0]);
            let start = arg_u32(args[1]) as usize;
            let mut buf = [0u8; 64];
            let ok = json_string_at(&bytes, start, &mut buf);
            let _ = write!(out, "{}|", i32::from(ok));
            if ok {
                put_hex(&mut out, cstr(&buf));
            } else {
                out.push('-');
            }
        }
        "json_estr" => {
            let bytes = parse_hex(args[0]);
            let start = arg_u32(args[1]) as usize;
            let mut unit = [0u8; 4];
            let (used, written) = json_estr(&bytes, start, &mut unit);
            let _ = write!(out, "{used}|{written}|");
            if written == 0 {
                out.push('-');
            } else {
                put_hex(&mut out, &unit[..written as usize]);
            }
        }
        "kv_next" | "kv_next_small" => {
            let bytes = parse_hex(args[0]);
            let pos = arg_u32(args[1]) as usize;
            if op == "kv_next_small" {
                let mut ns = [0u8; 8];
                let mut vs = [0u8; 16];
                let mut nvs = [0u8; 4];
                match kv_next(&bytes, pos, &mut ns, &mut vs, &mut nvs) {
                    Some(value) => {
                        let _ = write!(
                            out,
                            "{}|{}|{}|{}|{}|",
                            value.next,
                            value.kind,
                            value.ival,
                            bits64(value.fval),
                            value.over
                        );
                        put_hex(&mut out, cstr(&ns));
                        out.push('|');
                        put_hex(&mut out, cstr(&vs));
                        out.push('|');
                        put_hex(&mut out, cstr(&nvs));
                    }
                    None => out.push_str("-|-|-|-|-|-|-|-"),
                }
            } else {
                let mut name = [0u8; 64];
                let mut sval = [0u8; 1200];
                let mut nval = [0u8; 64];
                match kv_next(&bytes, pos, &mut name, &mut sval, &mut nval) {
                    Some(value) => {
                        let _ = write!(
                            out,
                            "{}|{}|{}|{}|{}|",
                            value.next,
                            value.kind,
                            value.ival,
                            bits64(value.fval),
                            value.over
                        );
                        put_hex(&mut out, cstr(&name));
                        out.push('|');
                        put_hex(&mut out, cstr(&sval));
                        out.push('|');
                        put_hex(&mut out, cstr(&nval));
                    }
                    None => out.push_str("-|-|-|-|-|-|-|-"),
                }
            }
        }
        "batch_op_next" => {
            let bytes = parse_hex(args[0]);
            let pos = arg_u32(args[1]) as usize;
            let mut opbuf = [0u8; 64];
            let mut pbuf = [0u8; 64];
            let step = batch_op_next(&bytes, pos, &mut opbuf, &mut pbuf);
            let _ = write!(out, "{}|{}|", i32::from(step.copied), step.next);
            put_hex(&mut out, cstr(&opbuf));
            out.push('|');
            put_hex(&mut out, cstr(&pbuf));
        }
        "batch_has_create" => {
            let _ = write!(out, "{}", i32::from(batch_has_create(&parse_hex(args[0]))));
        }
        "batch_node_recorded" => {
            let ids = parse_ids(args[0]);
            let _ = write!(out, "{}", i32::from(batch_node_recorded(&ids, arg_i32(args[1]))));
        }
        "batch_record_decision" => {
            // The C generator declares `int ids[64]` and passes the case's own count and cap; the
            // array holds the parsed ids in front (the rest stays zero).
            let parsed = parse_ids(args[0]);
            let mut ids = vec![0i32; 64];
            ids[..parsed.len()].copy_from_slice(&parsed);
            let mut count = arg_u32(args[1]) as usize;
            let cap = arg_u32(args[2]) as usize;
            let id = arg_i32(args[3]);
            let result = batch_record_decision(&mut ids, &mut count, cap, id);
            let _ = write!(out, "{}|{}|{}", i32::from(result), count, inline_ids(&ids[..count]));
        }
        "sb_addjson" => {
            let mut buf = [0u8; 512];
            let mut builder = Sb::new(&mut buf);
            builder.addjson(&parse_hex(args[0]));
            let _ = write!(out, "{}|", i32::from(builder.ovf()));
            put_hex(&mut out, builder.bytes());
        }
        "sb_addf" => {
            let mut buf = [0u8; 512];
            let mut builder = Sb::new(&mut buf);
            builder.addf(arg_f32(args[0]));
            let _ = write!(out, "{}|", i32::from(builder.ovf()));
            put_hex(&mut out, builder.bytes());
        }
        "sb_addi" => {
            let mut buf = [0u8; 512];
            let mut builder = Sb::new(&mut buf);
            builder.addi(arg_i64(args[0]));
            let _ = write!(out, "{}|", i32::from(builder.ovf()));
            put_hex(&mut out, builder.bytes());
        }
        "sb_addnum" => {
            let mut buf = [0u8; 512];
            let mut builder = Sb::new(&mut buf);
            builder.addnum(args[0].parse().expect("u64 arg"));
            let _ = write!(out, "{}|", i32::from(builder.ovf()));
            put_hex(&mut out, builder.bytes());
        }
        "f_finite" => {
            let _ = write!(out, "{}", i32::from(f_finite(arg_f32(args[0]))));
        }
        "xform_origin_ok" => {
            let v = [arg_f32(args[0]), arg_f32(args[1]), arg_f32(args[2])];
            let _ = write!(out, "{}", i32::from(xform_origin_ok(&v)));
        }
        "xform_scales_ok" => {
            let v = [arg_f32(args[0]), arg_f32(args[1]), arg_f32(args[2])];
            let _ = write!(out, "{}", i32::from(xform_scales_ok(&v)));
        }
        "xform_any" => {
            let _ = write!(
                out,
                "{}",
                i32::from(xform_any(arg_i32(args[0]) != 0, arg_i32(args[1]) != 0, arg_i32(args[2]) != 0))
            );
        }
        "xform_origin_reason" => {
            let mut dst = [0u8; 96];
            xform_origin_reason(&mut dst);
            put_hex(&mut out, cstr(&dst));
        }
        "conn_text_ok" => {
            let _ = write!(
                out,
                "{}",
                i32::from(conn_text_ok(&parse_hex(args[0]), arg_i32(args[1]) != 0))
            );
        }
        "conn_array_parse" => {
            let mut specs = vec![ConnSpec::default(); CONN_MAX];
            match conn_array_parse(&parse_hex(args[0]), &mut specs, CONN_MAX) {
                None => out.push('-'),
                Some(count) => {
                    let _ = write!(out, "{count}");
                    for spec in &specs[..count] {
                        out.push('|');
                        put_hex(&mut out, spec.output());
                        out.push(',');
                        put_hex(&mut out, spec.target());
                        out.push(',');
                        put_hex(&mut out, spec.input());
                        out.push(',');
                        put_hex(&mut out, spec.param());
                        let _ = write!(out, ",{},{}", bits32(spec.delay), spec.times_to_fire);
                    }
                }
            }
        }
        "rm_mode_ok" => {
            let _ = write!(
                out,
                "{}",
                i32::from(rm_mode_ok(arg_i32(args[0]) != 0, arg_i32(args[1]) != 0))
            );
        }
        "rm_indices_parse" => {
            let mut v = vec![0i32; RM_INDEX_MAX];
            match rm_indices_parse(&parse_hex(args[0]), &mut v, RM_INDEX_MAX) {
                None => out.push('-'),
                Some(count) => {
                    let _ = write!(out, "{count}:");
                    for (i, index) in v[..count].iter().enumerate() {
                        if i != 0 {
                            out.push(',');
                        }
                        let _ = write!(out, "{index}");
                    }
                }
            }
        }
        "rm_sort_desc" => {
            let mut v = parse_ids(args[0]);
            rm_sort_desc(&mut v);
            out.push_str(&inline_ids(&v));
        }
        other => panic!("unknown corpus op {other}"),
    }
    out
}

/// The Rust port's output for a cases file.
fn rust_corpus(cases: &str) -> String {
    let mut out = String::new();
    for line in cases.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens: Vec<&str> = line.split(' ').collect();
        let _ = write!(out, "{}|", tokens[0]);
        let body = run_case(tokens[0], &tokens[1..]);
        // `run_case` starts with the op; keep only the args/results after it.
        let _ = writeln!(out, "{}", &body[tokens[0].len() + 1..]);
    }
    out
}

fn read_cases() -> String {
    let path = data_dir().join("entity_json_cases.txt");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("the corpus case list {} must be readable: {error}", path.display()))
}

#[test]
fn differential_corpus_matches_the_c() {
    let cases = read_cases();
    let committed_path = data_dir().join("entity_json_corpus.txt");
    let committed = std::fs::read_to_string(&committed_path)
        .unwrap_or_else(|error| panic!("the corpus {} must be committed: {error}", committed_path.display()));
    assert!(
        committed.lines().count() == cases.lines().filter(|l| !l.is_empty()).count(),
        "the corpus must carry one line per case ({} lines, {} cases)",
        committed.lines().count(),
        cases.lines().count()
    );

    // The C side: rebuild tests/data/entity_json_gen.c against the extracted region and run it.
    let include = write_region_header()
        .parent()
        .expect("the header has a parent")
        .to_path_buf();
    let generator = data_dir().join("entity_json_gen.c");
    assert!(generator.is_file(), "{} must be committed", generator.display());
    let binary = compile(&generator, "entity_json_gen", &include);
    let output = Command::new(&binary)
        .arg(data_dir().join("entity_json_cases.txt"))
        .output()
        .expect("the C generator runs");
    assert!(
        output.status.success(),
        "the C generator must run:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let c_out = String::from_utf8(output.stdout).expect("the C generator writes UTF-8");
    corpus_diff("c", &c_out, &committed);

    // The Rust side: the same cases through the port.
    let rust_out = rust_corpus(&cases);
    corpus_diff("rust", &rust_out, &committed);
}

/// Compares two corpus texts, naming the first differing line: the whole corpus is too big to
/// print in a failure.
fn corpus_diff(label: &str, got: &str, want: &str) {
    if got == want {
        return;
    }
    let mut detail = String::new();
    for (i, (got_line, want_line)) in got.lines().zip(want.lines()).enumerate() {
        if got_line != want_line {
            let _ = write!(
                detail,
                "line {i}:\n  {label}:    {got_line}\n  expected: {want_line}\n"
            );
            break;
        }
    }
    if detail.is_empty() {
        let _ = write!(
            detail,
            "{} lines vs {} lines",
            got.lines().count(),
            want.lines().count()
        );
    }
    panic!("the {label} corpus must equal the committed one:\n{detail}");
}

/// The extraction gate itself: `bridge/build.sh:64-74` (and the copied logic here) must refuse a
/// missing marker, a missing scanner and an injected hex literal - each FAILS naming what is
/// missing, never silently skipping.
#[test]
fn region_extraction_refuses_a_broken_region() {
    let text = std::fs::read_to_string(bridge_source()).expect("bridge/mcp_bridge.c is readable");
    assert!(
        extract_region_from(&text, "bridge/mcp_bridge.c").contains("static const char *kv_next("),
        "the real source must extract"
    );

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let cases: [(&str, String, &str); 4] = [
        ("missing end marker", text.replace(MARK_END, ""), MARK_END),
        ("duplicated begin marker", format!("{text}{MARK_BEGIN}"), MARK_BEGIN),
        (
            "missing scanner",
            text.replace("batch_op_next", "batch_op_step"),
            "batch_op_next",
        ),
        ("hex literal", text.replace(MARK_BEGIN, &format!("{MARK_BEGIN}\nstatic int injected = 0x10;\n")), "hex literal"),
    ];
    let mut failures = Vec::new();
    for (name, mutated, needle) in cases {
        let message = match std::panic::catch_unwind(|| extract_region_from(&mutated, name)) {
            Ok(_) => String::from("the extraction accepted a broken region"),
            Err(payload) => panic_message(payload),
        };
        failures.push((name, needle, message));
    }
    std::panic::set_hook(previous);
    for (name, needle, message) in failures {
        assert!(
            message.contains(needle),
            "`{name}` must fail naming `{needle}`, got: {message}"
        );
    }
}

/// The message of a caught panic payload.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    if let Some(text) = payload.downcast_ref::<&str>() {
        return (*text).to_string();
    }
    String::from("(no message)")
}

/// The negative half: one flipped byte of the committed corpus must break both comparisons.
#[test]
fn one_byte_corruption_is_detected() {
    let cases = read_cases();
    let committed = std::fs::read_to_string(data_dir().join("entity_json_corpus.txt"))
        .expect("the corpus is committed");
    let mut corrupted = committed.clone().into_bytes();
    let index = corrupted
        .iter()
        .position(|&byte| byte == b'f')
        .expect("the corpus carries an f");
    corrupted[index] = b'e';
    let corrupted = String::from_utf8(corrupted).expect("still UTF-8");
    assert_ne!(corrupted, committed, "the corruption must change the bytes");
    assert_ne!(
        rust_corpus(&cases),
        corrupted,
        "the Rust port must not match a corrupted corpus"
    );
}
