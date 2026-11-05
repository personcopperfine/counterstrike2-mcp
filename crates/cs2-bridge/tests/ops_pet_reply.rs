//! The `pet.*` reply builders and the loadability gates, held to the C's bytes.
//!
//! Every expectation below is written from the C's `sb_add` sequence in
//! `mcp_bridge_pet.inc` (frozen, m2.20) - the line numbers in each test name the block.
//! The builders take plain inputs, so this runs on the host with no game.

use cs2_bridge::kv3_check::vpcf_check;
use cs2_bridge::ops::pet::reply::{
    self, ActiveRead, ApplyRefusal, Buf, ClassSchema, DocHead, DocJson, DocOutcome, FileRead,
    SchemaField, SchemaRead, TypeArray, TypeRow,
};

/// A builder the size of a small reply.
fn buf() -> Buf {
    Buf::new(4096)
}

/// The bytes a builder wrote, as text.
fn text(b: &Buf) -> &str {
    core::str::from_utf8(b.bytes()).expect("the builders only write JSON text")
}

/// A root whose array `t` has `count` children with the given classes (`None` = unreadable).
fn arrays_with(t: usize, count: i32, classes: &[Option<&str>]) -> [TypeArray; reply::ARRAY_TYPES] {
    core::array::from_fn(|i| {
        if i != t {
            return TypeArray::Children {
                count: 0,
                classes: Vec::new(),
            };
        }
        TypeArray::Children {
            count,
            classes: classes
                .iter()
                .map(|c| c.map(std::string::ToString::to_string))
                .collect(),
        }
    })
}

/// `pet_unloadable` (`mcp_bridge_pet.inc:304-335`): `-32602 UnloadableFile` with the reason and
/// the path in its 512-byte `data`.
#[test]
fn unloadable_names_the_reason_and_the_path() {
    let error = reply::unloadable("read_failed", b"particles/x.vpcf");
    assert_eq!(error.code, -32602);
    assert_eq!(error.name, "UnloadableFile");
    assert_eq!(error.data, r#"{"reason":"read_failed","path":"particles/x.vpcf"}"#);

    // The same for a reason that came out of `vpcf_check`.
    let refusal = vpcf_check(b"{{{ not kv3").expect_err("a bare brace soup is not KV3");
    let error = reply::unloadable(refusal.reason.as_str(), b"particles/y.vpcf");
    assert!(
        error.data.starts_with(r#"{"reason":"#),
        "the reason is the gate's own string: {}",
        error.data
    );
}

/// `pet_unloadable`'s shortening loop (`:316-334`): a path that does not fit the 512-byte `data`
/// is cut from the left behind a `~`, never left as an unterminated JSON string.
#[test]
fn unloadable_shortens_a_long_path_from_the_left() {
    // `{"reason":"no_app_root","path":"` + path + `"}` = 34 + path.len(); the C's 512-byte
    // buffer holds 511 characters, so a 478-character path is one byte over.
    let path = "p".repeat(478);
    let error = reply::unloadable("no_app_root", path.as_bytes());
    let kept = format!("~{}", "p".repeat(476)); // two bytes dropped behind the marker
    assert_eq!(
        error.data,
        format!(r#"{{"reason":"no_app_root","path":"{kept}"}}"#)
    );
    assert_eq!(error.data.len(), 511, "the C's data[512] holds 511 characters");

    // One character shorter fits whole, so the marker never appears.
    let path = "p".repeat(477);
    let error = reply::unloadable("no_app_root", path.as_bytes());
    assert_eq!(error.data, format!(r#"{{"reason":"no_app_root","path":"{path}"}}"#));
}

/// `pet_log_refusal` (`:338-353`): one line, `pet: <what> refused <reason>: <path>`.
#[test]
fn log_refusal_is_the_c_line() {
    assert_eq!(
        reply::log_refusal("open", "no_addon", b"particles/x.vpcf"),
        "pet: open refused no_addon: particles/x.vpcf"
    );
    // The C's 1200-byte buffer cuts the tail instead of overflowing.
    let long = "p".repeat(2000);
    let line = reply::log_refusal("open", "read_failed", long.as_bytes());
    assert_eq!(line.len(), 1199, "the C's char msg[1200] holds 1199 characters");
    assert!(line.starts_with("pet: open refused read_failed: ppp"));
}

/// `pet_text_path` (`:825-832`).
#[test]
fn text_path_is_the_private_file_under_local_app_data() {
    assert_eq!(
        reply::text_path("C:\\users\\steamuser\\AppData\\Local").as_deref(),
        Some("C:\\users\\steamuser\\AppData\\Local\\cs2mcpbridge\\pet_text.vpcf")
    );
    // 600 bytes of buffer: a base that does not fit refuses rather than cutting the path.
    assert_eq!(reply::text_path(&"d".repeat(700)), None);
}

/// `op_pet_text`'s `save_to_file_failed` message (`:909-918`): the prefix, `": "`, then the
/// callee's text bounded by the C's `char msg[192]` - at most `192 - 21 - 1` of its bytes. The
/// reader passes exactly that remainder (`192 - len("save_to_file_failed: ")`) to `md_str`, so
/// the two caps agree.
#[test]
fn save_failure_message_is_bounded_by_the_c_msg() {
    assert_eq!(reply::save_failure_message(None), "save_to_file_failed");
    assert_eq!(
        reply::save_failure_message(Some("disk full")),
        "save_to_file_failed: disk full"
    );
    let msg = reply::save_failure_message(Some(&"e".repeat(400)));
    assert_eq!(msg.len(), 191, "the C's char msg[192] holds 191 characters");
    assert_eq!(msg.len(), "save_to_file_failed: ".len() + 170);
    assert!(msg.starts_with("save_to_file_failed: eee"));
}

/// `pet.load`'s two answers (`:428-430`, `:452-456`).
#[test]
fn load_reports_the_latch_and_the_module_state() {
    let mut b = buf();
    reply::load_already(&mut b, "verified");
    assert_eq!(
        text(&b),
        r#"{"loaded":true,"alreadyLoaded":true,"toolIndex":null,"module":"verified"}"#
    );

    let mut b = buf();
    reply::load_result(&mut b, false, 7, "not_loaded");
    assert_eq!(
        text(&b),
        r#"{"loaded":false,"alreadyLoaded":false,"toolIndex":7,"module":"not_loaded"}"#
    );
}

/// `pet_arrays_json` (`:187-212`): nine objects, `type` 0..8, classes in index order.
#[test]
fn arrays_are_the_nine_types() {
    let arrays = arrays_with(0, 2, &[Some("C_OP_A"), Some("C_OP_B")]);
    let mut b = buf();
    reply::arrays_json(&mut b, &arrays).expect("readable");
    // `sb_add(out, "[")`, then per type `{"type":t,"count":n,"classes":[...]}` separated by
    // commas (`:188-210`).
    let mut want = String::from("[{\"type\":0,\"count\":2,\"classes\":[\"C_OP_A\",\"C_OP_B\"]}");
    for t in 1..9 {
        want.push_str(&format!(",{{\"type\":{t},\"count\":0,\"classes\":[]}}"));
    }
    want.push(']');
    assert_eq!(text(&b), want);
}

/// `pet_arrays_json` fails closed (`:191-194`, `:199-204`): a bad count or an unreadable child
/// aborts the whole object with `-32017 PrerequisiteFailed`.
#[test]
fn arrays_fail_closed() {
    let mut b = buf();
    let mut arrays = arrays_with(0, 2, &[Some("C_OP_A"), Some("C_OP_B")]);
    arrays[3] = TypeArray::CountUnusable(4097);
    let error = reply::arrays_json(&mut b, &arrays).expect_err("4097 children is unusable");
    assert_eq!(error.code, -32017);
    assert_eq!(error.name, "PrerequisiteFailed");
    assert_eq!(error.data, r#"{"reason":"root_child_count_unusable"}"#);

    let mut b = buf();
    let arrays = arrays_with(0, 2, &[Some("C_OP_A"), None]);
    let error = reply::arrays_json(&mut b, &arrays).expect_err("an unreadable child aborts");
    assert_eq!(error.data, r#"{"reason":"root_child_unreadable"}"#);
}

/// `pet_doc_json` (`:217-246`): the header, `arrays`, `docType`.
#[test]
fn doc_json_writes_the_document_object() {
    let head = DocHead {
        path: b"c:\\content\\particles\\x.vpcf".to_vec(),
        dirty: true,
        is_new: false,
    };
    let doc = DocJson::Root(
        DocHead {
            path: head.path.clone(),
            dirty: head.dirty,
            is_new: head.is_new,
        },
        arrays_with(0, 1, &[Some("C_OP_A")]),
    );
    let mut b = buf();
    assert_eq!(reply::doc_json(&mut b, &doc).expect("readable"), DocOutcome::Written);
    let mut want = String::from(
        "{\"path\":\"c:\\\\content\\\\particles\\\\x.vpcf\",\"dirty\":true,\"isNewFile\":false,\
         \"hasRoot\":true,\"arrays\":[{\"type\":0,\"count\":1,\"classes\":[\"C_OP_A\"]}",
    );
    for t in 1..9 {
        want.push_str(&format!(",{{\"type\":{t},\"count\":0,\"classes\":[]}}"));
    }
    want.push_str("],\"docType\":\"vpcf\"}");
    assert_eq!(text(&b), want);
}

/// `pet_doc_json`'s no-root and bad-root arms (`:224-237`): the first writes `arrays:null`, the
/// second writes nothing and reports `bad_root`.
#[test]
fn doc_json_no_root_and_bad_root() {
    let head = || DocHead {
        path: Vec::new(),
        dirty: false,
        is_new: true,
    };
    let mut b = buf();
    let doc = DocJson::NoRoot(head());
    assert_eq!(reply::doc_json(&mut b, &doc).expect("no root is a result"), DocOutcome::Written);
    assert_eq!(
        text(&b),
        r#"{"path":"","dirty":false,"isNewFile":true,"hasRoot":false,"arrays":null,"docType":"vpcf"}"#
    );

    let mut b = buf();
    let doc = DocJson::BadRoot(head());
    assert_eq!(reply::doc_json(&mut b, &doc).expect("bad root is a result"), DocOutcome::Refused);
    assert_eq!(text(&b), "", "the C rolls the whole object back (`:235-236`)");
    assert_eq!(doc.refuse_reason(), Some("bad_root"));

    let mut b = buf();
    assert_eq!(
        reply::doc_json(&mut b, &DocJson::NoDoc).expect("no doc"),
        DocOutcome::Refused
    );
    assert_eq!(text(&b), "");
    assert_eq!(DocJson::NoDoc.refuse_reason(), Some("no_document"));
}

/// `pet.active`'s two shapes (`:1133-1153`).
#[test]
fn active_is_a_result_not_an_error() {
    let mut b = buf();
    reply::active_json(&mut b, &ActiveRead::None("no_session")).expect("a result");
    assert_eq!(text(&b), r#"{"doc":null,"reason":"no_session"}"#);

    let mut b = buf();
    let doc = DocJson::BadRoot(DocHead {
        path: b"p".to_vec(),
        dirty: false,
        is_new: false,
    });
    reply::active_json(&mut b, &ActiveRead::Doc(&doc)).expect("a result");
    assert_eq!(text(&b), r#"{"doc":null,"reason":"bad_root"}"#);

    let mut b = buf();
    let doc = DocJson::NoRoot(DocHead {
        path: b"p".to_vec(),
        dirty: false,
        is_new: false,
    });
    reply::active_json(&mut b, &ActiveRead::Doc(&doc)).expect("a result");
    assert_eq!(
        text(&b),
        r#"{"doc":{"path":"p","dirty":false,"isNewFile":false,"hasRoot":false,"arrays":null,"docType":"vpcf"}}"#
    );
}

/// `pet_live_tail` (`:877-893`): flags, then the arrays, with `arrays:null` + `arraysError`
/// when a child is unreadable.
#[test]
fn live_tail_never_fails_the_call() {
    let mut b = buf();
    b.add(b"{\"applied\":true");
    reply::live_tail_json(&mut b, true, false, true, &arrays_with(0, 0, &[]));
    // The closing brace is the caller's (`apply_json` / `undo_redo_json`), not the tail's.
    b.add(b"}");
    assert_eq!(
        text(&b),
        "{\"applied\":true,\"dirty\":true,\"canUndo\":false,\"canRedo\":true,\"arrays\":[\
         {\"type\":0,\"count\":0,\"classes\":[]},{\"type\":1,\"count\":0,\"classes\":[]},\
         {\"type\":2,\"count\":0,\"classes\":[]},{\"type\":3,\"count\":0,\"classes\":[]},\
         {\"type\":4,\"count\":0,\"classes\":[]},{\"type\":5,\"count\":0,\"classes\":[]},\
         {\"type\":6,\"count\":0,\"classes\":[]},{\"type\":7,\"count\":0,\"classes\":[]},\
         {\"type\":8,\"count\":0,\"classes\":[]}]}"
    );

    let mut b = buf();
    b.add(b"{\"applied\":true");
    reply::live_tail_json(&mut b, false, false, false, &arrays_with(2, 1, &[None]));
    let got = text(&b);
    assert!(
        got.ends_with(",\"arrays\":null,\"arraysError\":{\"reason\":\"root_child_unreadable\"}"),
        "the C rolls only the array list back (`:887-891`): {got}"
    );
}

/// `pet_type_row` (`:539-581`) and `pet_function_types`' object (`:617-632`).
#[test]
fn function_types_rows_are_byte_exact() {
    let row = TypeRow::Row {
        name: String::from("Color"),
        class_name: String::from("CParticleFunctionRenderer"),
        metadata: vec![
            String::from("MObsoleteParticleFunction"),
            String::from("MParticleRenderer"),
        ],
    };
    let mut b = buf();
    reply::type_row_json(&mut b, &row, 0, 1, true).expect("readable");
    assert_eq!(
        text(&b),
        r#"{"type":0,"index":1,"name":"Color","className":"CParticleFunctionRenderer","metadata":["MObsoleteParticleFunction","MParticleRenderer"],"obsolete":true,"singleton":null}"#
    );

    let mut b = buf();
    reply::type_row_json(&mut b, &row, 0, 1, false).expect("readable");
    assert!(
        text(&b).starts_with(','),
        "`if (!first) sb_add(out, \",\")` (`:558`)"
    );

    let mut b = buf();
    let error = reply::type_row_json(&mut b, &TypeRow::Unreadable, 0, 0, true)
        .expect_err("an unreadable definition aborts the reply");
    assert_eq!(error.code, -32017);
    assert_eq!(error.data, r#"{"reason":"function_definition_unreadable"}"#);

    // One type with two rows, the rest empty.
    let rows: [Vec<TypeRow>; reply::FUNCTION_TYPES] = core::array::from_fn(|t| {
        if t != 0 {
            return Vec::new();
        }
        vec![
            TypeRow::Row {
                name: String::from("A"),
                class_name: String::from("CA"),
                metadata: Vec::new(),
            },
            TypeRow::Row {
                name: String::from("B"),
                class_name: String::from("CB"),
                metadata: Vec::new(),
            },
        ]
    });
    let mut counts = [0i32; reply::FUNCTION_TYPES];
    counts[0] = 2;
    let mut b = buf();
    reply::function_types_json(&mut b, &counts, &rows).expect("readable");
    assert_eq!(
        text(&b),
        r#"{"count":2,"types":[{"type":0,"index":0,"name":"A","className":"CA","metadata":[],"obsolete":false,"singleton":null},{"type":0,"index":1,"name":"B","className":"CB","metadata":[],"obsolete":false,"singleton":null}]}"#
    );

    // A count outside 0..=4096 is refused before anything is written (`:609-616`).
    let mut b = buf();
    counts[3] = -1;
    let error = reply::function_types_json(&mut b, &counts, &rows).expect_err("bad count");
    assert_eq!(error.data, r#"{"reason":"function_count_unusable"}"#);
    assert_eq!(text(&b), "", "the C checks every count before the first write");
}

/// `pet.schema` (`:759-778`): the found:false object, and a found class with bases, fields and
/// metadata.
#[test]
fn schema_objects_are_byte_exact() {
    let mut b = buf();
    reply::schema_json(&mut b, "C_OP_Unknown", &SchemaRead::NotFound).expect("a result");
    assert_eq!(
        text(&b),
        r#"{"class":"C_OP_Unknown","found":false,"size":null,"project":null,"bases":[],"fields":[],"metadata":[]}"#
    );

    let found = ClassSchema {
        size: 424,
        project: Some(String::from("particles")),
        bases: vec![
            String::from("CParticleFunctionOperator"),
            String::from("CParticleFunction"),
        ],
        fields: vec![
            SchemaField {
                name: String::from("m_flRadius"),
                type_name: Some(String::from("CUtlFloat")),
                declared_in: String::from("C_OP_Foo"),
            },
            SchemaField {
                name: String::from("m_bUnread"),
                type_name: None,
                declared_in: String::from("CParticleFunction"),
            },
        ],
        metadata: vec![String::from("MPropertyFriendlyName")],
    };
    let mut b = buf();
    reply::schema_json(&mut b, "C_OP_Foo", &SchemaRead::Found(found)).expect("readable");
    assert_eq!(
        text(&b),
        r#"{"class":"C_OP_Foo","found":true,"size":424,"project":"particles","bases":["CParticleFunctionOperator","CParticleFunction"],"fields":[{"name":"m_flRadius","type":"CUtlFloat","declaredIn":"C_OP_Foo"},{"name":"m_bUnread","type":null,"declaredIn":"CParticleFunction"}],"metadata":["MPropertyFriendlyName"]}"#
    );

    let mut b = buf();
    let error = reply::schema_json(&mut b, "C_OP_Foo", &SchemaRead::Unreadable)
        .expect_err("an unreadable hop is SchemaUnavailable");
    assert_eq!(error.code, -32023);
    assert_eq!(error.name, "SchemaUnavailable");
    assert_eq!(
        error.data,
        r#"{"reason":"particles.dll schema walk hit an unreadable hop"}"#
    );
}

/// `pet.text` (`:939-952`), `pet.apply` (`:1088-1093`), `pet.undo`/`pet.redo` (`:1105-1118`),
/// `pet.open` (`:521-533`), `pet.reload` (`:1206-1210`) and `pet.save` (`:1240-1243`).
#[test]
fn the_remaining_objects_are_byte_exact() {
    let mut b = buf();
    reply::text_json(
        &mut b,
        &reply::TextReply {
            text: b"<!-- kv3 encoding:text:format:vpcf -->\n{}",
            bytes: 37,
            path: b"C:\\x\\a.vpcf",
            dirty: true,
            is_new: false,
            can_undo: true,
            can_redo: false,
        },
    )
    .expect("fits");
    assert_eq!(
        text(&b),
        r#"{"text":"<!-- kv3 encoding:text:format:vpcf -->\n{}","bytes":37,"path":"C:\\x\\a.vpcf","dirty":true,"isNewFile":false,"canUndo":true,"canRedo":false}"#
    );

    // A text member that overflows the reply buffer refuses the whole reply (`:941-944`).
    let mut small = Buf::new(64);
    let error = reply::text_json(
        &mut small,
        &reply::TextReply {
            text: &[b'x'; 200],
            bytes: 200,
            path: b"",
            dirty: false,
            is_new: false,
            can_undo: false,
            can_redo: false,
        },
    )
    .expect_err("a cut string is never answered");
    assert_eq!(error.data, r#"{"reason":"text_too_large"}"#);

    let mut b = buf();
    reply::undo_redo_json(&mut b, false, Some("nothing_to_undo"), false, false, true, &arrays_with(0, 0, &[]));
    let got = text(&b);
    assert!(
        got.starts_with(r#"{"done":false,"reason":"nothing_to_undo","dirty":false,"canUndo":false,"canRedo":true,"arrays":["#),
        "{got}"
    );
    assert!(got.ends_with('}'), "{got}");

    let mut b = buf();
    reply::reload_json(&mut b, true, false, b"C:\\x\\a.vpcf", true);
    assert_eq!(
        text(&b),
        r#"{"reloaded":true,"hasRoot":false,"path":"C:\\x\\a.vpcf","discardedChanges":true}"#
    );

    let mut b = buf();
    reply::save_json(&mut b, false, false, b"C:\\x\\a.vpcf");
    assert_eq!(
        text(&b),
        r#"{"saved":false,"skippedClean":false,"path":"C:\\x\\a.vpcf"}"#
    );

    let mut b = buf();
    reply::open_json(&mut b, false, b"particles/x.vpcf", None).expect("a result");
    assert_eq!(text(&b), r#"{"opened":false,"path":"particles/x.vpcf","active":null}"#);

    let mut b = buf();
    reply::open_json(
        &mut b,
        true,
        b"particles/x.vpcf",
        Some(&DocJson::BadRoot(DocHead {
            path: b"p".to_vec(),
            dirty: true,
            is_new: false,
        })),
    )
    .expect("a result");
    assert_eq!(
        text(&b),
        r#"{"opened":true,"path":"particles/x.vpcf","active":null}"#,
        "a bad root is `active:null`, not an error (`:527-530`)"
    );
}

/// `pet_open_gate` (`:378-414`) from plain inputs: which reason, and which path the log line
/// names. The two path-shape refusals report the caller's addon-relative path; everything after
/// the host-path derivation reports the host file.
#[test]
fn open_gate_refusals_from_string_inputs() {
    let host = "C:\\game\\content\\csgo_addons\\a\\particles\\x.vpcf";
    let ok = b"<!-- kv3 encoding:text:format:vpcf -->\n{ _class = \"CParticleSystemDefinition\" }";

    // no_app_root / no_addon: the caller's path (`:384-393`).
    let refused = reply::open_gate(b"particles/x.vpcf", None, Some("a"), None, None)
        .expect("an app root is required");
    assert_eq!(refused.error.data, r#"{"reason":"no_app_root","path":"particles/x.vpcf"}"#);
    assert_eq!(refused.log, "pet: open refused no_app_root: particles/x.vpcf");

    let refused = reply::open_gate(b"particles/x.vpcf", Some("C:\\game"), None, None, None)
        .expect("an addon is required");
    assert_eq!(refused.error.data, r#"{"reason":"no_addon","path":"particles/x.vpcf"}"#);

    // A host path that could not be derived is a failed read (`:394-400`).
    let refused = reply::open_gate(b"particles/x.vpcf", Some("C:\\game"), Some("a"), None, None)
        .expect("no host path");
    assert_eq!(refused.error.data, r#"{"reason":"read_failed","path":"particles/x.vpcf"}"#);

    // A read that refused (`:401-405`) and a file the KV3 gate refused (`:406-412`) both name
    // the host path.
    let refused = reply::open_gate(
        b"particles/x.vpcf",
        Some("C:\\game"),
        Some("a"),
        Some(host.as_bytes()),
        Some(&FileRead::Refused("too_large")),
    )
    .expect("too large");
    assert_eq!(
        refused.error.data,
        format!(r#"{{"reason":"too_large","path":"{}"}}"#, host.replace('\\', "\\\\"))
    );
    assert_eq!(refused.log, format!("pet: open refused too_large: {host}"));

    let bad = b"{{{ not kv3";
    let refused = reply::open_gate(
        b"particles/x.vpcf",
        Some("C:\\game"),
        Some("a"),
        Some(host.as_bytes()),
        Some(&FileRead::Bytes(bad)),
    )
    .expect("not KV3");
    assert!(
        refused.log.starts_with("pet: open refused "),
        "the reason is the gate's own: {}",
        refused.log
    );
    assert!(refused.log.ends_with(host), "{}", refused.log);

    // A loadable file passes.
    assert!(reply::open_gate(
        b"particles/x.vpcf",
        Some("C:\\game"),
        Some("a"),
        Some(host.as_bytes()),
        Some(&FileRead::Bytes(ok)),
    )
    .is_none());
}

/// The JSON string body for a document, escaped the way `serde_json` writes it: the escapes
/// `json_text.h` accepts (`\"`, `\\`, `\n`).
fn json_body(doc: &str) -> String {
    doc.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// `pet.apply`'s parameter half (`:958-1021`): the unescaped text, the label default and the
/// label rules.
#[test]
fn apply_params_unescape_and_check_the_label() {
    let doc = "<!-- kv3 encoding:text:format:vpcf -->\n{ _class = \"CParticleSystemDefinition\" }";
    let params = format!(r#"{{"text":"{}"}}"#, json_body(doc));
    let parsed = match reply::apply_params(Some(&params)) {
        Ok(parsed) => parsed,
        Err(_) => panic!("a valid document must parse"),
    };
    assert_eq!(parsed.label, "MCP apply", "the C's default label (`:1011-1013`)");
    assert_eq!(core::str::from_utf8(&parsed.text).expect("ascii"), doc);

    let params = format!(r#"{{"text":"{}","label":"my step"}}"#, json_body(doc));
    let parsed = reply::apply_params(Some(&params)).ok().expect("a valid label");
    assert_eq!(parsed.label, "my step");

    for label in ["", &"L".repeat(65)] {
        let params = format!(r#"{{"text":"{}","label":"{label}"}}"#, json_body(doc));
        match reply::apply_params(Some(&params)) {
            Err(ApplyRefusal::Invalid(error)) => {
                assert_eq!(error.code, -32602);
                assert_eq!(
                    error.data,
                    r#"{"reason":"label: 1..64 printable ASCII (0x20..0x7E), no quote or backslash"}"#
                );
            }
            _ => panic!("label {label:?} must be refused"),
        }
    }

    // A raw DEL byte is outside 0x20..0x7e (`:1002-1009`).
    let params = format!("{{\"text\":\"{}\",\"label\":\"a\u{7f}b\"}}", json_body(doc));
    assert!(matches!(
        reply::apply_params(Some(&params)),
        Err(ApplyRefusal::Invalid(_))
    ));

    // The KV3 gate: a document pet cannot parse is UnloadableFile naming `pet.apply:text`.
    let params = r#"{"text":"{{{ not kv3"}"#;
    match reply::apply_params(Some(params)) {
        Err(ApplyRefusal::Unloadable { error, log }) => {
            assert_eq!(error.code, -32602);
            assert_eq!(error.name, "UnloadableFile");
            assert_eq!(
                error.data,
                r#"{"reason":"no_kv3_header","path":"pet.apply:text"}"#
            );
            assert_eq!(log, "pet: apply refused no_kv3_header: pet.apply:text");
        }
        _ => panic!("a malformed document must be refused before the document is touched"),
    }
}
